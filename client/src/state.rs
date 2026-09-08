// Client terminal state.
//
// Holds the vt100 parser, the active renderer (Canvas 2D default, WebGL2
// fallback), cell dimensions, and selection state. Exposes the methods the
// WASM exports mutate it through.

use js_sys::Function;
use std::cell::RefCell;
use vt100::Parser;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::CanvasRenderingContext2d;

use crate::color::{cell_fg_rgb, color_to_rgb, DEFAULT_BG, DEFAULT_FG};
use crate::graphics::draw_graphic_cell;
use crate::measure::{
    css_color, measure_cell_dimensions_scratch, CELL_EPSILON, FONT_STACK, FONT_STACK_BOLD,
};
use crate::renderer;
use crate::selection::SelectionMode;

pub(crate) const DEFAULT_ROWS: u16 = 24;
pub(crate) const DEFAULT_COLS: u16 = 80;
pub(crate) const SCROLLBACK_LEN: usize = 1024;

/// Terminal state parsed from ANSI byte streams, drawn to the canvas via the
/// active renderer (Canvas 2D by default, WebGL2 fallback).
pub(crate) struct TerminalState {
    /// vt100 parser
    parser: Parser,
    /// 2D rendering context (Canvas 2D default path)
    ctx: Option<CanvasRenderingContext2d>,
    /// WebGL2 renderer (fallback; text pass not yet rendering glyphs)
    webgl: Option<renderer::WebGL2Renderer>,
    /// Canvas element (source of pixel dimensions)
    canvas: web_sys::HtmlCanvasElement,
    /// Canvas element ID
    canvas_id: String,
    /// Terminal dimensions in cells
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// Measured cell width in CSS pixels
    pub(crate) cell_width: f64,
    /// Measured cell height in CSS pixels
    pub(crate) cell_height: f64,
    /// Resize callback
    on_resize: Option<Box<dyn FnMut(u16, u16) + 'static>>,
    /// Selection mode
    selection_mode: SelectionMode,
    /// Text selection start cell
    selection_start: Option<(u16, u16)>,
    /// Text selection end cell
    selection_end: Option<(u16, u16)>,
}

impl TerminalState {
    /// Create a new terminal state with a Canvas 2D rendering context
    ///
    /// # Parameters
    /// * `canvas_id` - HTML canvas element ID
    /// * `on_resize` - JS callback called with (rows, cols) when the terminal resizes
    pub(crate) fn new(
        canvas_id: &str,
        on_resize: Option<Function>,
    ) -> Result<Self, String> {
        let parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);

        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(canvas_id))
            .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .ok_or_else(|| format!("canvas '#{}' not found", canvas_id))?;

        // Try Canvas 2D first; fall back to WebGL2
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0)
            .max(1.0);

        // Measure cell dims via a scratch canvas so the real terminal canvas is
        // never given a context before the primary renderer is chosen (a canvas
        // only supports one context type).
        let (cell_width, cell_height) = measure_cell_dimensions_scratch()
            .unwrap_or((14.0, 20.0));

        // Canvas 2D primary path. Text rendering is currently unreliable under
        // WebGL2 (glyphs missing in practice), so Canvas 2D is the default until
        // the WebGL2 text path is fixed.
        let mut ctx = None;
        let mut try_webgl = true;
        if let Ok(c) = canvas
            .get_context("2d")
            .map_err(|_| ())
            .and_then(|c| c.ok_or(()))
            .and_then(|c| c.dyn_into::<CanvasRenderingContext2d>().map_err(|_| ()))
        {
            ctx = Some(c);
            try_webgl = false;
        }

        // WebGL2 fallback: only when a 2D context could not be obtained.
        let mut webgl = None;
        if try_webgl {
            if let Ok(w) = renderer::WebGL2Renderer::new(
                canvas_id,
                cell_width,
                cell_height,
                DEFAULT_ROWS,
                DEFAULT_COLS,
                dpr,
                renderer::EMBEDDED_FONT,
            ) {
                webgl = Some(w);
            }
        }

        if ctx.is_none() && webgl.is_none() {
            return Err("no rendering context available (2D failed and WebGL2 unavailable)".to_string());
        }

        let canvas_w = canvas.offset_width() as f64;
        let canvas_h = canvas.offset_height() as f64;
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

        let on_resize = on_resize.map(|f| {
            Box::new(move |rows: u16, cols: u16| {
                let _ = f.call2(
                    &JsValue::NULL,
                    &JsValue::from(rows),
                    &JsValue::from(cols),
                );
            }) as Box<dyn FnMut(u16, u16)>
        });

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
            on_resize,
            selection_mode: SelectionMode::None,
            selection_start: None,
            selection_end: None,
        })
    }

    /// Process incoming ANSI bytes through the VT100 parser
    pub(crate) fn process_bytes(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
    }

    /// Render the current parser screen.
    ///
    /// Dispatches to the active renderer: Canvas 2D by default, WebGL2
    /// when it was selected as the fallback.
    pub(crate) fn render(&mut self) -> Result<(), String> {
        if let Some(w) = self.webgl.as_ref() {
            let screen = self.parser.screen();
            let (cr, cc) = screen.cursor_position();
            let selection = self.selection_cells();
            return w.render(
                screen,
                DEFAULT_FG,
                DEFAULT_BG,
                &selection,
                (cr, cc),
            );
        }
        self.render_canvas2d()
    }

    /// Build the list of selected cells in the active selection rectangle.
    fn selection_cells(&self) -> Vec<(u16, u16)> {
        let (Some(a), Some(b)) = (self.selection_start, self.selection_end) else {
            return Vec::new();
        };
        let (a_r, a_c) = (a.0.min(b.0), a.1.min(b.1));
        let (b_r, b_c) = (a.0.max(b.0), a.1.max(b.1));
        let mut cells = Vec::new();
        for row in a_r..=b_r {
            for col in a_c..=b_c {
                cells.push((row, col));
            }
        }
        cells
    }

    /// Draw the current parser screen with native Canvas 2D text.
    ///
    /// Each visible cell is painted in CSS pixels on the DPR-scaled canvas:
    /// Rendering order ensures crisp text with visible selection highlight:
    /// 1. Clear whole canvas to default background
    /// 2. Fill background rects for cells with non-default background
    /// 3. Draw all glyph text (foreground color)
    /// 4. Fill selection background rects (swapped colors) on top of text
    /// 5. Draw cursor background and text on top
    fn render_canvas2d(&mut self) -> Result<(), String> {
        let ctx = self.ctx.as_ref().ok_or("no renderer")?;
        let screen = self.parser.screen();
        let (prows, pcols) = screen.size();
        let rows = if self.rows > 0 { self.rows } else { prows };
        let cols = if self.cols > 0 { self.cols } else { pcols };
        let cw = self.cell_width;
        let ch = self.cell_height;

        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0);
        let css_w = self.canvas.width() as f64 / dpr;
        let css_h = self.canvas.height() as f64 / dpr;
        let _ = ctx.set_transform(dpr.max(1.0), 0.0, 0.0, dpr.max(1.0), 0.0, 0.0);

        // 1. Clear to default background
        ctx.set_fill_style_str(&css_color(DEFAULT_BG));
        ctx.fill_rect(0.0, 0.0, css_w.max(1.0), css_h.max(1.0));
        ctx.set_text_baseline("middle");

        let mut font = FONT_STACK.to_string();
        ctx.set_font(&font);

        // 2. Background rects for cells with non-default background
        for row in 0..rows {
            for col in 0..cols {
                let bg = if row < prows && col < pcols {
                    match screen.cell(row, col) {
                        Some(c) => color_to_rgb(c.bgcolor(), DEFAULT_BG),
                        _ => DEFAULT_BG,
                    }
                } else {
                    DEFAULT_BG
                };
                if bg != DEFAULT_BG && !self.selected(row, col) {
                    ctx.set_fill_style_str(&css_color(bg));
                    ctx.fill_rect(
                        col as f64 * cw,
                        row as f64 * ch,
                        cw,
                        ch,
                    );
                }
            }
        }

        // 3. Draw selection background rects BEFORE text
        for row in 0..rows {
            for col in 0..cols {
                if self.selected(row, col) {
                    let (fg0, bg0) = if row < prows && col < pcols {
                        match screen.cell(row, col) {
                            Some(c) => (
                                cell_fg_rgb(&c, DEFAULT_FG),
                                color_to_rgb(c.bgcolor(), DEFAULT_BG),
                            ),
                            _ => (DEFAULT_FG, DEFAULT_BG),
                        }
                    } else {
                        (DEFAULT_FG, DEFAULT_BG)
                    };
                    let (mut fg, mut bg) = (fg0, bg0);
                    std::mem::swap(&mut fg, &mut bg);
                    ctx.set_fill_style_str(&css_color(bg));
                    ctx.fill_rect(
                        col as f64 * cw - CELL_EPSILON,
                        row as f64 * ch - CELL_EPSILON,
                        cw + CELL_EPSILON * 2.0,
                        ch + CELL_EPSILON * 2.0,
                    );
                }
            }
        }

        // 4. Draw all text on top of selection
        const SELECTION_FG: u32 = 0x000000;
        for row in 0..rows {
            for col in 0..cols {
                let (fg, text) = if row < prows && col < pcols {
                    match screen.cell(row, col) {
                        Some(c) if !c.contents().is_empty() => (
                            cell_fg_rgb(&c, DEFAULT_FG),
                            Some((c.contents().to_string(), c.bold())),
                        ),
                        _ => (DEFAULT_FG, None),
                    }
                } else {
                    (DEFAULT_FG, None)
                };
                if let Some((s, bold)) = text {
                    let draw_fg = if self.selected(row, col) { SELECTION_FG } else { fg };
                    if draw_fg != DEFAULT_BG {
                        if draw_graphic_cell(ctx, col, row, cw, ch, &s, draw_fg) {
                            continue;
                        }
                        let want = if bold { FONT_STACK_BOLD } else { FONT_STACK };
                        if font != want {
                            font = want.to_string();
                            ctx.set_font(&font);
                        }
                        ctx.set_fill_style_str(&css_color(draw_fg));
                        let _ = ctx.fill_text(
                            &s,
                            col as f64 * cw,
                            row as f64 * ch + ch * 0.5,
                        );
                    }
                }
            }
        }

        // 5. Cursor: background then text
        let (cr, cc) = screen.cursor_position();
        if (cr as u16) < rows && (cc as u16) < cols {
            let cell = screen.cell(cr as u16, cc as u16);
            let (mut fg, mut bg) = if let Some(c) = cell {
                (
                    cell_fg_rgb(&c, DEFAULT_FG),
                    color_to_rgb(c.bgcolor(), DEFAULT_BG),
                )
            } else {
                (DEFAULT_FG, DEFAULT_BG)
            };
            std::mem::swap(&mut fg, &mut bg);
            ctx.set_fill_style_str(&css_color(bg));
            ctx.fill_rect(cc as f64 * cw, cr as f64 * ch, cw, ch);
            ctx.set_fill_style_str(&css_color(fg));
            if let Some(c) = cell {
                let s = c.contents();
                if !s.is_empty() {
                    if !draw_graphic_cell(ctx, cc as u16, cr as u16, cw, ch, &s, fg) {
                        let _ = ctx.fill_text(s, cc as f64 * cw, cr as f64 * ch + ch * 0.5);
                    }
                }
            }
        }
        Ok(())
    }

    /// Whether the given cell lies inside the active selection rectangle
    /// (normalized so the anchor can be above/below the current end).
    fn selected(&self, row: u16, col: u16) -> bool {
        let (Some(a), Some(b)) = (self.selection_start, self.selection_end) else {
            return false;
        };
        let (a_r, a_c) = (a.0.min(b.0), a.1.min(b.1));
        let (b_r, b_c) = (a.0.max(b.0), a.1.max(b.1));
        (a_r..=b_r).contains(&row) && (a_c..=b_c).contains(&col)
    }

    /// Trigger resize callback
    pub(crate) fn trigger_resize(&mut self, new_rows: u16, new_cols: u16) {
        if let Some(ref mut cb) = self.on_resize {
            cb(new_rows, new_cols);
        }
    }

    /// Handle selection start
    pub(crate) fn handle_selection_start(&mut self, row: u16, col: u16) {
        self.selection_mode = SelectionMode::Linear;
        self.selection_start = Some((row, col));
        self.selection_end = None;
    }

    /// Handle selection update
    pub(crate) fn handle_selection_update(&mut self, row: u16, col: u16) {
        if let Some(ref mut end) = self.selection_end {
            *end = (row, col);
        } else if let Some(ref _start) = self.selection_start {
            self.selection_end = Some((row, col));
        }
    }

    /// Clear the active selection (reset both anchor and end to None)
    pub(crate) fn clear_selection(&mut self) {
        self.selection_start = None;
        self.selection_end = None;
        self.selection_mode = SelectionMode::None;
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
        self.selection_start
    }

    /// Access the selection end.
    pub(crate) fn selection_end(&self) -> Option<(u16, u16)> {
        self.selection_end
    }

    /// Access the selection mode.
    pub(crate) fn selection_mode(&self) -> SelectionMode {
        self.selection_mode
    }

    /// Mutable access to the WebGL renderer for resize handling.
    pub(crate) fn webgl_mut(&mut self) -> Option<&mut renderer::WebGL2Renderer> {
        self.webgl.as_mut()
    }

    /// Mutable access to the canvas.
    pub(crate) fn canvas_mut(&mut self) -> &mut web_sys::HtmlCanvasElement {
        &mut self.canvas
    }

    /// Whether the active renderer is the Canvas 2D path.
    pub(crate) fn ctx(&self) -> Option<&CanvasRenderingContext2d> {
        self.ctx.as_ref()
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
