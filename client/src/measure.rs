// Client cell-dimension measurement.
//
// Measures cell size from a scratch canvas so the real terminal canvas is never
// given a 2D context before the renderer choice is made (a canvas only supports
// one context type).

use wasm_bindgen::JsCast;
use web_sys::CanvasRenderingContext2d;

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

/// Maximum ink height of `probes` after rasterizing each glyph onto a scratch
/// canvas. `None` if the 2D/`getImageData` pipeline is unavailable (the caller
/// then falls back to font metric boxes).
fn rasterized_glyph_height_max(
    probes: &[&str],
    _font_ctx: &CanvasRenderingContext2d,
) -> Option<f64> {
    let doc = web_sys::window()?.document()?;
    let mut max_h = 0.0f64;
    for ch in probes {
        let Ok(scratch) = doc.create_element("canvas") else { return None };
        let Ok(scratch) = scratch.dyn_into::<web_sys::HtmlCanvasElement>() else {
            return None;
        };
        scratch.set_width(64);
        scratch.set_height(64);
        let Some(ctx) = scratch.get_context("2d").ok().flatten() else {
            return None;
        };
        let Ok(ctx) = ctx.dyn_into::<CanvasRenderingContext2d>() else {
            return None;
        };
        ctx.set_font(&FONT_STACK);
        ctx.set_fill_style_str("#ffffff");
        ctx.set_text_baseline("alphabetic");
        let _ = ctx.fill_text(ch, 8.0, 40.0);
        let Ok(img) = ctx.get_image_data(0.0, 0.0, 64.0, 64.0) else { return None };
        let px = img.data();
        let mut top_row = 64usize;
        let mut bottom_row = 0usize;
        for y in 0..64usize {
            let mut ink = false;
            for x in 8..24usize {
                let alpha = px[(y * 64 + x) * 4 + 3] as f64;
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
fn measure_text_advance(ctx: &CanvasRenderingContext2d) -> (f64, Option<f64>) {
    ctx.set_font(FONT_STACK);
    let width = ctx
        .measure_text("W")
        .map(|m| m.width())
        .ok()
        .unwrap_or(14.0);
    let mut h = 0.0f64;
    for ch in MEASURE_PROBES {
        if let Ok(m) = ctx.measure_text(ch) {
            let bh = m.actual_bounding_box_ascent() + m.actual_bounding_box_descent();
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
pub(crate) fn measure_cell_dimensions(ctx: &CanvasRenderingContext2d) -> (f64, f64) {
    let (width, fallback) = measure_text_advance(ctx);
    finish_cell_dims(width, rasterized_glyph_height_max(MEASURE_PROBES, ctx).or(fallback))
}

/// Measure cell dimensions using a throwaway scratch canvas, so the real
/// terminal canvas is never given a 2D context. This keeps the canvas free for
/// a later `get_context("webgl2")` (a canvas may only have one context type).
pub(crate) fn measure_cell_dimensions_scratch() -> Option<(f64, f64)> {
    let doc = web_sys::window()?.document()?;
    let Ok(scratch) = doc.create_element("canvas") else { return None };
    let Ok(scratch) = scratch.dyn_into::<web_sys::HtmlCanvasElement>() else {
        return None;
    };
    let Some(ctx) = scratch.get_context("2d").ok().flatten() else {
        return None;
    };
    let Ok(ctx) = ctx.dyn_into::<CanvasRenderingContext2d>() else {
        return None;
    };
    let (width, fallback) = measure_text_advance(&ctx);
    Some(finish_cell_dims(
        width,
        rasterized_glyph_height_max(MEASURE_PROBES, &ctx).or(fallback),
    ))
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
