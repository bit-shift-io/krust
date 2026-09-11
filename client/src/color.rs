// Client color utilities.
//
// Provides the xterm-256 palette lookup, vt100 color->RGB conversion, and the
// bold-bright color emulation shared by the renderers.

use vt100::Color;

/// Default foreground color (light gray)
pub(crate) const DEFAULT_FG: u32 = 0xf0f0f0;
/// Default background color (dark gray, matching the old krust theme)
pub(crate) const DEFAULT_BG: u32 = 0x2b2b2b;

/// Convert an ANSI/VT100 index (0-255) to its xterm-256 RGB value.
///
/// The 16-color block matches the xterm default theme so `ls` and other
/// colored programs render with the standard xterm palette.
pub(crate) fn xterm_palette(idx: u8) -> u32 {
    match idx {
        0..=15 => {
            const ANSI: [u32; 16] = [
                0x000000, 0xCD0000, 0x00CD00, 0xCDCD00, 0x0000EE, 0xCD00CD, 0x00CDCD, 0xE5E5E5,
                0x7F7F7F, 0xFF0000, 0x00FF00, 0xFFFF00, 0x5C5CFF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
            ];
            ANSI[idx as usize]
        }
        16..=231 => {
            let n = idx - 16;
            let (r, g, b) = (n / 36, (n / 6) % 6, n % 6);
            let step = |c: u8| if c == 0 { 0 } else { 55 + c * 40 };
            (step(r) as u32) << 16 | (step(g) as u32) << 8 | step(b) as u32
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            (v as u32) << 16 | (v as u32) << 8 | v as u32
        }
    }
}

/// Convert a vt100 color to its RGB value, using `default` for the default color.
pub(crate) fn color_to_rgb(color: Color, default: u32) -> u32 {
    match color {
        Color::Default => default,
        Color::Idx(i) => xterm_palette(i),
        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
    }
}

/// Foreground RGB for a cell, emulating xterm.js's
/// `drawBoldTextInBrightColors` (default on): bold text whose color is one of
/// the 8 base ANSI colors renders as the matching bright variant. This is what
/// makes `ls` directories pop as bright blue instead of dark navy.
pub(crate) fn cell_fg_rgb(cell: &vt100::Cell, default: u32) -> u32 {
    if cell.bold() {
        if let Color::Idx(i) = cell.fgcolor() {
            if i < 8 {
                return xterm_palette(i + 8);
            }
        }
    }
    color_to_rgb(cell.fgcolor(), default)
}

/// How a cell's cursor/selection state alters its fg/bg colors. Cursor takes
/// priority over selection (a block cursor masks the highlight).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CellOverride {
    Normal,
    Selected,
    Cursor,
}

/// Selection highlight text color (black over the original foreground).
pub(crate) const SELECTION_FG: u32 = 0x000000;

/// Resolved `(fg, bg)` colors for a cell after applying the cursor/selection
/// override. Shared by the Canvas 2D and WebGL2 renderers so both paint the
/// same colors for the same cell state:
///   Cursor:   fg = original bg, bg = original fg (block cursor)
///   Selected: fg = black,       bg = original fg
pub(crate) fn cell_visual(
    cell: Option<&vt100::Cell>,
    default_fg: u32,
    default_bg: u32,
    override_: CellOverride,
) -> (u32, u32) {
    let (fg0, bg0) = match cell {
        Some(c) => (
            cell_fg_rgb(c, default_fg),
            color_to_rgb(c.bgcolor(), default_bg),
        ),
        None => (default_fg, default_bg),
    };
    match override_ {
        CellOverride::Normal => (fg0, bg0),
        CellOverride::Selected => (SELECTION_FG, fg0),
        CellOverride::Cursor => (bg0, fg0),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vt100::Parser;

    fn cell() -> vt100::Cell {
        let mut p = Parser::new(1, 1, 0);
        p.process("\u{1b}[31;44mX".as_bytes());
        p.screen().cell(0, 0).unwrap().clone()
    }

    #[test]
    fn normal_uses_cell_colors() {
        let (fg, bg) = cell_visual(Some(&cell()), DEFAULT_FG, DEFAULT_BG, CellOverride::Normal);
        assert_eq!(fg, 0xCD0000);
        assert_eq!(bg, 0x0000EE);
    }

    #[test]
    fn selected_keeps_original_fg_as_highlight() {
        let (fg, bg) = cell_visual(
            Some(&cell()),
            DEFAULT_FG,
            DEFAULT_BG,
            CellOverride::Selected,
        );
        assert_eq!(fg, SELECTION_FG);
        assert_eq!(bg, 0xCD0000);
    }

    #[test]
    fn cursor_swaps_fg_and_bg() {
        let (fg, bg) = cell_visual(Some(&cell()), DEFAULT_FG, DEFAULT_BG, CellOverride::Cursor);
        assert_eq!(fg, 0x0000EE);
        assert_eq!(bg, 0xCD0000);
    }

    #[test]
    fn empty_cell_yields_defaults() {
        let (fg, bg) = cell_visual(None, DEFAULT_FG, DEFAULT_BG, CellOverride::Normal);
        assert_eq!(fg, DEFAULT_FG);
        assert_eq!(bg, DEFAULT_BG);
    }
}
