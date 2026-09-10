// Client graphic-glyph geometry.
//
// Block elements (U+2580..U+2593) and box-drawing (U+2500..U+257F) are painted
// as solid rectangles instead of font text. Font glyphs paint slightly smaller
// than their cell, which leaves ~1px horizontal / ~2px vertical background
// seams between adjacent block and border cells. Geometry covers the cell
// exactly (plus a small epsilon bleed) so grids and borders tile seamlessly.

use crate::ffi::{self, JsHandle};
use crate::measure::css_color;

/// Overdraw amount (device px) for graphic cells, hiding anti-aliasing seams.
pub(crate) const GRAPHIC_EPS: f64 = 0.7;

/// Horizontal half of a box-drawing glyph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum BarSide {
    None,
    Left,
    Right,
    Full,
}

/// Vertical half of a box-drawing glyph.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum StemSide {
    None,
    Up,
    Down,
    Full,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum LineWeight {
    Light,
    Heavy,
    Double,
}

/// Fractional rect + alpha for a block element, in cell units.
pub(crate) fn block_geometry(c: char) -> Option<(f64, f64, f64, f64, f64)> {
    let g = match c {
        '\u{2580}' => (0.0, 0.0, 1.0, 0.5, 1.0),   // ▀ upper half
        '\u{2584}' => (0.0, 0.5, 1.0, 1.0, 1.0),   // ▄ lower half
        '\u{2588}' => (0.0, 0.0, 1.0, 1.0, 1.0),   // █ full block
        '\u{258C}' => (0.0, 0.0, 0.5, 1.0, 1.0),   // ▌ left half
        '\u{2590}' => (0.5, 0.0, 1.0, 1.0, 1.0),   // ▐ right half
        '\u{2591}' => (0.0, 0.0, 1.0, 1.0, 0.25),  // ░ light shade
        '\u{2592}' => (0.0, 0.0, 1.0, 1.0, 0.5),   // ▒ medium shade
        '\u{2593}' => (0.0, 0.0, 1.0, 1.0, 0.75),  // ▓ dark shade
        _ => return None,
    };
    Some(g)
}

pub(crate) fn box_geometry(c: char) -> Option<(BarSide, StemSide, LineWeight)> {
    use BarSide as B;
    use LineWeight as W;
    use StemSide as S;
    let g = match c {
        '\u{2500}' => (B::Full, S::None, W::Light),   // ─
        '\u{2501}' => (B::Full, S::None, W::Heavy),   // ━
        '\u{2502}' => (B::None, S::Full, W::Light),   // │
        '\u{2503}' => (B::None, S::Full, W::Heavy),   // ┃
        '\u{250C}' => (B::Right, S::Up, W::Light),    // ┌
        '\u{250D}' => (B::Right, S::Up, W::Light),    // ┍
        '\u{250E}' => (B::Right, S::Up, W::Heavy),    // ┎
        '\u{250F}' => (B::Right, S::Up, W::Heavy),    // ┏
        '\u{2510}' => (B::Left, S::Up, W::Light),     // ┐
        '\u{2511}' => (B::Left, S::Up, W::Light),     // ┑
        '\u{2512}' => (B::Left, S::Up, W::Heavy),     // ┒
        '\u{2513}' => (B::Left, S::Up, W::Heavy),     // ┓
        '\u{2514}' => (B::Right, S::Down, W::Light),  // └
        '\u{2515}' => (B::Right, S::Down, W::Light),  // ┕
        '\u{2516}' => (B::Right, S::Down, W::Heavy),  // ┖
        '\u{2517}' => (B::Right, S::Down, W::Heavy),  // ┗
        '\u{2518}' => (B::Left, S::Down, W::Light),   // ┘
        '\u{2519}' => (B::Left, S::Down, W::Light),   // ┙
        '\u{251A}' => (B::Left, S::Down, W::Heavy),   // ┚
        '\u{251B}' => (B::Left, S::Down, W::Heavy),   // ┛
        '\u{251C}' => (B::Right, S::Full, W::Light),  // ├
        '\u{251D}' => (B::Right, S::Full, W::Light),  // ┝
        '\u{251E}' => (B::Right, S::Up, W::Light),    // ┞
        '\u{251F}' => (B::Right, S::Down, W::Light),  // ┟
        '\u{2520}' => (B::Right, S::Up, W::Light),    // ┠
        '\u{2521}' => (B::Right, S::Down, W::Light),  // ┡
        '\u{2522}' => (B::Right, S::Full, W::Light),  // ┢
        '\u{2523}' => (B::Right, S::Full, W::Heavy),  // ┣
        '\u{2524}' => (B::Left, S::Full, W::Light),   // ┤
        '\u{2525}' => (B::Left, S::Full, W::Light),   // ┥
        '\u{2526}' => (B::Left, S::Up, W::Light),     // ┦
        '\u{2527}' => (B::Left, S::Down, W::Light),   // ┧
        '\u{2528}' => (B::Left, S::Up, W::Light),     // ┨
        '\u{2529}' => (B::Left, S::Down, W::Light),   // ┩
        '\u{252A}' => (B::Left, S::Full, W::Light),   // ┪
        '\u{252B}' => (B::Left, S::Full, W::Heavy),   // ┫
        '\u{252C}' => (B::Full, S::Up, W::Light),     // ┬
        '\u{252D}' => (B::Full, S::Up, W::Light),     // ┭
        '\u{252E}' => (B::Full, S::Up, W::Light),     // ┮
        '\u{252F}' => (B::Full, S::Up, W::Light),     // ┯
        '\u{2530}' => (B::Full, S::Up, W::Light),     // ┰
        '\u{2531}' => (B::Full, S::Up, W::Light),     // ┱
        '\u{2532}' => (B::Full, S::Up, W::Light),     // ┲
        '\u{2533}' => (B::Full, S::Up, W::Heavy),     // ┳
        '\u{2534}' => (B::Full, S::Down, W::Light),   // ┴
        '\u{2535}' => (B::Full, S::Down, W::Light),   // ┵
        '\u{2536}' => (B::Full, S::Down, W::Light),   // ┶
        '\u{2537}' => (B::Full, S::Down, W::Light),   // ┷
        '\u{2538}' => (B::Full, S::Down, W::Light),   // ┸
        '\u{2539}' => (B::Full, S::Down, W::Light),   // ┹
        '\u{253A}' => (B::Full, S::Down, W::Light),   // ┺
        '\u{253B}' => (B::Full, S::Down, W::Heavy),   // ┻
        '\u{253C}' => (B::Full, S::Full, W::Light),   // ┼
        '\u{253D}' => (B::Full, S::Full, W::Light),   // ┽
        '\u{253E}' => (B::Full, S::Full, W::Light),   // ┾
        '\u{253F}' => (B::Full, S::Full, W::Light),   // ┿
        '\u{2540}' => (B::Full, S::Full, W::Light),   // ╀
        '\u{2541}' => (B::Full, S::Full, W::Light),   // ╁
        '\u{2542}' => (B::Full, S::Full, W::Light),   // ╂
        '\u{2543}' => (B::Right, S::Full, W::Light),  // ╃
        '\u{2544}' => (B::Left, S::Full, W::Light),   // ╄
        '\u{2545}' => (B::Full, S::Up, W::Light),     // ╅
        '\u{2546}' => (B::Full, S::Down, W::Light),   // ╆
        '\u{2547}' => (B::Right, S::Full, W::Light),  // ╇
        '\u{2548}' => (B::Left, S::Full, W::Light),   // ╈
        '\u{2549}' => (B::Full, S::Up, W::Light),     // ╉
        '\u{254A}' => (B::Full, S::Down, W::Light),   // ╊
        '\u{254B}' => (B::Full, S::Full, W::Heavy),   // ╋
        '\u{2550}' => (B::Full, S::None, W::Double),  // ═
        '\u{2551}' => (B::None, S::Full, W::Double),  // ║
        '\u{2554}' => (B::Right, S::Up, W::Double),   // ╔
        '\u{2557}' => (B::Left, S::Up, W::Double),    // ╗
        '\u{255A}' => (B::Right, S::Down, W::Double), // ╚
        '\u{255D}' => (B::Left, S::Down, W::Double),  // ╝
        '\u{2560}' => (B::Right, S::Full, W::Double), // ╠
        '\u{2563}' => (B::Left, S::Full, W::Double),  // ╣
        '\u{2566}' => (B::Full, S::Up, W::Double),    // ╦
        '\u{2569}' => (B::Full, S::Down, W::Double),  // ╩
        '\u{256C}' => (B::Full, S::Full, W::Double),  // ╬
        '\u{256D}' => (B::Right, S::Up, W::Light),    // ╭ rounded
        '\u{256E}' => (B::Left, S::Up, W::Light),     // ╮ rounded
        '\u{256F}' => (B::Left, S::Down, W::Light),   // ╯ rounded
        '\u{2570}' => (B::Right, S::Down, W::Light),  // ╰ rounded
        '\u{2504}' | '\u{2508}' | '\u{254C}' | '\u{254D}' => (B::Full, S::None, W::Light),
        '\u{2505}' | '\u{2506}' | '\u{2507}' | '\u{2509}' | '\u{250A}' | '\u{250B}' | '\u{254E}'
        | '\u{254F}' => (B::None, S::Full, W::Light),
        _ => return None,
    };
    Some(g)
}

/// Paint a graphic glyph (block element or box-drawing) as geometry covering
/// its cell. Returns `true` when handled (caller skips the font path).
pub(crate) fn draw_graphic_cell(
    ctx: JsHandle,
    col: u16,
    row: u16,
    cw: f64,
    ch: f64,
    glyph: &str,
    color: u32,
) -> bool {
    let Some(c) = glyph.chars().next() else {
        return false;
    };
    if let Some((fx0, fy0, fx1, fy1, alpha)) = block_geometry(c) {
        let x = col as f64 * cw + fx0 * cw - GRAPHIC_EPS;
        let y = row as f64 * ch + fy0 * ch - GRAPHIC_EPS;
        let w = (fx1 - fx0) * cw + GRAPHIC_EPS * 2.0;
        let h = (fy1 - fy0) * ch + GRAPHIC_EPS * 2.0;
        if alpha < 1.0 {
            ffi::ctx_set_global_alpha(ctx, alpha);
        }
        ffi::ctx_set_fill_style(ctx, &css_color(color));
        ffi::ctx_fill_rect(ctx, x, y, w, h);
        if alpha < 1.0 {
            ffi::ctx_set_global_alpha(ctx, 1.0);
        }
        return true;
    }
    let Some((bar, stem, weight)) = box_geometry(c) else {
        return false;
    };
    ffi::ctx_set_fill_style(ctx, &css_color(color));
    draw_box_lines(ctx, col, row, cw, ch, bar, stem, weight);
    true
}

pub(crate) fn box_line_width(weight: LineWeight) -> (f64, f64) {
    match weight {
        LineWeight::Light => (2.0, 0.0),
        LineWeight::Heavy => (3.0, 0.0),
        LineWeight::Double => (1.5, 3.0),
    }
}

fn draw_box_lines(
    ctx: JsHandle,
    col: u16,
    row: u16,
    cw: f64,
    ch: f64,
    bar: BarSide,
    stem: StemSide,
    weight: LineWeight,
) {
    let cx = col as f64 * cw + cw * 0.5;
    let cy = row as f64 * ch + ch * 0.5;
    let (t, gap) = box_line_width(weight);
    let offsets: &[f64] = if gap > 0.0 { &[-gap, gap] } else { &[0.0] };

    for &off in offsets {
        if stem != StemSide::None {
            let x = cx + off - t * 0.5;
            let (y, h) = match stem {
                StemSide::Full => (row as f64 * ch - GRAPHIC_EPS, ch + GRAPHIC_EPS * 2.0),
                StemSide::Up => (row as f64 * ch - GRAPHIC_EPS, cy - row as f64 * ch + t * 0.5 + GRAPHIC_EPS),
                StemSide::Down => (cy - t * 0.5 - GRAPHIC_EPS, (row as f64 + 1.0) * ch - (cy - t * 0.5 - GRAPHIC_EPS) + GRAPHIC_EPS),
                _ => unreachable!(),
            };
            ffi::ctx_fill_rect(ctx, x, y, t, h);
        }
        if bar != BarSide::None {
            let y = cy + off - t * 0.5;
            let (x, w) = match bar {
                BarSide::Full => (col as f64 * cw - GRAPHIC_EPS, cw + GRAPHIC_EPS * 2.0),
                BarSide::Left => (col as f64 * cw - GRAPHIC_EPS, cx - col as f64 * cw + t * 0.5 + GRAPHIC_EPS),
                BarSide::Right => (cx - t * 0.5 - GRAPHIC_EPS, (col as f64 + 1.0) * cw - (cx - t * 0.5 - GRAPHIC_EPS) + GRAPHIC_EPS),
                _ => unreachable!(),
            };
            ffi::ctx_fill_rect(ctx, x, y, w, t);
        }
    }
}
