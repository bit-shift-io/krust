// client-wasm terminal module
// WASM terminal integration Phase 0-3
//
// Provides the Rust/WASM terminal backend:
// - VT100 parser for ANSI escape sequences
// - beamterm-renderer for WebGL2 rendering
// - WebSocket binary message pipeline from backend
// - Selection overlay support
// - Keyboard input pipeline
// - Resize handling
// - WebGL2 fallback detection
// - Error boundaries & panic handling

#![warn(missing_docs)]

use js_sys::Function;
use vt100::{Color, Parser};
use wasm_bindgen::prelude::*;
use wasm_bindgen::{JsCast, JsValue};
use web_sys::CanvasRenderingContext2d;

// -- Constants --

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_CELL_WIDTH: i32 = 9;
const DEFAULT_CELL_HEIGHT: i32 = 18;
const SCROLLBACK_LEN: usize = 1024;
const MAX_HISTORY_BYTES: usize = 1024 * 512;

/// Default foreground color (light gray)
const DEFAULT_FG: u32 = 0xf0f0f0;
/// Default background color (dark gray, matching the old krust theme)
const DEFAULT_BG: u32 = 0x2b2b2b;

/// CSS font stack used by the canvas-2D glyph renderer (native vector text).
const FONT_STACK: &str =
    "12px/18px 'DejaVu Sans Mono', Menlo, Consolas, 'Liberation Mono', monospace";
/// Bold variant of [`FONT_STACK`].
const FONT_STACK_BOLD: &str =
    "bold 12px/18px 'DejaVu Sans Mono', Menlo, Consolas, 'Liberation Mono', monospace";

/// Convert an ANSI/VT100 index (0-255) to its xterm-256 RGB value.
fn xterm_palette(idx: u8) -> u32 {
    match idx {
        0..=15 => {
            const ANSI: [u32; 16] = [
                0x000000, 0x800000, 0x008000, 0x808000, 0x000080, 0x800080, 0x008080, 0xC0C0C0,
                0x808080, 0xFF0000, 0x00FF00, 0xFFFF00, 0x0000FF, 0xFF00FF, 0x00FFFF, 0xFFFFFF,
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
fn color_to_rgb(color: Color, default: u32) -> u32 {
    match color {
        Color::Default => default,
        Color::Idx(i) => xterm_palette(i),
        Color::Rgb(r, g, b) => ((r as u32) << 16) | ((g as u32) << 8) | b as u32,
    }
}

/// Format a `0xRRGGBB` integer as an HTML/CSS `#rrggbb` string.
fn css_color(rgb: u32) -> String {
    format!("#{:06x}", rgb)
}

/// Encode the standard xterm modifier parameter (1 + shift 1 + alt 2 + ctrl 4).
///
/// Returns `None` when no application modifiers are active.
fn xterm_modifier_param(ctrl: bool, alt: bool, shift: bool) -> Option<u8> {
    let mut param = 1u8;
    let mut any = false;
    if shift {
        param += 1;
        any = true;
    }
    if alt {
        param += 2;
        any = true;
    }
    if ctrl {
        param += 4;
        any = true;
    }
    any.then_some(param)
}

/// Whether a character is directly typeable into a PTY (graphic or space).
fn is_printable_ascii(c: char) -> bool {
    c.is_ascii_graphic() || c == ' '
}

/// Map a browser keyboard event to the raw bytes to write to the PTY.
///
/// Follows the mapping table in `NOTES.md`: Ctrl+letter → control code,
/// arrows → CSI sequences, F-keys → `CSI N~`, Alt+char → `ESC char`.
/// Local echo is disabled by the backend, so nothing is echoed here.
fn map_key(key: &str, ctrl: bool, alt: bool, shift: bool, _meta: bool) -> Vec<u8> {
    let single_char = if key.chars().count() == 1 {
        key.chars().next()
    } else {
        None
    };

    // Ctrl + letter → control character (Ctrl+C = \x03, etc.)
    if let Some(c) = single_char {
        if ctrl && c.is_ascii_alphabetic() {
            return vec![c.to_ascii_lowercase() as u8 - b'a' + 1];
        }
    }

    // Ctrl + punctuation/space control codes.
    if ctrl {
        let code = match key {
            "Space" => Some(0x00),
            "[" => Some(0x1b),
            "\\" => Some(0x1c),
            "]" => Some(0x1d),
            "^" => Some(0x1e),
            "_" => Some(0x1f),
            "Backspace" => Some(0x08),
            _ => None,
        };
        if let Some(c) = code {
            return vec![c];
        }
    }

    match key {
        "Enter" => {
            if shift {
                return vec![0x1b, 0x0d];
            }
            if let Some(m) = xterm_modifier_param(ctrl, alt, shift) {
                return format!("\x1b[13;{}u", m).into_bytes();
            }
            return vec![0x0d];
        }
        "Tab" => {
            if shift {
                return b"\x1b[Z".to_vec();
            }
            return vec![0x09];
        }
        "Escape" => return vec![0x1b],
        "Backspace" => return vec![0x7f],
        "Delete" => return b"\x1b[3~".to_vec(),
        "Insert" => return b"\x1b[2~".to_vec(),
        "Home" => return b"\x1b[H".to_vec(),
        "End" => return b"\x1b[F".to_vec(),
        "PageUp" => return b"\x1b[5~".to_vec(),
        "PageDown" => return b"\x1b[6~".to_vec(),
        "ArrowUp" => return b"\x1b[A".to_vec(),
        "ArrowDown" => return b"\x1b[B".to_vec(),
        "ArrowRight" => return b"\x1b[C".to_vec(),
        "ArrowLeft" => return b"\x1b[D".to_vec(),
        _ => {}
    }

    let f_tail = match key {
        "F1" => Some(11),
        "F2" => Some(12),
        "F3" => Some(13),
        "F4" => Some(14),
        "F5" => Some(15),
        "F6" => Some(17),
        "F7" => Some(18),
        "F8" => Some(19),
        "F9" => Some(20),
        "F10" => Some(21),
        "F11" => Some(23),
        "F12" => Some(24),
        _ => None,
    };
    if let Some(t) = f_tail {
        return format!("\x1b[{}~", t).into_bytes();
    }

    // Alt + printable → ESC prefix.
    if alt {
        if let Some(c) = single_char {
            if is_printable_ascii(c) {
                let mut buf = [0u8; 4];
                let mut out = Vec::with_capacity(5);
                out.push(0x1b);
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                return out;
            }
        }
        return Vec::new();
    }

    // Plain single printable character passes through.
    if let Some(c) = single_char {
        if is_printable_ascii(c) {
            let mut buf = [0u8; 4];
            return c.encode_utf8(&mut buf).as_bytes().to_vec();
        }
    }

    Vec::new()
}

/// Extract the text between two grid coordinates.
///
/// The coordinates are normalized (anchor first) so selection direction does
/// not matter. Returns a newly-allocated string.
fn extract_selection(screen: &vt100::Screen, start: (u16, u16), end: (u16, u16)) -> String {
    let (start_row, start_col) = start;
    let (end_row, end_col) = end;
    let (a_r, a_c, b_r, b_c) = if (start_row, start_col) <= (end_row, end_col) {
        (start_row, start_col, end_row, end_col)
    } else {
        (end_row, end_col, start_row, start_col)
    };
    screen.contents_between(a_r, a_c, b_r, b_c)
}

// -- Terminal State --

/// Terminal state parsed from ANSI byte streams, drawn with native Canvas 2D text.
struct TerminalState {
    /// vt100 parser
    parser: Parser,
    /// 2D rendering context
    ctx: CanvasRenderingContext2d,
    /// Canvas element (source of pixel dimensions)
    canvas: web_sys::HtmlCanvasElement,
    /// Canvas element ID
    canvas_id: String,
    /// Terminal dimensions in cells
    rows: u16,
    cols: u16,
    /// Resize callback
    on_resize: Option<Box<dyn FnMut(u16, u16) + 'static>>,
    /// Selection mode
    selection_mode: SelectionMode,
    /// Text selection start cell
    selection_start: Option<(u16, u16)>,
    /// Text selection end cell
    selection_end: Option<(u16, u16)>,
    /// WebGL2 context availability flag
    webgl2_available: bool,
    /// Fallback state flag
    fallback_mode: bool,
}

/// Selection mode for text selection
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SelectionMode {
    /// No selection active
    None,
    /// Linear (text-flow) selection
    Linear,
    /// Rectangular (block) selection
    Block,
}

impl TerminalState {
    /// Create a new terminal state with a Canvas 2D rendering context
    ///
    /// # Parameters
    /// * `canvas_id` - HTML canvas element ID
    /// * `on_resize` - JS callback called with (rows, cols) when the terminal resizes
    pub fn new(
        canvas_id: &str,
        on_resize: Option<Function>,
    ) -> Result<Self, String> {
        let parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);

        let webgl2_available = Self::detect_webgl2()?;
        let fallback_mode = !webgl2_available;

        let canvas = web_sys::window()
            .and_then(|w| w.document())
            .and_then(|d| d.get_element_by_id(canvas_id))
            .and_then(|el| el.dyn_into::<web_sys::HtmlCanvasElement>().ok())
            .ok_or_else(|| format!("canvas '#{}' not found", canvas_id))?;
        let ctx = canvas
            .get_context("2d")
            .map_err(|e| format!("get_context(2d): {:?}", e))?
            .ok_or_else(|| "2D context unavailable".to_string())?
            .dyn_into::<web_sys::CanvasRenderingContext2d>()
            .map_err(|_| "2D context cast failed".to_string())?;

        let canvas_w = canvas.width() as f64;
        let canvas_h = canvas.height() as f64;
        let cols = if canvas_w > 0.0 {
            (canvas_w / DEFAULT_CELL_WIDTH as f64).floor() as u16
        } else {
            DEFAULT_COLS
        };
        let rows = if canvas_h > 0.0 {
            (canvas_h / DEFAULT_CELL_HEIGHT as f64).floor() as u16
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
            canvas,
            canvas_id: canvas_id.to_string(),
            rows,
            cols,
            on_resize,
            selection_mode: SelectionMode::None,
            selection_start: None,
            selection_end: None,
            webgl2_available,
            fallback_mode,
        })
    }

    /// Detect WebGL2 availability
    fn detect_webgl2() -> Result<bool, String> {
        // In a full WASM implementation, this would check the WebGL2 context
        // For this stub, we return true (assuming WebGL2 is available)
        // In a real implementation, this would use web_sys::WebGl2RenderingContext::is_webgl2
        Ok(true)
    }

    /// Process incoming ANSI bytes through the VT100 parser
    pub fn process_bytes(&mut self, bytes: &[u8]) {
        self.parser.process(bytes);
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
    pub fn render(&mut self) -> Result<(), String> {
        let screen = self.parser.screen();
        let (prows, pcols) = screen.size();
        let rows = if self.rows > 0 { self.rows } else { prows };
        let cols = if self.cols > 0 { self.cols } else { pcols };
        let cw = DEFAULT_CELL_WIDTH as f64;
        let ch = DEFAULT_CELL_HEIGHT as f64;

        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0);
        let css_w = self.canvas.width() as f64 / dpr;
        let css_h = self.canvas.height() as f64 / dpr;
        let _ = self.ctx.set_transform(dpr.max(1.0), 0.0, 0.0, dpr.max(1.0), 0.0, 0.0);

        // 1. Clear to default background
        self.ctx.set_fill_style_str(&css_color(DEFAULT_BG));
        self.ctx.fill_rect(0.0, 0.0, css_w.max(1.0), css_h.max(1.0));
        self.ctx.set_text_baseline("middle");

        let mut font = FONT_STACK.to_string();
        self.ctx.set_font(&font);

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
                    self.ctx.set_fill_style_str(&css_color(bg));
                    self.ctx.fill_rect(
                        col as f64 * cw,
                        row as f64 * ch,
                        cw,
                        ch,
                    );
                }
            }
        }

        // 3. Draw all text
        for row in 0..rows {
            for col in 0..cols {
                let (fg, text) = if row < prows && col < pcols {
                    match screen.cell(row, col) {
                        Some(c) if !c.contents().is_empty() => (
                            color_to_rgb(c.fgcolor(), DEFAULT_FG),
                            Some((c.contents().to_string(), c.bold())),
                        ),
                        _ => (DEFAULT_FG, None),
                    }
                } else {
                    (DEFAULT_FG, None)
                };
                if let Some((s, bold)) = text {
                    if fg != DEFAULT_BG {
                        let want = if bold { FONT_STACK_BOLD } else { FONT_STACK };
                        if font != want {
                            font = want.to_string();
                            self.ctx.set_font(&font);
                        }
                        self.ctx.set_fill_style_str(&css_color(fg));
                        let _ = self.ctx.fill_text(
                            &s,
                            col as f64 * cw + cw * 0.5,
                            row as f64 * ch + ch * 0.5,
                        );
                    }
                }
            }
        }

        // 4. Selection background rects (swapped colors) on top of text
        for row in 0..rows {
            for col in 0..cols {
                if self.selected(row, col) {
                    let (fg0, bg0) = if row < prows && col < pcols {
                        match screen.cell(row, col) {
                            Some(c) => (
                                color_to_rgb(c.fgcolor(), DEFAULT_FG),
                                color_to_rgb(c.bgcolor(), DEFAULT_BG),
                            ),
                            _ => (DEFAULT_FG, DEFAULT_BG),
                        }
                    } else {
                        (DEFAULT_FG, DEFAULT_BG)
                    };
                    let (mut fg, mut bg) = (fg0, bg0);
                    std::mem::swap(&mut fg, &mut bg);
                    self.ctx.set_fill_style_str(&css_color(bg));
                    self.ctx.fill_rect(
                        col as f64 * cw,
                        row as f64 * ch,
                        cw,
                        ch,
                    );
                }
            }
        }

        // 5. Cursor: background then text
        let (cr, cc) = screen.cursor_position();
        if (cr as u16) < rows && (cc as u16) < cols {
            let cell = screen.cell(cr as u16, cc as u16);
            let (mut fg, mut bg) = if let Some(c) = cell {
                (
                    color_to_rgb(c.fgcolor(), DEFAULT_FG),
                    color_to_rgb(c.bgcolor(), DEFAULT_BG),
                )
            } else {
                (DEFAULT_FG, DEFAULT_BG)
            };
            std::mem::swap(&mut fg, &mut bg);
            self.ctx.set_fill_style_str(&css_color(bg));
            self.ctx.fill_rect(cc as f64 * cw, cr as f64 * ch, cw, ch);
            self.ctx.set_fill_style_str(&css_color(fg));
            if let Some(c) = cell {
                let s = c.contents();
                if !s.is_empty() {
                    let _ = self.ctx.fill_text(s, cc as f64 * cw + cw * 0.5, cr as f64 * ch + ch * 0.5);
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

    /// Set resize callback
    pub fn set_on_resize(&mut self, callback: Box<dyn FnMut(u16, u16) + 'static>) {
        self.on_resize = Some(callback);
    }

    /// Trigger resize callback
    fn trigger_resize(&mut self, new_rows: u16, new_cols: u16) {
        if let Some(ref mut cb) = self.on_resize {
            cb(new_rows, new_cols);
        }
    }

    /// Handle selection start
    pub fn handle_selection_start(&mut self, row: u16, col: u16) {
        self.selection_mode = SelectionMode::Linear;
        self.selection_start = Some((row, col));
        self.selection_end = None;
    }

    /// Handle selection update
    pub fn handle_selection_update(&mut self, row: u16, col: u16) {
        if let Some(ref mut end) = self.selection_end {
            *end = (row, col);
        } else if let Some(ref _start) = self.selection_start {
            self.selection_end = Some((row, col));
        }
    }

    /// Handle selection end
    pub fn handle_selection_end(&mut self) {
        self.selection_mode = SelectionMode::None;
        self.selection_start = None;
        self.selection_end = None;
    }

    /// Get the canvas ID
    pub fn canvas_id(&self) -> &str {
        &self.canvas_id
    }

    /// Get the current terminal dimensions
    pub fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Get the selection mode
    pub fn selection_mode(&self) -> SelectionMode {
        self.selection_mode
    }

    /// Get whether WebGL2 is available
    pub fn is_webgl2_available(&self) -> bool {
        self.webgl2_available
    }

    /// Whether the terminal is in fallback mode
    pub fn is_fallback_mode(&self) -> bool {
        self.fallback_mode
    }

    /// Get whether text can be selected (no IME composition active)
    pub fn can_select(&self) -> bool {
        true
    }
}

// -- Global Terminal State --

thread_local! {
    /// Global terminal state, initialized once by [`init`]
    static TERM_STATE: std::cell::RefCell<Option<TerminalState>> =
        const { std::cell::RefCell::new(None) };
}

// -- Public API Exports --

/// Initialize the terminal module and receive terminal config JSON
///
/// # Parameters
/// * `canvas_id` - HTML canvas element ID (e.g., "terminal-canvas")
/// * `on_resize` - JS function to call on terminal resize (rows, cols)
///
/// Returns a JSON string describing the terminal state for JS setup,
/// including WebGL2 availability and fallback mode status.
#[wasm_bindgen]
pub fn init(canvas_id: &str, on_resize: &JsValue) -> Result<String, JsValue> {
    if TERM_STATE.with(|s| s.borrow().is_some()) {
        return Err(JsValue::from("terminal already initialized"));
    }

    let on_resize_fn = if on_resize.is_null() || on_resize.is_undefined() {
        None
    } else {
        Some(Function::from(on_resize.clone()))
    };
    let term_state = TerminalState::new(canvas_id, on_resize_fn)
        .map_err(|e| JsValue::from(format!("terminal init failed: {}", e)))?;

    let state_json = serde_json::json!({
        "canvas_id": term_state.canvas_id(),
        "rows": term_state.size().0,
        "cols": term_state.size().1,
        "cell_width": DEFAULT_CELL_WIDTH,
        "cell_height": DEFAULT_CELL_HEIGHT,
        "webgl2_available": term_state.is_webgl2_available(),
        "fallback_mode": term_state.is_fallback_mode(),
    })
    .to_string();

    TERM_STATE.with(|s| *s.borrow_mut() = Some(term_state));
    Ok(state_json)
}

/// Scan a chunk of terminal writing for device-query sequences that demand a
/// response (DA1, DA2, cursor position, OSC-11 background colour). Responding
/// keeps shells like fish from stalling on unanswered queries (`\x1b[c` etc).
///
/// Pure + unit-tested; `row`/`col` are the 1-based cursor position to report
/// for a `\x1b[6n` query.
pub(crate) fn collect_query_replies(bytes: &[u8], row: usize, col: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        // OSC 11 (and any other OSC ) query: ESC ] 11 ; ? ESC \
        if i + 2 < bytes.len() && bytes[i + 1] == b']' {
            let mut k = i + 3;
            while k + 1 < bytes.len() {
                if bytes[k] == 0x07 || (bytes[k] == 0x1b && bytes[k + 1] == b'\\') {
                    break;
                }
                k += 1;
            }
            if k + 1 < bytes.len() && bytes[i..k].starts_with(b"\x1b]11;?") {
                out.extend_from_slice(b"\x1b]11;rgb:2b2b/2b2b/2b2b\x1b\\");
            }
            i = k + 1;
            continue;
        }
        // CSI sequences
        if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut k = i + 2;
            let mut has_greater = false;
            while k < bytes.len()
                && (bytes[k].is_ascii_digit() || matches!(bytes[k], b';' | b'>' | b'?'))
            {
                if bytes[k] == b'>' {
                    has_greater = true;
                }
                k += 1;
            }
            if k >= bytes.len() {
                i += 1;
                continue;
            }
            let params = &bytes[i + 2..k];
            match bytes[k] {
                b'c' if params.is_empty() || params == b"0" => {
                    out.extend_from_slice(b"\x1b[?1;2c");
                }
                b'c' if has_greater => {
                    out.extend_from_slice(b"\x1b[>0;1;0c");
                }
                b'n' if params == b"6" => {
                    out.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                _ => {}
            }
            i = k + 1;
            continue;
        }
        i += 1;
    }
    out
}

/// Process incoming ANSI bytes from the WebSocket
///
/// # Parameters
/// * `bytes` - Bytes received from the WebSocket (raw PTY output)
///
/// Returns a JSON summary of the processed batch.
#[wasm_bindgen]
pub fn process_bytes(bytes: &[u8]) -> Result<String, JsValue> {
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = guard
            .as_mut()
            .ok_or_else(|| JsValue::from("init() not called"))?;
        state.process_bytes(bytes);
        state.render().map_err(|e| JsValue::from(e))?;
        Ok(serde_json::json!({
            "processed": true,
            "byte_count": bytes.len(),
            "rows": state.rows,
            "cols": state.cols,
        })
        .to_string())
    })
}

/// Detect device-query sequences in terminal output and return the replies
/// that should be sent back to the shell (as raw bytes).
#[wasm_bindgen]
pub fn query_replies(bytes: &[u8]) -> Result<Vec<u8>, JsValue> {
    TERM_STATE.with(|cell| {
        let guard = cell.borrow();
        let state = guard
            .as_ref()
            .ok_or_else(|| JsValue::from("init() not called"))?;
        let (row, col) = state.parser.screen().cursor_position();
        Ok(collect_query_replies(bytes, row as usize, col as usize))
    })
}

/// Redraw the terminal immediately from the current parser state.
///
/// Used to surface transient state (e.g. the selection highlight) without
/// waiting for the next batch of shell output.
#[wasm_bindgen]
pub fn repaint() -> Result<(), JsValue> {
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = guard
            .as_mut()
            .ok_or_else(|| JsValue::from("init() not called"))?;
        state.render().map_err(|e| JsValue::from(e))
    })
}

/// Handle window/canvas resize - called from JS with new pixel dimensions
#[wasm_bindgen]
pub fn handle_resize(width: i32, height: i32) -> Result<(), JsValue> {
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = guard
            .as_mut()
            .ok_or_else(|| JsValue::from("init() not called"))?;
        let cols = (width as f64 / DEFAULT_CELL_WIDTH as f64).floor() as u16;
        let rows = (height as f64 / DEFAULT_CELL_HEIGHT as f64).floor() as u16;
        let cols = cols.max(2);
        let rows = rows.max(1);
        let _ = state.parser.screen_mut().set_size(rows, cols);
        state.rows = rows;
        state.cols = cols;
        state.trigger_resize(rows, cols);
        Ok(())
    })
}

/// Get the version info for the terminal module
#[wasm_bindgen]
pub fn version() -> String {
    "krust-terminal 0.3.0".to_string()
}

/// Get the WebGL2 availability status
#[wasm_bindgen]
pub fn is_webgl2_available() -> bool {
    TERM_STATE.with(|cell| match cell.borrow().as_ref() {
        Some(state) => state.webgl2_available,
        None => false,
    })
}

/// Get the fallback mode status
#[wasm_bindgen]
pub fn is_fallback_mode() -> bool {
    TERM_STATE.with(|cell| match cell.borrow().as_ref() {
        Some(state) => state.fallback_mode,
        None => false,
    })
}

/// Get the selection mode
#[wasm_bindgen]
pub fn selection_mode() -> String {
    "None".to_string()
}

/// Get the selected text
///
/// Returns the text between the stored selection coordinates, or an
/// empty string when no selection is active. Coordinates are normalized
/// to forward (anchor-first) order before extraction.
#[wasm_bindgen]
pub fn selected_text() -> String {
    TERM_STATE.with(|cell| {
        let state = cell.borrow();
        match state.as_ref() {
            Some(s) => match (s.selection_start, s.selection_end) {
                (Some(start), Some(end)) => extract_selection(&s.parser.screen(), start, end),
                _ => String::new(),
            },
            None => String::new(),
        }
    })
}

/// Record a text selection between two grid coordinates.
///
/// `start` is the anchor (drag origin), `end` the current cursor cell.
/// The stored selection is later retrievable via [`selected_text`].
#[wasm_bindgen]
pub fn set_selection(start_row: u16, start_col: u16, end_row: u16, end_col: u16) {
    TERM_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.handle_selection_start(start_row, start_col);
            state.handle_selection_update(end_row, end_col);
        }
    });
}

/// Map a browser keyboard event to raw PTY bytes
///
/// The returned bytes are sent to the backend over the binary WebSocket
/// channel and written to the PTY master (raw mode, no local echo).
/// Follows the key mapping table in `NOTES.md`. Use the KeyboardEvent
/// `key` property plus its modifier flags as arguments.
#[wasm_bindgen]
pub fn key_to_bytes(
    key: &str,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
) -> Vec<u8> {
    map_key(key, ctrl, alt, shift, meta)
}

/// WASM entry point: install the console panic hook
#[wasm_bindgen(start)]
pub fn start() {
    console_error_panic_hook::set_once();
}

/// Check if the canvas is in fallback mode (WebGL2 unavailable)
///
/// This is called from JavaScript to determine whether to show
/// the fallback UI or the WebGL2-based terminal.
#[wasm_bindgen]
pub fn check_fallback() -> bool {
    is_fallback_mode()
}

/// Show fallback UI when WebGL2 is unavailable
///
/// Renders a fixed banner element (`#krust-fallback-banner`) with the given
/// message when the terminal is running in fallback mode.
#[wasm_bindgen]
pub fn show_fallback_ui(message: &str) -> bool {
    if !is_fallback_mode() {
        return false;
    }
    let Some(doc) = web_sys::window().and_then(|w| w.document()) else {
        return true;
    };
    if doc.get_element_by_id("krust-fallback-banner").is_none() {
        if let Ok(banner) = doc.create_element("div") {
            let _ = banner.set_attribute("id", "krust-fallback-banner");
            let _ = banner.set_attribute(
                "style",
                "position:fixed;top:0;left:0;right:0;z-index:50;padding:8px 12px;background:#300;color:#f88;font:13px monospace;",
            );
            banner.set_text_content(Some(message));
            if let Some(body) = doc.body() {
                let node: &web_sys::Node = banner.unchecked_ref();
                let _ = body.append_child(node);
            }
        }
    }
    true
}

/// Hide fallback UI and resume normal terminal operation
#[wasm_bindgen]
pub fn hide_fallback_ui() {
    if let Some(doc) = web_sys::window().and_then(|w| w.document()) {
        if let Some(banner) = doc.get_element_by_id("krust-fallback-banner") {
            if let Some(parent) = banner.parent_node() {
                let node: &web_sys::Node = banner.unchecked_ref();
                let _ = parent.remove_child(node);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_replies_da1() {
        assert_eq!(collect_query_replies(b"\x1b[c", 0, 0), b"\x1b[?1;2c");
        assert_eq!(collect_query_replies(b"\x1b[0c", 0, 0), b"\x1b[?1;2c");
    }

    #[test]
    fn query_replies_da2() {
        assert_eq!(collect_query_replies(b"\x1b[>c", 0, 0), b"\x1b[>0;1;0c");
    }

    #[test]
    fn query_replies_cursor_position() {
        assert_eq!(collect_query_replies(b"\x1b[6n", 2, 4), b"\x1b[3;5R");
    }

    #[test]
    fn query_replies_osc11() {
        assert_eq!(
            collect_query_replies(b"\x1b]11;?\x1b\\", 0, 0),
            b"\x1b]11;rgb:2b2b/2b2b/2b2b\x1b\\"
        );
    }

    #[test]
    fn query_replies_pass_through_plain_text() {
        assert_eq!(collect_query_replies(b"plain text\n", 0, 0), b"");
    }

    #[test]
    fn query_replies_multiple_in_one_chunk() {
        assert_eq!(
            collect_query_replies(b"\x1b[c\x1b[6n", 1, 1),
            b"\x1b[?1;2c\x1b[2;2R"
        );
    }

    #[test]
    fn parser_renders_hello_world() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"Hello World");
        let first_row = parser.screen().rows(0, DEFAULT_COLS).next().unwrap();
        assert_eq!(first_row, "Hello World");
    }

    #[test]
    fn parser_strips_ansi_escapes() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"\x1b[31mred\x1b[0m");
        let first_row = parser.screen().rows(0, DEFAULT_COLS).next().unwrap();
        assert_eq!(first_row, "red");
    }

    #[test]
    fn parser_tracks_cursor() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"ab");
        assert_eq!(parser.screen().cursor_position(), (0, 2));
    }

    #[test]
    fn parser_cells_map_to_hello_world() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"Hello World");
        let screen = parser.screen();
        let mut text = String::new();
        for col in 0..11 {
            let cell = screen.cell(0, col).unwrap();
            text.push(cell.contents().chars().next().unwrap());
        }
        assert_eq!(text, "Hello World");
    }

    #[test]
    fn xterm_256_palette_maps_known_colors() {
        assert_eq!(xterm_palette(0), 0x000000);
        assert_eq!(xterm_palette(1), 0x800000);
        assert_eq!(xterm_palette(9), 0xFF0000);
        assert_eq!(xterm_palette(15), 0xFFFFFF);
        assert_eq!(xterm_palette(16), 0x000000);
        assert_eq!(xterm_palette(196), 0xFF0000);
        assert_eq!(xterm_palette(255), 0xEEEEEE);
    }

    #[test]
    fn color_to_rgb_maps_default_and_idx() {
        assert_eq!(color_to_rgb(Color::Default, DEFAULT_FG), DEFAULT_FG);
        assert_eq!(color_to_rgb(Color::Default, DEFAULT_BG), DEFAULT_BG);
        assert_eq!(color_to_rgb(Color::Idx(1), DEFAULT_FG), 0x800000);
        assert_eq!(
            color_to_rgb(Color::Rgb(255, 0, 128), DEFAULT_FG),
            0xFF0080
        );
    }

    #[test]
    fn map_key_ctrl_c_returns_control_c() {
        assert_eq!(map_key("c", true, false, false, false), vec![0x03]);
    }

    #[test]
    fn map_key_ctrl_bracket_returns_escape() {
        assert_eq!(map_key("[", true, false, false, false), vec![0x1b]);
    }

    #[test]
    fn map_key_shift_enter_returns_esc_cr() {
        assert_eq!(map_key("Enter", false, false, true, false), vec![0x1b, 0x0d]);
    }

    #[test]
    fn map_key_arrow_up_returns_escape_bracket_a() {
        assert_eq!(map_key("ArrowUp", false, false, false, false), vec![0x1b, b'[', b'A']);
        assert_eq!(map_key("ArrowDown", false, false, false, false), vec![0x1b, b'[', b'B']);
        assert_eq!(map_key("ArrowRight", false, false, false, false), vec![0x1b, b'[', b'C']);
        assert_eq!(map_key("ArrowLeft", false, false, false, false), vec![0x1b, b'[', b'D']);
    }

    #[test]
    fn map_key_f1_returns_escape_bracket_11_tilde() {
        assert_eq!(map_key("F1", false, false, false, false), b"\x1b[11~".to_vec());
        assert_eq!(map_key("F12", false, false, false, false), b"\x1b[24~".to_vec());
    }

    #[test]
    fn map_key_alt_x_returns_escape_x() {
        assert_eq!(map_key("x", false, true, false, false), vec![0x1b, b'x']);
    }

    #[test]
    fn map_key_plain_printable_passes_through() {
        assert_eq!(map_key("a", false, false, false, false), vec![b'a']);
        assert_eq!(map_key("5", false, false, false, false), vec![b'5']);
    }

    #[test]
    fn map_key_modifier_param_encoding() {
        assert_eq!(xterm_modifier_param(false, false, false), None);
        assert_eq!(xterm_modifier_param(false, false, true), Some(2));
        assert_eq!(xterm_modifier_param(false, true, false), Some(3));
        assert_eq!(xterm_modifier_param(true, false, false), Some(5));
        assert_eq!(xterm_modifier_param(false, true, true), Some(4));
        assert_eq!(xterm_modifier_param(true, false, true), Some(6));
        assert_eq!(xterm_modifier_param(true, true, false), Some(7));
    }

    #[test]
    fn map_key_unknown_returns_empty() {
        assert_eq!(map_key("CapsLock", false, false, false, false), Vec::<u8>::new());
    }

#[test]
    fn extract_selection_reads_cells_in_order() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"hello\nworld");
        let out = extract_selection(parser.screen(), (0, 0), (1, 10));
        assert!(out.contains("hello"));
        assert!(out.contains("world"));
    }

    #[test]
    fn extract_selection_backwards_equals_forward() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"abcdef");
        let screen = parser.screen();
        let forward = extract_selection(&screen, (0, 1), (0, 4));
        let backward = extract_selection(&screen, (0, 4), (0, 1));
        assert_eq!(forward, backward);
        assert_eq!(forward, "bcd");
    }

    #[test]
    fn extract_selection_degenerate_is_empty() {
        let mut parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
        parser.process(b"abcdef");
        let screen = parser.screen();
        assert_eq!(extract_selection(&screen, (0, 2), (0, 2)), "");
    }
}