// Client cell-dimension measurement.
//
// Measures cell size from a scratch canvas so the real terminal canvas is never
// given a 2D context before the renderer choice is made (a canvas only supports
// one context type).

use crate::ffi::{self, JsHandle};

/// Epsilon to prevent 1-pixel anti-aliasing gaps between adjacent cell rects
pub(crate) const CELL_EPSILON: f64 = 0.5;

/// CSS font stack used by the canvas-2D glyph renderer (native vector text).
pub(crate) const FONT_STACK: &str =
    "14px/18px 'JetBrains Mono', 'Fira Code', Menlo, Consolas, monospace";
/// Bold variant of [`FONT_STACK`].
pub(crate) const FONT_STACK_BOLD: &str =
    "bold 14px/18px 'JetBrains Mono', 'Fira Code', Menlo, Consolas, monospace";

/// Glyph shapes the renderer actually draws. Cell height is measured from these
/// (see [`measure_cell_dimensions`]), so box-drawing/braille rows tile with no
/// hairline seams regardless of which font the user's system resolves to.
const MEASURE_PROBES: &[&str] = &["W", "0", "g", "j", "│", "─", "█", "▀", "░", "⠋", "⣿"];

/// Number of integer pixels that must be read back by `getImageData` when the
/// 2D pipeline is used for ink measurement.
const SCRATCH_SIZE: usize = 64;

/// Maximum ink height of `probes` after rasterizing each glyph onto a scratch
/// canvas. `None` if the 2D/`getImageData` pipeline is unavailable (the caller
/// then falls back to font metric boxes).
fn rasterized_glyph_height_max(probes: &[&str], _font_ctx: JsHandle) -> Option<f64> {
    let win = ffi::window();
    if win == 0 {
        return None;
    }
    let doc = ffi::window_document(win);
    if doc == 0 {
        return None;
    }
    let mut max_h = 0.0f64;
    let mut px = vec![0u8; SCRATCH_SIZE * SCRATCH_SIZE * 4];
    for ch in probes {
        let scratch = ffi::document_create_canvas(doc);
        if scratch == 0 {
            return None;
        }
        ffi::canvas_set_width(scratch, SCRATCH_SIZE as u32);
        ffi::canvas_set_height(scratch, SCRATCH_SIZE as u32);
        let ctx = ffi::canvas_get_2d(scratch);
        if ctx == 0 {
            ffi::release(scratch);
            return None;
        }
        ffi::ctx_set_font(ctx, FONT_STACK);
        ffi::ctx_set_fill_style(ctx, "#ffffff");
        ffi::ctx_set_text_baseline(ctx, "alphabetic");
        ffi::ctx_fill_text(ctx, ch, 8.0, 40.0);
        let written = ffi::ctx_get_image_data(ctx, 0.0, 0.0, SCRATCH_SIZE as f64, SCRATCH_SIZE as f64, &mut px);
        if written < px.len() {
            ffi::release(ctx);
            ffi::release(scratch);
            return None;
        }
        ffi::release(ctx);
        ffi::release(scratch);
        let mut top_row = SCRATCH_SIZE;
        let mut bottom_row = 0usize;
        for y in 0..SCRATCH_SIZE {
            let mut ink = false;
            for x in 8..24usize {
                let alpha = px[(y * SCRATCH_SIZE + x) * 4 + 3] as f64;
                if alpha > 0.0 {
                    ink = true;
                    break;
                }
            }
            if ink {
                top_row = top_row.min(y);
                bottom_row = y;
            }
        }
        if bottom_row >= top_row {
            let h = (bottom_row - top_row + 1) as f64;
            if h > max_h {
                max_h = h;
            }
        }
    }
    Some(max_h)
}

/// Measure a `"W"` advance and text-metric fallback height on an existing 2D
/// context. Returns `(width, height)`, where `height` is `None` when no ink
/// fallback could be computed (caller should then use the painted-glyph path).
fn measure_text_advance(ctx: JsHandle) -> (f64, Option<f64>) {
    ffi::ctx_set_font(ctx, FONT_STACK);
    let mut width = 14.0f64;
    let tm = ffi::ctx_measure_text(ctx, "W");
    if tm != 0 {
        width = ffi::tm_width(tm).max(1.0);
        ffi::release(tm);
    }
    let mut h = 0.0f64;
    for ch in MEASURE_PROBES {
        let tm = ffi::ctx_measure_text(ctx, ch);
        if tm != 0 {
            let bh = ffi::tm_ascent(tm) + ffi::tm_descent(tm);
            ffi::release(tm);
            if bh > h {
                h = bh;
            }
        }
    }
    (width, (h > 0.0).then_some(h))
}

/// Measure actual cell dimensions from the font.
///
/// Width comes from `"W"` (the monospace advance). Height is the tallest
/// *painted* glyph across [`MEASURE_PROBES`], rasterized to pixels. The font
/// metric boxes (`font_bounding_box_*`, and even `actual_bounding_box_*`) are
/// larger than what is actually drawn at terminal sizes, which leaves hairline
/// vertical seams between rows of `│`/`─` in box-drawing UIs (opencode borders,
/// `htop`, `vim` splits, ...). Measuring ink directly makes the cell pitch match
/// the painted glyphs for whatever font the system resolves the stack to.
pub(crate) fn measure_cell_dimensions(ctx: JsHandle) -> (f64, f64) {
    let (width, fallback) = measure_text_advance(ctx);
    finish_cell_dims(width, rasterized_glyph_height_max(MEASURE_PROBES, ctx).or(fallback))
}

/// Measure cell dimensions using a throwaway scratch canvas, so the real
/// terminal canvas is never given a 2D context. This keeps the canvas free for
/// a later `get_context("webgl2")` (a canvas may only have one context type).
pub(crate) fn measure_cell_dimensions_scratch() -> Option<(f64, f64)> {
    let win = ffi::window();
    if win == 0 {
        return None;
    }
    let doc = ffi::window_document(win);
    if doc == 0 {
        return None;
    }
    let scratch = ffi::document_create_canvas(doc);
    if scratch == 0 {
        return None;
    }
    ffi::canvas_set_width(scratch, SCRATCH_SIZE as u32);
    ffi::canvas_set_height(scratch, SCRATCH_SIZE as u32);
    let ctx = ffi::canvas_get_2d(scratch);
    let result = if ctx == 0 {
        None
    } else {
        let (width, fallback) = measure_text_advance(ctx);
        Some(finish_cell_dims(
            width,
            rasterized_glyph_height_max(MEASURE_PROBES, ctx).or(fallback),
        ))
    };
    if ctx != 0 {
        ffi::release(ctx);
    }
    ffi::release(scratch);
    result
}

/// Round the measured width/height to whole device pixels and sanity-guard them.
///
/// Snap to whole device pixels (xterm-style): every cell starts on an integer
/// coordinate, so adjacent glyphs share exact pixel boundaries instead of
/// leaving anti-aliased hairline seams at fractional advances. Columns/rows are
/// then `floor(canvas / cell)` and any leftover pixels become background
/// padding around the terminal.
fn finish_cell_dims(width: f64, height: Option<f64>) -> (f64, f64) {
    let width = width.round().max(1.0);
    let height = height.unwrap_or(20.0).round().max(1.0);
    (width, height)
}

/// Format a `0xRRGGBB` integer as an HTML/CSS `#rrggbb` string.
pub(crate) fn css_color(rgb: u32) -> String {
    format!("#{:06x}", rgb)
}