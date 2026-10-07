// Client color utilities.
//
// Provides the xterm-256 palette lookup, vt100 color->RGB conversion, and the
// bold-bright color emulation shared by the renderers.

use vt100::Color;

/// Default foreground color (light gray)
pub(crate) const DEFAULT_FG: u32 = 0xf0f0f0;
/// Default background color (dark gray, matching the old krust theme)
pub(crate) const DEFAULT_BG: u32 = 0x2b2b2b;

/// Runtime palette overrides set by `OSC 4` / `OSC 10` / `OSC 11` sequences.
///
/// The terminal has exactly one of these, so it lives in a thread-local like
/// the rest of the single-threaded client state. `indexed[i] == None` means the
/// built-in xterm value is used.
struct Palette {
    indexed: [Option<u32>; 256],
    default_fg: u32,
    default_bg: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            indexed: [None; 256],
            default_fg: DEFAULT_FG,
            default_bg: DEFAULT_BG,
        }
    }
}

thread_local! {
    static PALETTE: std::cell::RefCell<Palette> = std::cell::RefCell::new(Palette::default());
}

/// The active color for an ANSI index, honouring any `OSC 4` override.
pub(crate) fn resolved_indexed(idx: u8) -> u32 {
    PALETTE.with(|p| {
        p.borrow()
            .indexed[idx as usize]
            .unwrap_or_else(|| xterm_palette(idx))
    })
}

/// The active default foreground color, honouring any `OSC 10` override.
pub(crate) fn default_fg() -> u32 {
    PALETTE.with(|p| p.borrow().default_fg)
}

/// The active default background color, honouring any `OSC 11` override.
pub(crate) fn default_bg() -> u32 {
    PALETTE.with(|p| p.borrow().default_bg)
}

/// Restore the built-in palette. Called on terminal reset so an app that
/// recolored the palette does not leak into the next session.
pub(crate) fn reset_palette() {
    PALETTE.with(|p| *p.borrow_mut() = Palette::default());
}

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
        Color::Idx(i) => resolved_indexed(i),
        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
    }
}

/// Parse an xterm color specification: `rgb:R/G/B`, `rgba:R/G/B/A`, or
/// `#RRGGBB` (also `#RGB`). Components may be 1-4 hex digits and are scaled to
/// 8 bits. Returns `None` for anything malformed.
pub(crate) fn parse_color_spec(spec: &[u8]) -> Option<u32> {
    let s = std::str::from_utf8(spec).ok()?.trim();
    if let Some(rest) = s.strip_prefix("rgb:").or_else(|| s.strip_prefix("rgba:")) {
        let mut parts = rest.split('/');
        let r = parse_hex_component(parts.next()?)?;
        let g = parse_hex_component(parts.next()?)?;
        let b = parse_hex_component(parts.next()?)?;
        return Some(((r as u32) << 16) | ((g as u32) << 8) | b as u32);
    }
    if let Some(hex) = s.strip_prefix('#') {
        if hex.len() == 6 {
            return u32::from_str_radix(hex, 16).ok();
        }
        if hex.len() == 3 {
            let r = u8::from_str_radix(&hex[0..1], 16).ok()?;
            let g = u8::from_str_radix(&hex[1..2], 16).ok()?;
            let b = u8::from_str_radix(&hex[2..3], 16).ok()?;
            return Some((((r * 17) as u32) << 16) | (((g * 17) as u32) << 8) | (b * 17) as u32);
        }
    }
    None
}

/// Parse one `rgb:` component (1-4 hex digits) to an 8-bit value.
fn parse_hex_component(s: &str) -> Option<u8> {
    if s.is_empty() || s.len() > 4 || !s.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let v = u32::from_str_radix(s, 16).ok()?;
    let max = (1u32 << (4 * s.len())) - 1;
    Some(((v * 255) / max) as u8)
}

/// Apply an `OSC 4` / `OSC 10` / `OSC 11` / `OSC 12` color-set sequence.
///
/// `params` is the OSC body split on `;`: `["4", idx, spec, ...]` or
/// `["10"|"11"|"12", spec]`. A `?` spec is a query and is ignored here (the
/// query responder answers it). Returns true when the palette changed.
pub(crate) fn apply_dynamic_color(params: &[&[u8]]) -> bool {
    let Some(kind) = params.first() else {
        return false;
    };
    match *kind {
        b"4" => {
            let mut changed = false;
            let mut i = 1;
            while i + 1 < params.len() {
                let idx = std::str::from_utf8(params[i])
                    .ok()
                    .and_then(|s| s.trim().parse::<u8>().ok());
                if let Some(idx) = idx {
                    if params[i + 1] != b"?" {
                        if let Some(rgb) = parse_color_spec(params[i + 1]) {
                            PALETTE.with(|p| p.borrow_mut().indexed[idx as usize] = Some(rgb));
                            changed = true;
                        }
                    }
                }
                i += 2;
            }
            changed
        }
        b"10" => set_default(params, true),
        b"11" => set_default(params, false),
        _ => false,
    }
}

/// Helper for `OSC 10`/`OSC 11`: apply `params[1]` to the default fg/bg.
fn set_default(params: &[&[u8]], fg: bool) -> bool {
    let Some(spec) = params.get(1) else {
        return false;
    };
    if *spec == b"?" {
        return false;
    }
    let Some(rgb) = parse_color_spec(spec) else {
        return false;
    };
    PALETTE.with(|p| {
        let mut p = p.borrow_mut();
        if fg {
            p.default_fg = rgb;
        } else {
            p.default_bg = rgb;
        }
    });
    true
}

/// Foreground RGB for a cell, emulating xterm.js's
/// `drawBoldTextInBrightColors` (default on): bold text whose color is one of
/// the 8 base ANSI colors renders as the matching bright variant. This is what
/// makes `ls` directories pop as bright blue instead of dark navy.
pub(crate) fn cell_fg_rgb(cell: &vt100::Cell, default: u32) -> u32 {
    if cell.bold() {
        if let Color::Idx(i) = cell.fgcolor() {
            if i < 8 {
                return resolved_indexed(i + 8);
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

    #[test]
    fn parse_color_spec_accepts_xterm_forms() {
        assert_eq!(parse_color_spec(b"rgb:2b/2b/2b"), Some(0x2b2b2b));
        assert_eq!(parse_color_spec(b"rgb:2b2b/2b2b/2b2b"), Some(0x2b2b2b));
        assert_eq!(parse_color_spec(b"rgb:f/0/0"), Some(0xff0000));
        assert_eq!(parse_color_spec(b"#ff8000"), Some(0xff8000));
        assert_eq!(parse_color_spec(b"#f80"), Some(0xff8800));
        assert_eq!(parse_color_spec(b"rgba:00/00/00/ff"), Some(0x000000));
        assert_eq!(parse_color_spec(b"bogus"), None);
    }

    #[test]
    fn osc4_overrides_an_indexed_color() {
        reset_palette();
        assert_eq!(resolved_indexed(1), 0xCD0000);
        assert!(apply_dynamic_color(&[b"4", b"1", b"#123456"]));
        assert_eq!(resolved_indexed(1), 0x123456);
        reset_palette();
        assert_eq!(resolved_indexed(1), 0xCD0000);
    }

    #[test]
    fn osc4_query_does_not_change_the_palette() {
        reset_palette();
        assert!(!apply_dynamic_color(&[b"4", b"1", b"?"]));
        assert_eq!(resolved_indexed(1), 0xCD0000);
    }

    #[test]
    fn osc10_and_11_set_defaults() {
        reset_palette();
        assert!(apply_dynamic_color(&[b"10", b"#010203"]));
        assert!(apply_dynamic_color(&[b"11", b"#040506"]));
        assert_eq!(default_fg(), 0x010203);
        assert_eq!(default_bg(), 0x040506);
        assert!(!apply_dynamic_color(&[b"11", b"?"]));
        reset_palette();
        assert_eq!(default_fg(), DEFAULT_FG);
        assert_eq!(default_bg(), DEFAULT_BG);
    }
}
