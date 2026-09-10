// Client terminal state.
//
// Holds the vt100 parser, the active renderer (WebGL2 primary, Canvas 2D
// fallback), cell dimensions, and selection state. Exposes the methods the
// WASM exports mutate it through.

use std::cell::RefCell;
use vt100::Parser;

use crate::color::{cell_fg_rgb, color_to_rgb, DEFAULT_BG, DEFAULT_FG};
use crate::ffi::{self, JsHandle};
use crate::graphics::draw_graphic_cell;
use crate::measure::{
    css_color, measure_cell_dimensions_scratch, CELL_EPSILON, FONT_STACK, FONT_STACK_BOLD,
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

    let carry_from = if feed.len() >= 2 && feed[feed.len() - 2] == 0x1b && feed[feed.len() - 1] == b'['
    {
        feed.len() - 2
    } else if feed.len() >= 1 && feed[feed.len() - 1] == 0x1b {
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
            let c1 = if row == er { ec.min(cols - 1) } else { cols - 1 };
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

impl TerminalState {
    /// Create a new terminal state with a Canvas 2D rendering context
    ///
    /// # Parameters
    /// * `canvas_id` - HTML canvas element ID
    pub(crate) fn new(canvas_id: &str, cached: Option<(f64, f64)>) -> Result<Self, String> {
        let parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);

        let win = ffi::window();
        let doc = if win != 0 { ffi::window_document(win) } else { 0 };
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
        // sensible defaults.
        let (cell_width, cell_height) = cached
            .filter(|(w, h)| *w > 0.0 && *h > 0.0)
            .or_else(|| measure_cell_dimensions_scratch())
            .unwrap_or((14.0, 20.0));

        // Try WebGL2 first for GPU-accelerated rendering; fall back to Canvas 2D.
        let mut webgl = None;
        if let Ok(w) = renderer::WebGL2Renderer::new(
            canvas_id,
            cell_width,
            cell_height,
            DEFAULT_ROWS,
            DEFAULT_COLS,
            dpr,
            renderer::EMBEDDED_FONT,
        ) {
            ffi::console_log("KRUST: WebGL2 renderer initialized");
            webgl = Some(w);
        } else {
            ffi::console_log("KRUST: WebGL2 unavailable, falling back to Canvas 2D");
        }
        // Canvas 2D fallback: only when WebGL2 could not be obtained.
        let ctx = if webgl.is_none() {
            let c = ffi::canvas_get_2d(canvas);
            (c != 0).then_some(c)
        } else {
            None
        };
        if ctx.is_none() && webgl.is_none() {
            return Err("no rendering context available (WebGL2 and Canvas 2D both failed)".to_string());
        }

        let canvas_w = ffi::element_offset_width(canvas);
        let canvas_h = ffi::element_offset_height(canvas);
        let cols = if canvas_w > 0.0 {
            (canvas_w / cell_width).floor() as u16
        } else {
            DEFAULT_COLS
        };
        let rows = if canvas_h > 0.0 {
            (canvas_h / cell_height).floor() as u16
        } else {
            DEFAULT_ROWS
        };

        Ok(TerminalState {
            parser,
            ctx,
            webgl,
            canvas,
            canvas_id: canvas_id.to_string(),
            rows,
            cols,
            cell_width,
            cell_height,
            scroll: ScrollState::new(),
            selection: SelectionState::new(),
            prev_screen: None,
            dirty_cells: Vec::new(),
            full_redraw: true,
            prev_cursor: None,
            needs_render: false,
            csi_su_carry: Vec::new(),
        })
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
            self.parser.screen_mut().set_scrollback(self.scroll.normal_offset);
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
        let old_offset = self.parser.screen().scrollback();
        if old_offset != offset {
            ffi::console_log(&format!(
                "APPLY_SCROLLBACK: screen_alt={} old_off={} new_off={} normal={} alt={}",
                screen_alt, old_offset, offset,
                self.scroll.normal_offset, self.scroll.alternate_offset
            ));
            self.parser.screen_mut().set_scrollback(offset);
        }
    }

    /// Dispatches to the active renderer: WebGL2 by default, Canvas 2D
    /// when WebGL2 was unavailable. Honors the `needs_render` flag set by
    /// `schedule_render` to enable frame coalescing.
    pub(crate) fn render(&mut self) -> Result<(), String> {
        // Always render (caller decides when to invoke). The `needs_render`
        // flag only controls full vs selective strategy inside render_canvas2d.
        self.needs_render = false;
        self.apply_scrollback();
        if let Some(w) = self.webgl.as_ref() {
            let screen = self.parser.screen();
            let (cr, cc) = screen.cursor_position();
            // Hide the block cursor when scrolled into history.
            let cursor = if self.active_scroll_offset() == 0 {
                (cr, cc)
            } else {
                (u16::MAX, u16::MAX)
            };
            let selection = self.selection_cells();
            return w.render(
                screen,
                DEFAULT_FG,
                DEFAULT_BG,
                &selection,
                cursor,
            );
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
        let ctx = self.ctx.clone().ok_or("no renderer")?;
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

        if full {
            self.render_full_grid(ctx, rows, cols, cw, ch, dpr)
        } else {
            self.render_dirty_cells(ctx, rows, cols, cw, ch, dpr, dirty)
        }?;
        Ok(())
    }

    /// Repaint the entire grid (first frame, resize, scroll, selection change).
    fn render_full_grid(
        &mut self,
        ctx: JsHandle,
        rows: u16,
        cols: u16,
        cw: f64,
        ch: f64,
        dpr: f64,
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
                self.paint_cell(ctx, screen, row, col, cw, ch, prows, pcols, &mut font);
            }
        }

        // Cursor: background then text (hidden when scrolled into history)
        let cur = self.visible_cursor(screen, rows, cols);
        let cursor_pos = self.draw_cursor(ctx, cur, cw, ch)?;
        self.prev_cursor = cursor_pos;
        Ok(())
    }

    /// Redraw only cells that changed since the last render, plus the cursor.
    fn render_dirty_cells(
        &mut self,
        ctx: JsHandle,
        rows: u16,
        cols: u16,
        cw: f64,
        ch: f64,
        dpr: f64,
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
            self.paint_cell(ctx, screen, row, col, cw, ch, prows, pcols, &mut font);
        }

        // Cursor drawn last, on top of the regular cell content.
        let cur = self.visible_cursor(screen, rows, cols);
        let cursor_pos = self.draw_cursor(ctx, cur, cw, ch)?;
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
        if (cr as u16) < rows && (cc as u16) < cols {
            Some((cr as u16, cc as u16))
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
    fn draw_cursor(
        &mut self,
        ctx: JsHandle,
        cursor: Option<(u16, u16)>,
        cw: f64,
        ch: f64,
    ) -> Result<Option<(u16, u16)>, String> {
        let Some((cr, cc)) = cursor else { return Ok(None) };
        let screen = self.parser.screen();
        let cell = screen.cell(cr, cc);
        let (mut fg, mut bg) = if let Some(c) = cell {
            (
                cell_fg_rgb(&c, DEFAULT_FG),
                color_to_rgb(c.bgcolor(), DEFAULT_BG),
            )
        } else {
            (DEFAULT_FG, DEFAULT_BG)
        };
        std::mem::swap(&mut fg, &mut bg);
        ffi::ctx_set_fill_style(ctx, &css_color(bg));
        ffi::ctx_fill_rect(ctx, cc as f64 * cw, cr as f64 * ch, cw, ch);
        ffi::ctx_set_fill_style(ctx, &css_color(fg));
        if let Some(c) = cell {
            let s = c.contents();
            if !s.is_empty() {
                if !draw_graphic_cell(ctx, cc, cr, cw, ch, s, fg) {
                    ffi::ctx_fill_text(ctx, s, cc as f64 * cw, cr as f64 * ch + ch * 0.5);
                }
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
    ) {
        // 1. Clear cell to default background
        ffi::ctx_set_fill_style(ctx, &css_color(DEFAULT_BG));
        ffi::ctx_fill_rect(ctx, col as f64 * cw, row as f64 * ch, cw, ch);

        let cell = if row < prows && col < pcols {
            screen.cell(row, col)
        } else {
            None
        };
        let selected = self.selected(row, col);

        // 2. Non-default background rect
        let bg = match cell {
            Some(c) => color_to_rgb(c.bgcolor(), DEFAULT_BG),
            _ => DEFAULT_BG,
        };
        if bg != DEFAULT_BG && !selected {
            ffi::ctx_set_fill_style(ctx, &css_color(bg));
            ffi::ctx_fill_rect(ctx, col as f64 * cw, row as f64 * ch, cw, ch);
        }

        // 3. Selection background rect (before text)
        if selected {
            let (fg0, bg0) = match cell {
                Some(c) => (
                    cell_fg_rgb(&c, DEFAULT_FG),
                    color_to_rgb(c.bgcolor(), DEFAULT_BG),
                ),
                _ => (DEFAULT_FG, DEFAULT_BG),
            };
            let (mut fg, mut bg) = (fg0, bg0);
            std::mem::swap(&mut fg, &mut bg);
            ffi::ctx_set_fill_style(ctx, &css_color(bg));
            ffi::ctx_fill_rect(
                ctx,
                col as f64 * cw - CELL_EPSILON,
                row as f64 * ch - CELL_EPSILON,
                cw + CELL_EPSILON * 2.0,
                ch + CELL_EPSILON * 2.0,
            );
        }

        // 4. Text glyph
        if let Some(c) = cell {
            let s = c.contents();
            if !s.is_empty() {
                const SELECTION_FG: u32 = 0x000000;
                let fg = cell_fg_rgb(&c, DEFAULT_FG);
                let draw_fg = if selected { SELECTION_FG } else { fg };
                if draw_fg != DEFAULT_BG {
                    if draw_graphic_cell(ctx, col, row, cw, ch, s, draw_fg) {
                        return;
                    }
                    let want = if c.bold() { FONT_STACK_BOLD } else { FONT_STACK };
                    if font.as_str() != want {
                        *font = want.to_string();
                        ffi::ctx_set_font(ctx, font);
                    }
                    ffi::ctx_set_fill_style(ctx, &css_color(draw_fg));
                    ffi::ctx_fill_text(
                        ctx,
                        s,
                        col as f64 * cw,
                        row as f64 * ch + ch * 0.5,
                    );
                }
            }
        }
    }

    /// Trigger resize callback
    pub(crate) fn trigger_resize(&mut self, _new_rows: u16, _new_cols: u16) {
    }

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

    /// Set measured cell dimensions (used on resize).
    pub(crate) fn set_cell_dims(&mut self, cw: f64, ch: f64) {
        self.cell_width = cw;
        self.cell_height = ch;
    }

    /// Set parsed terminal dimensions (used on resize).
    pub(crate) fn set_dims(&mut self, rows: u16, cols: u16) {
        self.rows = rows;
        self.cols = cols;
    }

    /// Resize the underlying parser screen to `rows` x `cols`.
    pub(crate) fn resize_screen(&mut self, rows: u16, cols: u16) {
        let _ = self.parser.screen_mut().set_size(rows, cols);
    }
}

thread_local! {
    /// Global terminal state, initialized once by [`crate::init`]
    pub(crate) static TERM_STATE: RefCell<Option<TerminalState>> =
        const { RefCell::new(None) };
}
