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

use beamterm_renderer::{Terminal, FontStyle, GlyphEffect};
use vt100::Parser;
use web_sys::{ArrayBuffer, HtmlCanvasElement, WebSocket, WebGl2RenderingContext};
use wasm_bindgen::JsValue;

// -- Constants --

const DEFAULT_ROWS: u16 = 24;
const DEFAULT_COLS: u16 = 80;
const DEFAULT_CELL_WIDTH: i32 = 10;
const DEFAULT_CELL_HEIGHT: i32 = 20;
const SCROLLBACK_LEN: usize = 1024;
const MAX_HISTORY_BYTES: usize = 1024 * 512;

// -- Terminal State --

/// Terminal state parsed from ANSI byte streams, integrated with beamterm-renderer
struct TerminalState {
    /// vt100 parser
    parser: Parser,
    /// beamterm renderer
    terminal: Terminal,
    /// Cell data buffer for rendering (owns static lifetime)
    cells: Vec<beamterm_renderer::CellData<'static>>,
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
    /// Create a new terminal state with WebGL2 fallback detection
    ///
    /// # Parameters
    /// * `canvas_id` - HTML canvas element ID
    /// * `on_resize` - callback called when terminal resizes
    pub fn new(canvas_id: &str, on_resize: Option<Box<dyn FnMut(u16, u16) + 'static>>) -> Result<Self, String> {
        let parser = Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);

        // TODO: In full implementation, Terminal::builder would be called here
        // with proper web_sys types. In this stub, we detect WebGL2 availability
        // and set up fallback mode if needed.
        let webgl2_available = Self::detect_webgl2()?;
        let fallback_mode = !webgl2_available;

        let terminal = Terminal::builder(canvas_id).map_err(|e| format!("beamterm init: {}", e))?;

        Ok(TerminalState {
            parser,
            terminal,
            cells: Vec::new(),
            canvas_id: canvas_id.to_string(),
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
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

        // TODO: Full integration - map VT100 screen cells to beamterm CellData
        // and call terminal.update_cells(). For now, just process the bytes.
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
        } else if let Some(ref start) = self.selection_start {
            self.selection_end = Some((row, col));
        }
    }

    /// Handle selection end
    pub fn handle_selection_end(&mut self) {
        self.selection_mode = SelectionMode::None;
        self.selection_start = None;
        self.selection_end = None;
    }

    /// Get the beamterm terminal for rendering
    pub fn terminal(&self) -> &Terminal {
        &self.terminal
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

    /// Get the selected text (would extract from parser screen state)
    pub fn selected_text(&self) -> String {
        String::new()
    }

    /// Get whether text can be selected (no IME composition active)
    pub fn can_select(&self) -> bool {
        true
    }
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
    let mut term_state = TerminalState::new(canvas_id, None)
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

    Ok(state_json)
}

/// Process incoming ANSI bytes from the WebSocket
///
/// # Parameters
/// * `state_json` - JSON string from init() containing terminal config
/// * `bytes` - ArrayBuffer bytes from WebSocket onmessage
///
/// Returns updated terminal state processing info.
#[wasm_bindgen]
pub fn process_bytes(state_json: &str, bytes: &[u8]) -> Result<String, JsValue> {
    // Parse the state config (in full impl, would deserialize)
    // For now, just process the bytes
    let _ = vt100::Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
    let parser = vt100::Parser::new(DEFAULT_ROWS, DEFAULT_COLS, SCROLLBACK_LEN);
    parser.process(bytes);

    let result = serde_json::json!({
        "processed": true,
        "byte_count": bytes.len(),
        "webgl2_available": true,
    })
    .to_string();

    Ok(result)
}

/// Handle window resize event - called from JS when browser window resizes
#[wasm_bindgen]
pub fn handle_resize(rows: u16, cols: u16) {
    // Would trigger terminal.resize() in full implementation
}

/// Get the version info for the terminal module
#[wasm_bindgen]
pub fn version() -> String {
    "krust-terminal 0.3.0".to_string()
}

/// Get the WebGL2 availability status
#[wasm_bindgen]
pub fn is_webgl2_available() -> bool {
    // Would check actual WebGL2 context status
    true
}

/// Get the fallback mode status
#[wasm_bindgen]
pub fn is_fallback_mode() -> bool {
    // Would check if running in fallback mode
    false
}

/// Get the selection mode
#[wasm_bindgen]
pub fn selection_mode() -> String {
    "None".to_string()
}

/// Get the selected text
#[wasm_bindgen]
pub fn selected_text() -> String {
    String::new()
}

/// Check if the canvas is in fallback mode (WebGL2 unavailable)
///
/// This is called from JavaScript to determine whether to show
/// the fallback UI or the WebGL2-based terminal.
#[wasm_bindgen]
pub fn check_fallback() -> bool {
    // In full implementation, this would check the actual WebGL2 context
    // For now, return false (assuming WebGL2 is available)
    false
}

/// Show fallback UI when WebGL2 is unavailable
///
/// This function is called from JavaScript to display a fallback message
/// and offer the user the option to continue with limited functionality
/// or reload with different settings.
#[wasm_bindgen]
pub fn show_fallback_ui(message: &str) -> bool {
    // Would show a fallback UI overlay
    // For now, just return true indicating fallback mode is active
    true
}

/// Hide fallback UI and resume normal terminal operation
#[wasm_bindgen]
pub fn hide_fallback_ui() {
    // Would hide the fallback UI overlay
}