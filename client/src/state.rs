// Client terminal state.
//
// Holds the vt100 parser, the active renderer (WebGL2 primary, Canvas 2D
// fallback), cell dimensions, and selection state. Exposes the methods the
// WASM exports mutate it through.

use std::cell::RefCell;
use vt100::Parser;

use crate::color::{cell_visual, color_to_rgb, CellOverride, DEFAULT_BG, DEFAULT_FG};
use crate::ffi::{self, JsHandle};
use crate::graphics::draw_graphic_cell;
use crate::measure::{
    css_color, device_pitch, fit_grid, measure_cell_dimensions_scratch, CELL_EPSILON, FONT_STACK,
    FONT_STACK_BOLD,
};
use crate::renderer;
use crate::selection::{cell_is_selected, normalized_bounds, SelectionMode};

pub(crate) const DEFAULT_ROWS: u16 = 24;
pub(crate) const DEFAULT_COLS: u16 = 80;
pub(crate) const SCROLLBACK_LEN: usize = 1024;

/// The `vt100` crate implements the ANSI save/restore cursor sequences
/// (`ESC 7`/`ESC 8`) but silently ignores the CSI equivalents (`CSI s`/`CSI u`)
/// that many TUIs (opencode, zsh) emit. Because DECSET/DECRESET 1049 save and
/// restore the normal-grid cursor via `decsc`/`decrc`, ignoring `CSI s`/`CSI u`
/// makes the cursor land at the home position after exiting the alternate
/// screen. This maps `CSI s`/`CSI u` onto the implemented `ESC 7`/`ESC 8`
/// before the bytes reach the parser.
///
/// Sequence tails that may straddle a `process_bytes` chunk boundary are
/// carried in `carry` across calls.
pub(crate) fn normalize_save_restore(bytes: &[u8], carry: &mut Vec<u8>) -> Vec<u8> {
    let mut feed = std::mem::take(carry);
    feed.extend_from_slice(bytes);

    let carry_from =
        if feed.len() >= 2 && feed[feed.len() - 2] == 0x1b && feed[feed.len() - 1] == b'[' {
            feed.len() - 2
        } else if !feed.is_empty() && feed[feed.len() - 1] == 0x1b {
            feed.len() - 1
        } else {
            feed.len()
        };

    let mut out = Vec::with_capacity(feed.len());
    let mut i = 0;
    while i < carry_from {
        if i + 2 < feed.len()
            && feed[i] == 0x1b
            && feed[i + 1] == b'['
            && (feed[i + 2] == b's' || feed[i + 2] == b'u')
        {
            out.push(0x1b);
            out.push(if feed[i + 2] == b's' { b'7' } else { b'8' });
            i += 3;
        } else {
            out.push(feed[i]);
            i += 1;
        }
    }
    if carry_from < feed.len() {
        *carry = feed[carry_from..].to_vec();
    }
    out
}

/// Scroll-related state, grouped for clarity.
struct ScrollState {
    /// Normal screen scrollback offset saved when entering alternate screen
    saved_normal_offset_for_alt: Option<usize>,
    /// Scrollback view offset for the normal screen (0 = at bottom, >0 = scrolled up)
    normal_offset: usize,
    /// Scrollback view offset for the alternate screen (typically always 0)
    alternate_offset: usize,
}

impl ScrollState {
    fn new() -> Self {
        Self {
            saved_normal_offset_for_alt: None,
            normal_offset: 0,
            alternate_offset: 0,
        }
    }

    fn offset(&self, alternate: bool) -> usize {
        if alternate {
            self.alternate_offset
        } else {
            self.normal_offset
        }
    }

    fn set_offset(&mut self, alternate: bool, offset: usize) {
        if alternate {
            self.alternate_offset = offset;
        } else {
            self.normal_offset = offset;
        }
    }
}

/// Selection-related state, grouped for clarity.
struct SelectionState {
    /// Selection mode
    mode: SelectionMode,
    /// Text selection start cell
    start: Option<(u16, u16)>,
    /// Text selection end cell
    end: Option<(u16, u16)>,
}

impl SelectionState {
    fn new() -> Self {
        Self {
            mode: SelectionMode::None,
            start: None,
            end: None,
        }
    }

    fn clear(&mut self) {
        self.start = None;
        self.end = None;
        self.mode = SelectionMode::None;
    }

    fn is_selected(&self, row: u16, col: u16) -> bool {
        match (self.start, self.end) {
            (Some(a), Some(b)) => cell_is_selected(a, b, row, col),
            _ => false,
        }
    }

    fn cells(&self, cols: u16) -> Vec<(u16, u16)> {
        let (Some(a), Some(b)) = (self.start, self.end) else {
            return Vec::new();
        };
        let ((sr, sc), (er, ec)) = normalized_bounds(a, b);
        let cols = cols.max(1);
        let mut cells = Vec::new();
        for row in sr..=er {
            let c0 = if row == sr { sc } else { 0 };
            let c1 = if row == er {
                ec.min(cols - 1)
            } else {
                cols - 1
            };
            for col in c0..=c1 {
                cells.push((row, col));
            }
        }
        cells
    }
}

/// Terminal state fields.
///
/// Holds the vt100 parser, the active renderer (WebGL2 primary, Canvas 2D
/// fallback), cell dimensions, and selection state. Exposes the methods the
/// WASM exports mutate it through.
pub(crate) struct TerminalState {
    /// vt100 parser
    parser: Parser,
    /// 2D rendering context (Canvas 2D fallback path)
    ctx: Option<JsHandle>,
    /// WebGL2 renderer (primary path)
    webgl: Option<renderer::WebGL2Renderer>,
    /// Canvas element (source of pixel dimensions)
    canvas: JsHandle,
    /// Canvas element ID
    canvas_id: String,
    /// Terminal dimensions in cells
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// Measured cell width in CSS pixels
    pub(crate) cell_width: f64,
    /// Measured cell height in CSS pixels
    pub(crate) cell_height: f64,
    /// Pixel offset of the grid inside the canvas, in device pixels. The grid
    /// holds as many whole cells as fit and is centered in the leftover, so
    /// this is what keeps the terminal inside the window instead of running
    /// off the right/bottom edge.
    origin_x: i64,
    origin_y: i64,
    /// Scroll-related state
    scroll: ScrollState,
    /// Selection-related state
    selection: SelectionState,
    /// Previous screen state, used to diff against the current screen after
    /// each `process_bytes` batch to find cells that changed.
    prev_screen: Option<vt100::Screen>,
    /// Cells that changed since the last render. Consumed by the renderer to
    /// redraw only the affected cells instead of the whole grid.
    dirty_cells: Vec<(u16, u16)>,
    /// True when a full redraw is required (resize, scroll, selection change,
    /// first frame). Cleared after the next render.
    full_redraw: bool,
    /// Last rendered cursor cell, so the old highlight can be cleared when
    /// the cursor moves or becomes hidden.
    prev_cursor: Option<(u16, u16)>,
    /// True when a render pass has been scheduled via `schedule_render`
    /// and needs to be flushed on the next JS animation frame.
    needs_render: bool,
    /// Incomplete tail of an ESC sequence from the previous `process_bytes`
    /// call that may yet form a `CSI s`/`CSI u` cursor save/restore.
    csi_su_carry: Vec<u8>,
}

/// Renderer selection override, set from JS before `init()` via
/// `set_renderer_mode`. 0 = auto (WebGL2 first, Canvas 2D fallback),
/// 1 = force WebGL2 (error if unavailable), 2 = force Canvas 2D.
pub static RENDERER_MODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

impl TerminalState {
    /// Create a new terminal state with a Canvas 2D rendering context
    ///
    /// # Parameters
    /// * `canvas_id` - HTML canvas element ID
    pub(crate) fn new(canvas_id: &str, cached: Option<(f64, f64)>) -> Result<Self, String> {
        let parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);

        let win = ffi::window();
        let doc = if win != 0 {
            ffi::window_document(win)
        } else {
            0
        };
        let canvas = if doc != 0 {
            ffi::document_get_element_by_id(doc, canvas_id)
        } else {
            0
        };
        if canvas == 0 {
            return Err(format!("canvas '#{}' not found", canvas_id));
        }

        // Try Canvas 2D first; fall back to WebGL2
        let dpr = ffi::window_dpr(win);

        // Use cached cell dims when available (avoids scratch canvas measurement
        // on repeat visits). Fall back to a scratch canvas measurement, then to
        // sensible defaults. Snap to a whole device-pixel pitch so the CSS size,
        // the GL cell size and the column/row fit all agree.
        let dpr_eff = dpr.max(1.0);
        let (cell_width, cell_height) = cached
            .filter(|(w, h)| *w > 0.0 && *h > 0.0)
            .or_else(measure_cell_dimensions_scratch)
            .unwrap_or((14.0, 20.0));
        let cell_width = device_pitch(cell_width, dpr_eff) as f64 / dpr_eff;
        let cell_height = device_pitch(cell_height, dpr_eff) as f64 / dpr_eff;

        // Try WebGL2 first for GPU-accelerated rendering; fall back to Canvas 2D.
        // `?r=gl`/`?r=2d` (via set_renderer_mode) force one renderer for A/B
        // comparison: mode 1 fails hard when WebGL2 is unavailable; mode 2
        // skips WebGL2 entirely.
        let mode = RENDERER_MODE.load(std::sync::atomic::Ordering::Relaxed);
        let mut webgl = None;
        if mode != 2 {
            match renderer::WebGL2Renderer::new(
                canvas_id,
                cell_width,
                cell_height,
                DEFAULT_ROWS,
                DEFAULT_COLS,
                dpr,
            ) {
                Ok(w) => {
                    ffi::console_log(if mode == 1 {
                        "KRUST: WebGL2 renderer initialized (forced r=gl)"
                    } else {
                        "KRUST: WebGL2 renderer initialized"
                    });
                    webgl = Some(w);
                }
                Err(e) => {
                    if mode == 1 {
                        return Err(format!("r=gl: WebGL2 unavailable: {}", e));
                    }
                    ffi::console_log("KRUST: WebGL2 unavailable, falling back to Canvas 2D");
                }
            }
        } else {
            ffi::console_log("KRUST: forced Canvas 2D renderer (r=2d)");
        }
        // Canvas 2D fallback: only when WebGL2 could not be obtained (or was
        // skipped by the forced-2D mode).
        let ctx = if webgl.is_none() {
            let c = ffi::canvas_get_2d(canvas);
            (c != 0).then_some(c)
        } else {
            None
        };
        if ctx.is_none() && webgl.is_none() {
            return Err(
                "no rendering context available (WebGL2 and Canvas 2D both failed)".to_string(),
            );
        }

        let canvas_w = ffi::element_offset_width(canvas);
        let canvas_h = ffi::element_offset_height(canvas);

        let mut state = TerminalState {
            parser,
            ctx,
            webgl,
            canvas,
            canvas_id: canvas_id.to_string(),
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            cell_width,
            cell_height,
            origin_x: 0,
            origin_y: 0,
            scroll: ScrollState::new(),
            selection: SelectionState::new(),
            prev_screen: None,
            dirty_cells: Vec::new(),
            full_redraw: true,
            prev_cursor: None,
            needs_render: false,
            csi_su_carry: Vec::new(),
        };
        if canvas_w > 0.0 && canvas_h > 0.0 {
            state.refit(
                (canvas_w * dpr_eff).round() as i64,
                (canvas_h * dpr_eff).round() as i64,
            );
        }
        state.parser.screen_mut().set_size(state.rows, state.cols);
        Ok(state)
    }

    /// Process incoming ANSI bytes through the VT100 parser
    pub(crate) fn process_bytes(&mut self, bytes: &[u8]) {
        let screen_before = self.parser.screen().alternate_screen();

        // Clamp the current scroll offset to the (possibly shrunken) history
        // before new bytes arrive, so we always stay within valid range.
        self.clamp_scroll();
        // First batch: record the pre-processing screen so the first diff
        // produces a sensible dirty set (full grid) instead of nothing.
        if self.prev_screen.is_none() {
            self.prev_screen = Some(self.parser.screen().clone());
        }
        let normalized = normalize_save_restore(bytes, &mut self.csi_su_carry);
        self.parser.process(&normalized);

        let screen_after = self.parser.screen().alternate_screen();

        if !screen_before && screen_after {
            self.scroll.saved_normal_offset_for_alt = Some(self.scroll.normal_offset);
            self.parser
                .screen_mut()
                .set_scrollback(self.scroll.normal_offset);
        } else if screen_before && !screen_after {
            if let Some(saved) = self.scroll.saved_normal_offset_for_alt.take() {
                self.scroll.normal_offset = saved;
                self.parser.screen_mut().set_scrollback(saved);
            }
        }

        self.compute_dirty_cells();
    }

    /// Mark every cell dirty so the next render is a full redraw.
    pub(crate) fn mark_all_dirty(&mut self) {
        self.full_redraw = true;
        self.dirty_cells.clear();
    }

    /// Compare the current screen against the previous one and record the
    /// cells whose content or attributes changed. Also flags the wide
    /// character partner, so a wide glyph is always redrawn as a unit.
    fn compute_dirty_cells(&mut self) {
        if self.full_redraw {
            return;
        }
        let Some(prev) = self.prev_screen.clone() else {
            self.full_redraw = true;
            return;
        };
        let screen = self.parser.screen().clone();
        let (prows, pcols) = screen.size();
        let rows = if self.rows > 0 { self.rows } else { prows };
        let cols = if self.cols > 0 { self.cols } else { pcols };
        let mut dirty = Vec::new();
        for row in 0..rows {
            for col in 0..cols {
                if Self::cell_changed(&prev, &screen, row, col) {
                    dirty.push((row, col));
                    // Wide character partner: redraw the other half too.
                    if let Some(c) = screen.cell(row, col) {
                        if c.is_wide() && col + 1 < cols {
                            dirty.push((row, col + 1));
                        } else if c.is_wide_continuation() && col > 0 {
                            dirty.push((row, col - 1));
                        }
                    }
                    // The previous screen's wide partner may also need a
                    // redraw if the glyph changed or disappeared.
                    if let Some(pc) = prev.cell(row, col) {
                        if pc.is_wide() && col + 1 < cols {
                            dirty.push((row, col + 1));
                        } else if pc.is_wide_continuation() && col > 0 {
                            dirty.push((row, col - 1));
                        }
                    }
                }
            }
        }
        dirty.sort_unstable();
        dirty.dedup();
        self.dirty_cells = dirty;
        self.prev_screen = Some(screen);
    }

    /// Whether the cell at (`row`, `col`) differs between two screens.
    fn cell_changed(a: &vt100::Screen, b: &vt100::Screen, row: u16, col: u16) -> bool {
        match (a.cell(row, col), b.cell(row, col)) {
            (Some(ca), Some(cb)) => {
                ca.contents() != cb.contents()
                    || ca.fgcolor() != cb.fgcolor()
                    || ca.bgcolor() != cb.bgcolor()
                    || ca.bold() != cb.bold()
                    || ca.dim() != cb.dim()
                    || ca.italic() != cb.italic()
                    || ca.underline() != cb.underline()
                    || ca.inverse() != cb.inverse()
            }
            (Some(_), None) | (None, Some(_)) => true,
            (None, None) => false,
        }
    }

    /// Returns the scroll offset for the currently active screen.
    fn active_scroll_offset(&self) -> usize {
        let screen = self.parser.screen();
        self.scroll.offset(screen.alternate_screen())
    }

    /// Clamp the current scroll offset to the actual scrollback bounds.
    fn clamp_scroll(&mut self) {
        let max = self.active_scrollback_len();
        let cur = self.active_scroll_offset();
        if cur > max {
            let screen = self.parser.screen();
            self.scroll.set_offset(screen.alternate_screen(), max);
        }
    }

    /// Returns the scrollback length for the currently active screen.
    fn active_scrollback_len(&mut self) -> usize {
        let offset = self.active_scroll_offset();
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let max = screen.scrollback();
        screen.set_scrollback(offset);
        max
    }

    /// Returns the scrollback length for the currently active screen.
    pub(crate) fn scrollback_len(&mut self) -> usize {
        self.active_scrollback_len()
    }

    /// Whether the alternate screen is active.
    ///
    /// Full-screen TUIs (opencode, vim, less, htop) own the screen and manage
    /// their own scrolling, so the alt screen's grid — which vt100 keeps
    /// separate from the normal screen, and which still accumulates rows when
    /// the app scrolls the full region up — is not offered as scrollback.
    pub(crate) fn is_alt_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Current scrollback view offset (0 = active screen at bottom, >0 = scrolled up).
    pub(crate) fn scroll_offset(&self) -> usize {
        self.active_scroll_offset()
    }

    /// Set the scrollback view offset for the currently active screen.
    pub(crate) fn set_scroll_offset(&mut self, offset: usize) {
        let current = self.active_scroll_offset();
        if current != offset {
            self.clear_selection();
            let screen = self.parser.screen();
            self.scroll.set_offset(screen.alternate_screen(), offset);
            self.clamp_scroll();
        }
        self.apply_scrollback();
        self.mark_all_dirty();
    }

    /// Adjust the scrollback view by `delta` lines (positive = up/back,
    /// negative = down/forward). Returns the resulting offset.
    pub(crate) fn scroll_by(&mut self, delta: isize) -> usize {
        let cur = self.active_scroll_offset() as isize;
        let next = (cur + delta).max(0);
        self.set_scroll_offset(next as usize);
        self.active_scroll_offset()
    }

    /// Snap the view to the bottom (active screen).
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.set_scroll_offset(0);
    }

    /// Snap the view to the oldest available history row on the active screen.
    pub(crate) fn scroll_to_top(&mut self) {
        let max = self.active_scrollback_len();
        self.set_scroll_offset(max);
    }

    /// Apply the current scroll offset to the parser screen view for the
    /// currently active screen.
    fn apply_scrollback(&mut self) {
        let screen_alt = self.parser.screen().alternate_screen();
        let offset = self.scroll.offset(screen_alt);
        if self.parser.screen().scrollback() != offset {
            self.parser.screen_mut().set_scrollback(offset);
        }
    }

    /// Dispatches to the active renderer: WebGL2 by default, Canvas 2D
    /// when WebGL2 was unavailable. Honors the `needs_render` flag set by
    /// `schedule_render` to enable frame coalescing.
    pub(crate) fn render(&mut self) -> Result<(), String> {
        // Browsers may drop the WebGL context while a tab or iframe is hidden
        // (notably on Firefox under memory pressure, but also Chrome when the
        // browser is under pressure). When the context is lost, all GL calls
        // become silent no-ops and the terminal freezes as a blank canvas
        // until a page reload. Detect this here and rebuild the renderer so
        // the next frame paints correctly as soon as the context is restored.
        if let Some(w) = self.webgl.as_mut() {
            // Check if the underlying WebGL context has been lost. If so,
            // rebuild the renderer to restore drawing.
            if ffi::gl_is_context_lost(w.ctx) {
                let rebuilt = self.rebuild_webgl();
                if rebuilt.is_err() {
                    // If we cannot rebuild (no GL context available), fall back
                    // to Canvas 2D renderer which is more resilient.
                    return self.render_canvas2d();
                }
            }
        }
        // Always render (caller decides when to invoke). The `needs_render`
        // flag only controls full vs selective strategy inside render_canvas2d.
        self.needs_render = false;
        self.apply_scrollback();
        if self.webgl.is_some() {
            // Compute everything that borrows `self` before taking the mutable
            // WebGL borrow, which may bake new glyphs into the atlas.
            let scroll_offset = self.active_scroll_offset();
            let selection = self.selection_cells();
            let (cr, cc) = self.parser.screen().cursor_position();
            // Hide the block cursor when scrolled into history.
            let cursor = if scroll_offset == 0 {
                (cr, cc)
            } else {
                (u16::MAX, u16::MAX)
            };
            let screen = self.parser.screen();
            if let Some(w) = self.webgl.as_mut() {
                return w.render(screen, DEFAULT_FG, DEFAULT_BG, &selection, cursor);
            }
        }
        self.render_canvas2d()
    }

    /// Build the list of cells in the active line-based (text-flow)
    /// selection. The anchor row runs from the anchor column to the end of
    /// the row, intermediate rows are selected in full, and the last row runs
    /// from column 0 to the end column.
    fn selection_cells(&self) -> Vec<(u16, u16)> {
        self.selection.cells(self.cols)
    }

    /// Draw the current parser screen with native Canvas 2D text.
    ///
    /// Renders either the whole grid (first frame, resize, scroll, selection
    /// change) or only the cells that changed since the last frame, keeping
    /// redraw cost proportional to the amount of new output.
    fn render_canvas2d(&mut self) -> Result<(), String> {
        // Clone the (cheap) context handle so no borrow of `self` lingers
        // while the render helpers mutate other fields.
        let ctx = self.ctx.ok_or("no renderer")?;
        let (prows, pcols) = self.parser.screen().size();
        let rows = if self.rows > 0 { self.rows } else { prows };
        let cols = if self.cols > 0 { self.cols } else { pcols };
        let cw = self.cell_width;
        let ch = self.cell_height;

        let dpr = ffi::window_dpr(ffi::window());

        // Full redraw when forced, or when enough of the grid changed that a
        // selective pass would cost more than just repainting everything.
        let total = (rows as usize).saturating_mul(cols as usize);
        let dirty_count = self.dirty_cells.len();
        let full = self.full_redraw || dirty_count >= total / 2;
        self.full_redraw = false;

        let dirty = std::mem::take(&mut self.dirty_cells);

        ffi::ctx_set_transform(ctx, dpr.max(1.0), 0.0, 0.0, dpr.max(1.0), 0.0, 0.0);

        let (ox, oy) = self.origin_css(dpr);
        if full {
            self.render_full_grid(ctx, rows, cols, cw, ch, dpr, ox, oy)
        } else {
            self.render_dirty_cells(ctx, rows, cols, cw, ch, dpr, ox, oy, dirty)
        }?;
        Ok(())
    }

    /// Repaint the entire grid (first frame, resize, scroll, selection change).
    #[allow(clippy::too_many_arguments)]
    fn render_full_grid(
        &mut self,
        ctx: JsHandle,
        rows: u16,
        cols: u16,
        cw: f64,
        ch: f64,
        dpr: f64,
        ox: f64,
        oy: f64,
    ) -> Result<(), String> {
        let screen = self.parser.screen();
        let (prows, pcols) = screen.size();
        let css_w = ffi::canvas_width(self.canvas) as f64 / dpr;
        let css_h = ffi::canvas_height(self.canvas) as f64 / dpr;

        // Clear to default background
        ffi::ctx_set_fill_style(ctx, &css_color(DEFAULT_BG));
        ffi::ctx_fill_rect(ctx, 0.0, 0.0, css_w.max(1.0), css_h.max(1.0));
        ffi::ctx_set_text_baseline(ctx, "middle");

        let mut font = FONT_STACK.to_string();
        ffi::ctx_set_font(ctx, &font);

        // Paint each cell
        for row in 0..rows {
            for col in 0..cols {
                self.paint_cell(
                    ctx, screen, row, col, cw, ch, prows, pcols, &mut font, ox, oy,
                );
            }
        }

        // Cursor: background then text (hidden when scrolled into history)
        let cur = self.visible_cursor(screen, rows, cols);
        let cursor_pos = self.draw_cursor(ctx, cur, cw, ch, ox, oy)?;
        self.prev_cursor = cursor_pos;
        Ok(())
    }

    /// Redraw only cells that changed since the last render, plus the cursor.
    #[allow(clippy::too_many_arguments)]
    fn render_dirty_cells(
        &mut self,
        ctx: JsHandle,
        rows: u16,
        cols: u16,
        cw: f64,
        ch: f64,
        dpr: f64,
        ox: f64,
        oy: f64,
        mut dirty: Vec<(u16, u16)>,
    ) -> Result<(), String> {
        let screen = self.parser.screen();
        let (prows, pcols) = screen.size();
        let css_w = ffi::canvas_width(self.canvas) as f64 / dpr;
        let css_h = ffi::canvas_height(self.canvas) as f64 / dpr;

        // Clear entire canvas to default background
        ffi::ctx_set_fill_style(ctx, &css_color(DEFAULT_BG));
        ffi::ctx_fill_rect(ctx, 0.0, 0.0, css_w.max(1.0), css_h.max(1.0));

        // Cells to repaint: everything marked dirty, plus current and previous cursor cells
        let cur = self.visible_cursor(screen, rows, cols);
        if let Some(c) = cur {
            dirty.push(c);
        }
        if let Some(pc) = self.prev_cursor {
            if pc.0 < rows && pc.1 < cols {
                dirty.push(pc);
            }
        }
        dirty.sort_unstable();
        dirty.dedup();

        ffi::ctx_set_text_baseline(ctx, "middle");
        let mut font = FONT_STACK.to_string();
        ffi::ctx_set_font(ctx, &font);

        for &(row, col) in dirty.iter() {
            self.paint_cell(
                ctx, screen, row, col, cw, ch, prows, pcols, &mut font, ox, oy,
            );
        }

        // Cursor drawn last, on top of the regular cell content.
        let cur = self.visible_cursor(screen, rows, cols);
        let cursor_pos = self.draw_cursor(ctx, cur, cw, ch, ox, oy)?;
        self.prev_cursor = cursor_pos;
        Ok(())
    }

    /// The cursor cell to render, or `None` when scrolled into history.
    fn visible_cursor(&self, screen: &vt100::Screen, rows: u16, cols: u16) -> Option<(u16, u16)> {
        let active_offset = self.scroll.offset(screen.alternate_screen());
        if active_offset != 0 {
            return None;
        }
        let (cr, cc) = screen.cursor_position();
        if cr < rows && cc < cols {
            Some((cr, cc))
        } else {
            None
        }
    }

    /// Schedule a render call for the next `requestAnimationFrame` callback.
    /// This enables frame coalescing: multiple `process_bytes` calls within a
    /// single animation frame will only result in one render.
    pub(crate) fn schedule_render(&mut self) {
        self.needs_render = true;
    }

    /// Draw the block cursor (swapped fg/bg + glyph) on top of a cell.
    /// `cursor` is the cell to draw, or `None` to skip cursor drawing.
    /// `ox`/`oy` are the grid origin in CSS pixels.
    #[allow(clippy::too_many_arguments)]
    fn draw_cursor(
        &mut self,
        ctx: JsHandle,
        cursor: Option<(u16, u16)>,
        cw: f64,
        ch: f64,
        ox: f64,
        oy: f64,
    ) -> Result<Option<(u16, u16)>, String> {
        let Some((cr, cc)) = cursor else {
            return Ok(None);
        };
        let screen = self.parser.screen();
        let cell = screen.cell(cr, cc);
        let (x, y) = (ox + cc as f64 * cw, oy + cr as f64 * ch);
        // Shared cursor decision: swap original fg/bg (block cursor).
        let (fg, bg) = cell_visual(cell, DEFAULT_FG, DEFAULT_BG, CellOverride::Cursor);
        ffi::ctx_set_fill_style(ctx, &css_color(bg));
        ffi::ctx_fill_rect(ctx, x, y, cw, ch);
        ffi::ctx_set_fill_style(ctx, &css_color(fg));
        if let Some(c) = cell {
            let s = c.contents();
            if !s.is_empty() && !draw_graphic_cell(ctx, x, y, cw, ch, s, fg) {
                ffi::ctx_fill_text(ctx, s, x, y + ch * 0.5);
            }
        }
        Ok(Some((cr, cc)))
    }

    /// Whether the given cell lies inside the active line-based (text-flow)
    /// selection (normalized so the anchor can be above/below the end).
    fn selected(&self, row: u16, col: u16) -> bool {
        self.selection.is_selected(row, col)
    }

    /// Paint a single cell: clear to default bg, paint non-default bg,
    /// paint selection bg, and draw text glyph.
    ///
    /// `ox`/`oy` are the grid origin in CSS pixels; every rect is placed
    /// relative to it so the grid can be centered inside the canvas.
    #[allow(clippy::too_many_arguments)]
    fn paint_cell(
        &self,
        ctx: JsHandle,
        screen: &vt100::Screen,
        row: u16,
        col: u16,
        cw: f64,
        ch: f64,
        prows: u16,
        pcols: u16,
        font: &mut String,
        ox: f64,
        oy: f64,
    ) {
        let (x, y) = (ox + col as f64 * cw, oy + row as f64 * ch);

        // 1. Clear cell to default background
        ffi::ctx_set_fill_style(ctx, &css_color(DEFAULT_BG));
        ffi::ctx_fill_rect(ctx, x, y, cw, ch);

        let cell = if row < prows && col < pcols {
            screen.cell(row, col)
        } else {
            None
        };
        let selected = self.selected(row, col);
        let override_ = if selected {
            CellOverride::Selected
        } else {
            CellOverride::Normal
        };

        // 2. Non-default background rect
        let base_bg = match cell {
            Some(c) => color_to_rgb(c.bgcolor(), DEFAULT_BG),
            _ => DEFAULT_BG,
        };
        if base_bg != DEFAULT_BG && !selected {
            ffi::ctx_set_fill_style(ctx, &css_color(base_bg));
            ffi::ctx_fill_rect(ctx, x, y, cw, ch);
        }

        // 3. Selection background rect (before text)
        if selected {
            let (_fg, sel_bg) = cell_visual(cell, DEFAULT_FG, DEFAULT_BG, CellOverride::Selected);
            ffi::ctx_set_fill_style(ctx, &css_color(sel_bg));
            ffi::ctx_fill_rect(
                ctx,
                x - CELL_EPSILON,
                y - CELL_EPSILON,
                cw + CELL_EPSILON * 2.0,
                ch + CELL_EPSILON * 2.0,
            );
        }

        // 4. Text glyph
        if let Some(c) = cell {
            let s = c.contents();
            if !s.is_empty() {
                let (draw_fg, _bg) = cell_visual(Some(c), DEFAULT_FG, DEFAULT_BG, override_);
                if draw_fg != DEFAULT_BG {
                    if draw_graphic_cell(ctx, x, y, cw, ch, s, draw_fg) {
                        return;
                    }
                    let want = if c.bold() {
                        FONT_STACK_BOLD
                    } else {
                        FONT_STACK
                    };
                    if font.as_str() != want {
                        *font = want.to_string();
                        ffi::ctx_set_font(ctx, font);
                    }
                    ffi::ctx_set_fill_style(ctx, &css_color(draw_fg));
                    ffi::ctx_fill_text(ctx, s, x, y + ch * 0.5);
                }
            }
        }
    }

    /// Trigger resize callback
    pub(crate) fn trigger_resize(&mut self, _new_rows: u16, _new_cols: u16) {}

    /// Handle selection start
    pub(crate) fn handle_selection_start(&mut self, row: u16, col: u16) {
        self.selection.mode = SelectionMode::Line;
        self.selection.start = Some((row, col));
        self.selection.end = None;
        self.mark_all_dirty();
    }

    /// Handle selection update
    pub(crate) fn handle_selection_update(&mut self, row: u16, col: u16) {
        if let Some(ref mut end) = self.selection.end {
            if *end != (row, col) {
                *end = (row, col);
                self.mark_all_dirty();
            }
        } else if self.selection.start.is_some() {
            self.selection.end = Some((row, col));
            self.mark_all_dirty();
        }
    }

    /// Clear the active selection (reset both anchor and end to None)
    pub(crate) fn clear_selection(&mut self) {
        if self.selection.start.is_some() || self.selection.end.is_some() {
            self.mark_all_dirty();
        }
        self.selection.clear();
    }

    /// Get the canvas ID
    pub(crate) fn canvas_id(&self) -> &str {
        &self.canvas_id
    }

    /// Get the current terminal dimensions
    pub(crate) fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Access the parser's screen.
    pub(crate) fn parser_screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Access the selection start.
    pub(crate) fn selection_start(&self) -> Option<(u16, u16)> {
        self.selection.start
    }

    /// Access the selection end.
    pub(crate) fn selection_end(&self) -> Option<(u16, u16)> {
        self.selection.end
    }

    /// Access the selection mode.
    pub(crate) fn selection_mode(&self) -> SelectionMode {
        self.selection.mode
    }

    /// Mutable access to the WebGL renderer for resize handling.
    pub(crate) fn webgl_mut(&mut self) -> Option<&mut renderer::WebGL2Renderer> {
        self.webgl.as_mut()
    }

    /// Mutable access to the canvas.
    pub(crate) fn canvas_handle(&self) -> JsHandle {
        self.canvas
    }

    /// Whether the active renderer is the Canvas 2D path.
    pub(crate) fn ctx(&self) -> Option<JsHandle> {
        self.ctx
    }

    /// Short name of the renderer that actually ended up active: `"gl"` for
    /// the WebGL2 path, `"2d"` for the Canvas 2D fallback. Reported in the
    /// `init` config JSON so the page can show which mode won the fallback
    /// race (and which `?r=` override is really in effect).
    pub(crate) fn renderer_name(&self) -> &'static str {
        if self.webgl.is_some() {
            "gl"
        } else {
            "2d"
        }
    }

    /// Set measured cell dimensions (used on resize).
    pub(crate) fn set_cell_dims(&mut self, cw: f64, ch: f64) {
        self.cell_width = cw;
        self.cell_height = ch;
        self.snap_cell_dims();
    }

    /// Snap the cell dimensions to a whole device-pixel pitch.
    ///
    /// The page hit-tests against the CSS-pixel size while both renderers draw
    /// at the integer device-pixel pitch, so the CSS size has to be exactly
    /// `device_pitch / dpr` — otherwise the two drift apart and a click near
    /// the right edge of a wide terminal lands in the wrong column.
    fn snap_cell_dims(&mut self) {
        let dpr = ffi::window_dpr(ffi::window()).max(1.0);
        self.cell_width = device_pitch(self.cell_width, dpr) as f64 / dpr;
        self.cell_height = device_pitch(self.cell_height, dpr) as f64 / dpr;
    }

    /// Resize the grid to fit a canvas of `w` x `h` device pixels, dropping
    /// whole columns/rows until it fits and centering what is left.
    ///
    /// The count comes from the *device-pixel* cell pitch — the same number the
    /// renderers draw with — so the grid can never be laid out wider than the
    /// canvas. The leftover pixels are split evenly, giving a small equal margin
    /// instead of a ragged strip down one side.
    ///
    /// Also syncs the WebGL2 renderer's cell pitch and origin, which is the
    /// other half of the same fit.
    pub(crate) fn refit(&mut self, w_dev: i64, h_dev: i64) {
        let dpr = ffi::window_dpr(ffi::window()).max(1.0);
        let cell_w = device_pitch(self.cell_width, dpr);
        let cell_h = device_pitch(self.cell_height, dpr);
        let (cols, origin_x) = fit_grid(w_dev, cell_w);
        let (rows, origin_y) = fit_grid(h_dev, cell_h);
        self.cols = cols.clamp(2, u16::MAX as i64) as u16;
        self.rows = rows.clamp(1, u16::MAX as i64) as u16;
        self.origin_x = origin_x;
        self.origin_y = origin_y;
        let (rows, cols) = (self.rows, self.cols);
        if let Some(w) = self.webgl.as_mut() {
            w.cell_w = cell_w as u32;
            w.cell_h = cell_h as u32;
            w.origin_x = origin_x as u32;
            w.origin_y = origin_y as u32;
            w.rows = rows;
            w.cols = cols;
        }
    }

    /// Grid origin in CSS pixels, for the Canvas 2D render path.
    pub(crate) fn origin_css(&self, dpr: f64) -> (f64, f64) {
        let dpr = dpr.max(1.0);
        (self.origin_x as f64 / dpr, self.origin_y as f64 / dpr)
    }

    /// Map a canvas-relative CSS-pixel point onto a cell, clamped to the grid.
    ///
    /// The grid is centered in the canvas, so points in the surrounding margin
    /// have to be clamped to the nearest cell rather than running off the edge.
    pub(crate) fn cell_at(&self, x: f64, y: f64) -> (u16, u16) {
        let dpr = ffi::window_dpr(ffi::window()).max(1.0);
        let (ox, oy) = self.origin_css(dpr);
        let col = (((x - ox) * dpr).max(0.0) / device_pitch(self.cell_width, dpr).max(1) as f64)
            .floor()
            .min(self.cols.saturating_sub(1) as f64) as u16;
        let row = (((y - oy) * dpr).max(0.0) / device_pitch(self.cell_height, dpr).max(1) as f64)
            .floor()
            .min(self.rows.saturating_sub(1) as f64) as u16;
        (row, col)
    }

    /// Recreate the entire WebGL2 renderer (shader program, buffers and glyph
    /// atlas) from scratch on the currently bound context.
    ///
    /// Browsers may drop the WebGL context while a tab is hidden (Firefox does
    /// this under memory pressure). Every GL object handle becomes invalid, so
    /// `render()` on the old objects silently no-ops and the terminal freezes
    /// as a blank canvas — which is what krust showed whenever the user
    /// switched tabs away and back. After the browser fires
    /// `webglcontextrestored`, calling this rebuilds all objects and the next
    /// `render()` paints a full frame.
    pub(crate) fn rebuild_webgl(&mut self) -> Result<(), String> {
        let Some(_) = self.webgl.as_ref() else {
            return Ok(());
        };
        let dpr = ffi::window_dpr(ffi::window());
        let mut fresh = renderer::WebGL2Renderer::new(
            &self.canvas_id,
            self.cell_width,
            self.cell_height,
            self.rows,
            self.cols,
            dpr,
        )?;
        fresh.origin_x = self.origin_x as u32;
        fresh.origin_y = self.origin_y as u32;
        self.webgl = Some(fresh);
        self.mark_all_dirty();
        Ok(())
    }

    /// Resize the underlying parser screen to `rows` x `cols`.
    pub(crate) fn resize_screen(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Throw away all parser state and start from a blank screen.
    ///
    /// Used when the server reports this client has fallen out of the
    /// retained output window: a contiguous byte tail can no longer bring the
    /// grid up to date, so the only correct move is to forget the old state
    /// and rebuild it from the log that follows. The renderer is kept —
    /// canvas, cell pitch and GL objects are all still valid — and the next
    /// `render` is a full redraw because `mark_all_dirty` forces one.
    pub(crate) fn reset(&mut self) {
        self.parser = Parser::new(self.rows, self.cols, SCROLLBACK_LEN);
        self.scroll = ScrollState::new();
        self.selection = SelectionState::new();
        self.prev_screen = None;
        self.prev_cursor = None;
        self.csi_su_carry.clear();
        self.mark_all_dirty();
    }

    /// Whether the WebGL context has been lost, which leaves every GL object
    /// krust holds invalid until the renderer is rebuilt.
    pub(crate) fn webgl_is_lost(&self) -> bool {
        self.webgl.as_ref().is_some_and(renderer::WebGL2Renderer::is_lost)
    }
}

thread_local! {
    /// Global terminal state, initialized once by [`crate::init`]
    pub(crate) static TERM_STATE: RefCell<Option<TerminalState>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
mod resync_tests {
    use vt100::Parser;

    /// The whole visible content of the screen, as text.
    ///
    /// A blank cell reports empty contents (vt100 stores `len: 0` rather than
    /// a space), so render those as spaces to keep columns meaningful.
    fn screen_text(p: &Parser) -> String {
        let (rows, cols) = p.screen().size();
        let mut out = String::new();
        for row in 0..rows {
            let mut line = String::new();
            for col in 0..cols {
                match p.screen().cell(row, col) {
                    Some(c) if !c.contents().is_empty() => line.push_str(c.contents()),
                    _ => line.push(' '),
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// Regression: a resumed stream that begins mid-escape-sequence makes the
    /// parser print the orphaned tail as literal text. This is what put
    /// `;255m`, `25h` and `6l156;245m` in the input field — the server
    /// replayed a history buffer trimmed at an arbitrary byte offset, so the
    /// tail of an SGR/DECSET sequence was parsed as ground text in cells the
    /// application never wrote.
    #[test]
    fn mid_sequence_resume_prints_orphaned_escape_text() {
        let mut p = Parser::new(6, 80, 0);
        p.process(b"\x1b[1;1Hfield");
        // Resumed in the middle of "\x1b[38;5;255m".
        p.process(b";255m");
        let text = screen_text(&p);
        assert!(
            text.contains(";255m"),
            "expected the orphaned tail to be painted:\n{}",
            text
        );
    }

    /// The fix: the server trims the retained log on an ESC boundary, so a
    /// fresh client replaying it never starts mid-sequence and never prints
    /// escape-sequence text into the grid.
    #[test]
    fn esc_boundary_resume_prints_no_escape_text() {
        let mut p = Parser::new(6, 80, 0);
        // A replay that begins on an ESC, as the boundary-aligned trim
        // guarantees, including the sequences the bug report showed up in.
        p.process(b"\x1b[?1006l\x1b[38;5;255m\x1b[?25htext");
        let text = screen_text(&p);
        assert!(
            !text.contains(";255m") && !text.contains("25h") && !text.contains("6l"),
            "no escape-sequence fragments may reach the grid:\n{}",
            text
        );
        assert!(text.contains("text"), "content should still render:\n{}", text);
    }

    /// A delta resync continues the client's own byte stream, so the parser
    /// resumes mid-sequence exactly where it left off and nothing is lost.
    #[test]
    fn delta_resync_is_invisible_to_the_parser() {
        let stream: &[u8] = b"\x1b[2J\x1b[1;1Halpha\x1b[38;2;156;245mbeta";

        // Whole stream in one go.
        let mut whole = Parser::new(6, 80, 0);
        whole.process(stream);
        let expected = screen_text(&whole);

        // Same stream, delivered as a prefix plus a delta tail, with the
        // split landing in the middle of the SGR parameter list.
        let split = stream.len() - 6;
        let mut split_parser = Parser::new(6, 80, 0);
        split_parser.process(&stream[..split]);
        split_parser.process(&stream[split..]);
        assert_eq!(
            screen_text(&split_parser),
            expected,
            "a delta resync must not change the resulting screen"
        );
    }

    /// A duplicated resync (the old drop-then-replay-everything behaviour)
    /// does change the result, which is why the tail is sent instead.
    ///
    /// The stream uses relative cursor motion, which is what a TUI emits
    /// between full repaints. Replaying it runs those relative moves twice, so
    /// the second copy of the glyph lands further along the row — and lands in
    /// cells the application's own model has no knowledge of, which is exactly
    /// the reported symptom (arrow keys skip them, typing overwrites them, and
    /// a copy still yields them).
    #[test]
    fn duplicated_replay_is_not_equivalent() {
        let stream: &[u8] = b"\x1b[3CX";
        let mut whole = Parser::new(6, 80, 0);
        whole.process(stream);
        let expected = screen_text(&whole);
        assert_eq!(expected, "   X\n\n\n\n\n\n");

        let mut replayed = Parser::new(6, 80, 0);
        replayed.process(stream);
        replayed.process(stream);
        assert_eq!(screen_text(&replayed), "   X   X\n\n\n\n\n\n");
        assert_ne!(screen_text(&replayed), expected);
    }
}
