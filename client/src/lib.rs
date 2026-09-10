// client terminal module
// WASM terminal integration Phase 0-3
//
// Provides the Rust/WASM terminal:
// - VT100 parser for ANSI escape sequences
// - Canvas 2D glyph renderer (sharp vector text via native text rasterization)
// - WebSocket binary message pipeline from the server
// - Selection overlay support
// - Keyboard input pipeline
// - Resize handling
// - Error boundaries & panic handling

#![allow(missing_docs)]

mod color;
mod exports;
mod ffi;
mod graphics;
mod input;
mod measure;
mod query;
mod renderer;
mod selection;
mod state;

#[cfg(test)]
mod tests {
    use crate::color::{
        cell_fg_rgb, color_to_rgb, xterm_palette, DEFAULT_BG, DEFAULT_FG,
    };
    use crate::graphics::{block_geometry, box_geometry};
    use crate::input::{map_key, xterm_modifier_param};
    use crate::query::collect_query_replies;
    use crate::state::{DEFAULT_COLS, DEFAULT_ROWS, SCROLLBACK_LEN};
    use vt100::{Color, Parser};

    #[test]
    fn query_replies_da1() {
        assert_eq!(collect_query_replies(b"\x1b[c", 0, 0), b"\x1b[?1;2c");
        assert_eq!(collect_query_replies(b"\x1b[0c", 0, 0), b"\x1b[?1;2c");
    }

    #[test]
    fn query_replies_da2() {
        assert_eq!(collect_query_replies(b"\x1b[>c", 0, 0), b"\x1b[>0;1;0c");
    }

    #[test]
    fn query_replies_cursor_position() {
        assert_eq!(collect_query_replies(b"\x1b[6n", 2, 4), b"\x1b[3;5R");
    }

    #[test]
    fn query_replies_osc11() {
        assert_eq!(
            collect_query_replies(b"\x1b]11;?\x1b\\", 0, 0),
            b"\x1b]11;rgb:2b2b/2b2b/2b2b\x1b\\"
        );
    }

    #[test]
    fn query_replies_pass_through_plain_text() {
        assert_eq!(collect_query_replies(b"plain text\n", 0, 0), b"");
    }

    #[test]
    fn query_replies_multiple_in_one_chunk() {
        assert_eq!(
            collect_query_replies(b"\x1b[c\x1b[6n", 1, 1),
            b"\x1b[?1;2c\x1b[2;2R"
        );
    }

    #[test]
    fn parser_renders_hello_world() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"Hello World");
        let first_row = parser.screen().rows(0, DEFAULT_COLS).next().unwrap();
        assert_eq!(first_row, "Hello World");
    }

    #[test]
    fn parser_strips_ansi_escapes() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"\x1b[31mred\x1b[0m");
        let first_row = parser.screen().rows(0, DEFAULT_COLS).next().unwrap();
        assert_eq!(first_row, "red");
    }

    #[test]
    fn parser_tracks_cursor() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"ab");
        assert_eq!(parser.screen().cursor_position(), (0, 2));
    }

    #[test]
    fn parser_cells_map_to_hello_world() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"Hello World");
        let screen = parser.screen();
        let mut text = String::new();
        for col in 0..11 {
            let cell = screen.cell(0, col).unwrap();
            text.push(cell.contents().chars().next().unwrap());
        }
        assert_eq!(text, "Hello World");
    }

    #[test]
    fn xterm_256_palette_maps_known_colors() {
        assert_eq!(xterm_palette(0), 0x000000);
        assert_eq!(xterm_palette(1), 0xCD0000);
        assert_eq!(xterm_palette(4), 0x0000EE);
        assert_eq!(xterm_palette(9), 0xFF0000);
        assert_eq!(xterm_palette(12), 0x5C5CFF);
        assert_eq!(xterm_palette(15), 0xFFFFFF);
        assert_eq!(xterm_palette(16), 0x000000);
        assert_eq!(xterm_palette(196), 0xFF0000);
        assert_eq!(xterm_palette(255), 0xEEEEEE);
    }

    #[test]
    fn color_to_rgb_maps_default_and_idx() {
        assert_eq!(color_to_rgb(Color::Default, DEFAULT_FG), DEFAULT_FG);
        assert_eq!(color_to_rgb(Color::Default, DEFAULT_BG), DEFAULT_BG);
        assert_eq!(color_to_rgb(Color::Idx(1), DEFAULT_FG), 0xCD0000);
        assert_eq!(
            color_to_rgb(Color::Rgb(255, 0, 128), DEFAULT_FG),
            0xFF0080
        );
    }

    #[test]
    fn bold_base_colors_render_as_bright() {
        let mut p = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        p.process(b"\x1b[1;34mX");
        let cell = p.screen().cell(0, 0).unwrap();
        assert!(cell.bold());
        assert_eq!(cell.fgcolor(), Color::Idx(4));
        assert_eq!(cell_fg_rgb(&cell, DEFAULT_FG), xterm_palette(12));
        assert_eq!(cell_fg_rgb(&cell, DEFAULT_FG), 0x5C5CFF);
    }

    #[test]
    fn non_bold_uses_palette_color() {
        let mut p = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        p.process(b"\x1b[34mX");
        let cell = p.screen().cell(0, 0).unwrap();
        assert!(!cell.bold());
        assert_eq!(cell_fg_rgb(&cell, DEFAULT_FG), 0x0000EE);
    }

    #[test]
    fn block_geometry_covers_known_blocks() {
        use crate::graphics::{BarSide as B, LineWeight as W, StemSide as S};
        // Full block is opaque and covers the whole cell.
        let g = block_geometry('\u{2588}').unwrap();
        assert_eq!(g, (0.0, 0.0, 1.0, 1.0, 1.0));
        // Half blocks cover exactly half; shades use alpha.
        assert_eq!(block_geometry('\u{2580}').unwrap().3, 0.5);
        assert_eq!(block_geometry('\u{2592}').unwrap().4, 0.5);
        assert_eq!(block_geometry('\u{2584}').unwrap().1, 0.5);
        // Non-block chars return None.
        assert!(block_geometry('A').is_none());
        assert!(box_geometry('A').is_none());
        // Box-drawing corners encode the correct arms.
        assert_eq!(box_geometry('\u{250C}').unwrap(), (B::Right, S::Up, W::Light)); // ┌
        assert_eq!(box_geometry('\u{2510}').unwrap(), (B::Left, S::Up, W::Light));  // ┐
        assert_eq!(box_geometry('\u{2514}').unwrap(), (B::Right, S::Down, W::Light)); // └
        assert_eq!(box_geometry('\u{2518}').unwrap(), (B::Left, S::Down, W::Light)); // ┘
        assert_eq!(box_geometry('\u{2500}').unwrap(), (B::Full, S::None, W::Light)); // ─
        assert_eq!(box_geometry('\u{2502}').unwrap(), (B::None, S::Full, W::Light)); // │
        assert_eq!(box_geometry('\u{2501}').unwrap().2, W::Heavy);             // ━
        assert_eq!(box_geometry('\u{2550}').unwrap().2, W::Double);            // ═
        assert!(box_geometry('A').is_none());
    }

    #[test]
    fn map_key_ctrl_c_returns_control_c() {
        assert_eq!(map_key("c", true, false, false, false), vec![0x03]);
    }

    #[test]
    fn map_key_ctrl_bracket_returns_escape() {
        assert_eq!(map_key("[", true, false, false, false), vec![0x1b]);
    }

    #[test]
    fn map_key_shift_enter_returns_esc_cr() {
        assert_eq!(map_key("Enter", false, false, true, false), vec![0x1b, 0x0d]);
    }

    #[test]
    fn map_key_arrow_up_returns_escape_bracket_a() {
        assert_eq!(map_key("ArrowUp", false, false, false, false), vec![0x1b, b'[', b'A']);
        assert_eq!(map_key("ArrowDown", false, false, false, false), vec![0x1b, b'[', b'B']);
        assert_eq!(map_key("ArrowRight", false, false, false, false), vec![0x1b, b'[', b'C']);
        assert_eq!(map_key("ArrowLeft", false, false, false, false), vec![0x1b, b'[', b'D']);
    }

    #[test]
    fn map_key_f1_returns_escape_bracket_11_tilde() {
        assert_eq!(map_key("F1", false, false, false, false), b"\x1b[11~".to_vec());
        assert_eq!(map_key("F12", false, false, false, false), b"\x1b[24~".to_vec());
    }

    #[test]
    fn map_key_alt_x_returns_escape_x() {
        assert_eq!(map_key("x", false, true, false, false), vec![0x1b, b'x']);
    }

    #[test]
    fn map_key_plain_printable_passes_through() {
        assert_eq!(map_key("a", false, false, false, false), vec![b'a']);
        assert_eq!(map_key("5", false, false, false, false), vec![b'5']);
    }

    #[test]
    fn map_key_modifier_param_encoding() {
        assert_eq!(xterm_modifier_param(false, false, false), None);
        assert_eq!(xterm_modifier_param(false, false, true), Some(2));
        assert_eq!(xterm_modifier_param(false, true, false), Some(3));
        assert_eq!(xterm_modifier_param(true, false, false), Some(5));
        assert_eq!(xterm_modifier_param(false, true, true), Some(4));
        assert_eq!(xterm_modifier_param(true, false, true), Some(6));
        assert_eq!(xterm_modifier_param(true, true, false), Some(7));
    }

    #[test]
    fn map_key_unknown_returns_empty() {
        assert_eq!(map_key("CapsLock", false, false, false, false), Vec::<u8>::new());
    }

    #[test]
    fn normalize_maps_csi_save_restore() {
        use crate::state::normalize_save_restore;
        let mut carry = Vec::new();
        assert_eq!(normalize_save_restore(b"\x1b[s\x1b[u", &mut carry), b"\x1b7\x1b8");
        assert!(carry.is_empty());
    }

    #[test]
    fn normalize_leaves_other_sequences_untouched() {
        use crate::state::normalize_save_restore;
        let mut carry = Vec::new();
        assert_eq!(
            normalize_save_restore(b"\x1b[H\x1b[?1049h\x1b[?1049l\x1b[K", &mut carry),
            b"\x1b[H\x1b[?1049h\x1b[?1049l\x1b[K"
        );
    }

    #[test]
    fn normalize_carries_sequences_across_chunk_boundaries() {
        use crate::state::normalize_save_restore;
        let mut carry = Vec::new();
        assert_eq!(normalize_save_restore(b"pre\x1b[", &mut carry), b"pre");
        assert_eq!(carry, b"\x1b[");
        assert_eq!(normalize_save_restore(b"s mid", &mut carry), b"\x1b7 mid");
        assert!(carry.is_empty());
    }

    #[test]
    fn normalize_carries_lone_escape() {
        use crate::state::normalize_save_restore;
        let mut carry = Vec::new();
        assert_eq!(normalize_save_restore(b"x\x1b", &mut carry), b"x");
        assert_eq!(carry, b"\x1b");
        assert_eq!(normalize_save_restore(b"[u", &mut carry), b"\x1b8");
        assert!(carry.is_empty());
    }

    #[test]
    fn normalize_handles_complete_sequence_at_chunk_end() {
        use crate::state::normalize_save_restore;
        let mut carry = Vec::new();
        assert_eq!(normalize_save_restore(b"\x1b[s", &mut carry), b"\x1b7");
        assert!(carry.is_empty());
    }
}
