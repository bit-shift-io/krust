// Client cell-dimension measurement.
//
// Measures cell size from a scratch canvas so the real terminal canvas is never
// given a 2D context before the renderer choice is made (a canvas only supports
// one context type).

use crate::ffi::{self, JsHandle};

/// Epsilon to prevent 1-pixel anti-aliasing gaps between adjacent cell rects
pub(crate) const CELL_EPSILON: f64 = 0.5;

/// CSS `font-family` list for the WebGL2 atlas rasterizer. It is the same
/// family list as [`FONT_STACK`] (minus the size/line-height prefix), so the GL
/// atlas and the Canvas 2D reference resolve identical families and per-glyph
/// fallback. Only the wasm atlas rasterizer consumes it.
#[cfg(target_arch = "wasm32")]
pub(crate) const FONT_FAMILIES: &str = "'JetBrains Mono', 'Fira Code', Menlo, Consolas, monospace";

/// CSS font size (px) the Canvas 2D renderer draws text at. The WebGL2 atlas
/// rasterizes glyphs at `FONT_SIZE_CSS * dpr` device px to match.
pub(crate) const FONT_SIZE_CSS: f64 = 14.0;

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
        let written = ffi::ctx_get_image_data(
            ctx,
            0.0,
            0.0,
            SCRATCH_SIZE as f64,
            SCRATCH_SIZE as f64,
            &mut px,
        );
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
    finish_cell_dims(
        width,
        rasterized_glyph_height_max(MEASURE_PROBES, ctx).or(fallback),
    )
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

/// Round the measured width/height to whole CSS pixels and sanity-guard them.
///
/// This is a coarse first pass: a whole number of CSS pixels is only a whole
/// number of *device* pixels when `devicePixelRatio` is an integer, so it is
/// not by itself enough to keep cells pixel-exact. [`device_pitch`] does the
/// real snapping, in device pixels, and the CSS size callers draw with is
/// derived back from it. Columns/rows then come from `fit_grid` and any
/// leftover pixels become padding around the terminal.
fn finish_cell_dims(width: f64, height: Option<f64>) -> (f64, f64) {
    let width = width.round().max(1.0);
    let height = height.unwrap_or(20.0).round().max(1.0);
    (width, height)
}

/// Cell pitch in whole device pixels for a CSS-pixel cell size.
///
/// Both renderers and the grid fit have to agree on one pitch, because the fit
/// multiplies a cell count by it and each renderer multiplies that same count
/// by it again — if they disagreed, the grid would be laid out at one width and
/// drawn at another, and the wider one runs off the canvas edge.
///
/// The pitch must be a whole number of device pixels: the WebGL2 glyph atlas is
/// a 1:1 texel-to-device-pixel bitmap, so a fractional cell would resample
/// every glyph on every frame. Canvas 2D has no such constraint (it draws under
/// a scaled transform) and would happily use the fractional pitch, which is
/// exactly how the two paths drifted apart. Ceiling rather than rounding keeps
/// the atlas slot at least as wide as the font's real advance, so a wide glyph
/// is never clipped at the right edge of its cell.
pub(crate) fn device_pitch(css: f64, dpr: f64) -> i64 {
    (css.max(1.0) * dpr.max(1.0)).ceil().max(1.0) as i64
}

/// Fit a cell grid inside `avail` device pixels and center it in the leftover.
///
/// Returns `(count, origin)`: how many whole cells fit — never more than
/// `avail / cell`, so the grid can always be drawn entirely inside the canvas —
/// and the pixel offset of the grid from the canvas origin. The leftover is
/// split evenly, so the grid ends up centered with a small margin on each side
/// rather than hugging one edge. Both values are whole device pixels, which is
/// what keeps every cell boundary pixel-exact.
pub(crate) fn fit_grid(avail: i64, cell: i64) -> (i64, i64) {
    let avail = avail.max(0);
    let cell = cell.max(1);
    let count = avail / cell;
    (count, (avail - count * cell) / 2)
}

/// Format a `0xRRGGBB` integer as an HTML/CSS `#rrggbb` string.
pub(crate) fn css_color(rgb: u32) -> String {
    format!("#{:06x}", rgb)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fit_grid_never_lays_out_wider_than_the_canvas() {
        // The regression this guards: columns were counted in CSS pixels but
        // drawn at a wider device-pixel pitch, so the grid ran off the right
        // edge of the window.
        for avail in 0..600i64 {
            for cell in 1..40i64 {
                let (count, origin) = fit_grid(avail, cell);
                assert!(count >= 0);
                assert!(origin >= 0);
                assert!(
                    origin * 2 + count * cell <= avail,
                    "grid {count}x{cell} at {origin} overflows {avail}"
                );
                // The leftover is split as evenly as whole pixels allow.
                let right = avail - (origin + count * cell);
                assert!(right.abs_diff(origin) <= 1);
            }
        }
    }

    #[test]
    fn fit_grid_uses_every_whole_cell_that_fits() {
        assert_eq!(fit_grid(1500, 11), (136, 2));
        assert_eq!(fit_grid(1000, 8), (125, 0));
        // Too small for a single cell: the caller's minimum (2 cols, 1 row)
        // takes over rather than the fit inventing negative space.
        assert_eq!(fit_grid(5, 11), (0, 2));
        assert_eq!(fit_grid(0, 11), (0, 0));
    }

    #[test]
    fn device_pitch_is_whole_pixels_and_round_trips_through_css() {
        for dpr in [1.0, 1.25, 1.5, 2.0, 3.0] {
            for css in [1.0, 7.0, 8.4, 9.0, 14.0] {
                let dev = device_pitch(css, dpr);
                assert!(dev >= 1);
                assert_eq!(dev as f64, (css * dpr).ceil());
                // The atlas slot must never be narrower than the font's real
                // advance, or wide glyphs get clipped at the cell edge.
                assert!(dev as f64 >= css * dpr);
                // Deriving the CSS size from the integer pitch must round-trip,
                // or the page's hit-testing drifts away from what is drawn.
                let snapped = dev as f64 / dpr;
                assert_eq!(device_pitch(snapped, dpr), dev);
            }
        }
    }
}
