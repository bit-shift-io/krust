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
