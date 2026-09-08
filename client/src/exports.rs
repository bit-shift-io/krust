// Client WASM public API exports.
//
// The `#[wasm_bindgen]` surface the JavaScript side calls. All state is held in
// the `TERM_STATE` thread-local.

use js_sys::Function;
use wasm_bindgen::prelude::*;
use wasm_bindgen::JsValue;

use crate::input::map_key;
use crate::measure::measure_cell_dimensions;
use crate::query::collect_query_replies;
use crate::selection::extract_selection;
use crate::state::{TerminalState, TERM_STATE};

/// Initialize the terminal module and receive terminal config JSON
///
/// # Parameters
/// * `canvas_id` - HTML canvas element ID (e.g., "terminal-canvas")
/// * `on_resize` - JS function to call on terminal resize (rows, cols)
///
/// Returns a JSON string describing the terminal state for JS setup.
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
        "cell_width": term_state.cell_width,
        "cell_height": term_state.cell_height,
    })
    .to_string();

    TERM_STATE.with(|s| *s.borrow_mut() = Some(term_state));
    Ok(state_json)
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
        state.schedule_render();
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
        let (row, col) = state.parser_screen().cursor_position();
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
        state.mark_all_dirty();
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
        let (cw, ch) = if let Some(ctx) = state.ctx() {
            // Measure in CSS pixels: reset any dpr scale a prior render left
            // on the context, so cell dims stay independent of devicePixelRatio
            // (browser zoom) and rows/cols below are computed from CSS px.
            let _ = ctx.set_transform(1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
            measure_cell_dimensions(ctx)
        } else {
            (state.cell_width, state.cell_height)
        };
        state.set_cell_dims(cw, ch);
        let dpr = web_sys::window()
            .map(|w| w.device_pixel_ratio())
            .unwrap_or(1.0)
            .max(1.0);
        let phys_w = (width as f64) * dpr;
        let phys_h = (height as f64) * dpr;
        let _ = state.canvas_mut().set_width(phys_w as u32);
        let _ = state.canvas_mut().set_height(phys_h as u32);
        let cols = ((width as f64) / cw).floor() as u16;
        let rows = ((height as f64) / ch).floor() as u16;
        let cols = cols.max(2);
        let rows = rows.max(1);
        state.resize_screen(rows, cols);
        state.set_dims(rows, cols);
        state.mark_all_dirty();
        if let Some(w) = state.webgl_mut() {
            w.cell_w = (cw * dpr).ceil() as u32;
            w.cell_h = (ch * dpr).ceil() as u32;
            w.rows = rows;
            w.cols = cols;
            let _ = w.rebuild_atlas();
        }
        state.trigger_resize(rows, cols);
        Ok(())
    })
}

/// Get the version info for the terminal module
#[wasm_bindgen]
pub fn version() -> String {
    "krust-terminal 0.3.0".to_string()
}

/// Get the selection mode
#[wasm_bindgen]
pub fn selection_mode() -> String {
    TERM_STATE.with(|cell| {
        let state = cell.borrow();
        match state.as_ref() {
            Some(s) => s.selection_mode().to_string(),
            None => "None".to_string(),
        }
    })
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
            Some(s) => match (s.selection_start(), s.selection_end()) {
                (Some(start), Some(end)) => extract_selection(s.parser_screen(), start, end),
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

    /// Clear the active selection (reset both anchor and end to None).
    ///
    /// Useful for clearing the selection when the user clicks elsewhere
    /// or starts a new drag.
    #[wasm_bindgen]
    pub fn clear_selection() {
        TERM_STATE.with(|cell| {
            if let Some(state) = cell.borrow_mut().as_mut() {
                state.clear_selection();
                let _ = state.render();
            }
        });
    }

    /// Handle a click at the given pixel coordinates.
    ///
    /// Clears any existing selection and repaints. Returns JSON with clicked cell coordinates,
    /// or empty JSON if not initialized.
    #[wasm_bindgen]
    pub fn handle_click(x: i32, y: i32) -> String {
        TERM_STATE.with(|cell| {
            let mut guard = cell.borrow_mut();
            if let Some(state) = guard.as_mut() {
                state.clear_selection();
                let _ = state.render();
                let col = (x as f64 / state.cell_width).floor() as u16;
                let row = (y as f64 / state.cell_height).floor() as u16;
                serde_json::json!({ "row": row, "col": col }).to_string()
            } else {
                String::new()
            }
        })
    }

/// Scroll the terminal view by `delta` lines (positive = up into history,
/// negative = down toward the live screen). Returns the resulting scroll
/// offset (0 = live screen at the bottom).
#[wasm_bindgen]
pub fn scroll(delta: isize) -> u32 {
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        match guard.as_mut() {
            Some(state) => {
                state.scroll_by(delta);
                let _ = state.render();
                state.scroll_offset() as u32
            }
            None => 0,
        }
    })
}

/// Snap the terminal view to the live screen (bottom of scrollback).
#[wasm_bindgen]
pub fn scroll_to_bottom() {
    TERM_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.scroll_to_bottom();
            let _ = state.render();
        }
    });
}

/// Snap the terminal view to the oldest available history row.
#[wasm_bindgen]
pub fn scroll_to_top() {
    TERM_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.scroll_to_top();
            let _ = state.render();
        }
    });
}

/// Set the terminal view to an absolute scrollback offset (0 = live screen).
#[wasm_bindgen]
pub fn scroll_to(offset: usize) -> u32 {
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        match guard.as_mut() {
            Some(state) => {
                state.set_scroll_offset(offset);
                let _ = state.render();
                state.scroll_offset() as u32
            }
            None => 0,
        }
    })
}

/// Return the current scrollback view offset (0 = live screen at the bottom).
#[wasm_bindgen]
pub fn scroll_offset() -> u32 {
    TERM_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.scroll_offset() as u32)
            .unwrap_or(0)
    })
}

/// Return the number of scrollback history rows currently available.
#[wasm_bindgen]
pub fn scrollback_len() -> u32 {
    TERM_STATE.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .map(|s| s.scrollback_len() as u32)
            .unwrap_or(0)
    })
}

/// Map a browser keyboard event to raw PTY bytes
///
/// The returned bytes are sent to the server over the binary WebSocket
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
