// Client WASM public API exports.
//
// Raw WASM ABI exports for direct WebAssembly JS API access.
// All strings are returned as (ptr, len) pairs pointing into WASM linear memory.
// The JS caller is responsible for freeing returned memory.

use std::ffi::CString;
use std::os::raw::c_char;

use crate::ffi;
use crate::input::map_key;
use crate::measure::measure_cell_dimensions;
use crate::query::collect_query_replies;
use crate::selection::extract_selection;
use crate::state::{TerminalState, TERM_STATE};

/// Helper: write a string into WASM memory and return (ptr, len).
fn write_string_to_wasm(s: String) -> (*mut c_char, usize) {
    let cstring = CString::new(s).unwrap_or_else(|_| CString::new("").unwrap());
    let ptr = cstring.into_raw();
    let len = unsafe { std::ffi::CStr::from_ptr(ptr).to_bytes().len() };
    (ptr, len)
}

/// Helper: write bytes into WASM memory and return (ptr, len).
///
/// The JS caller frees returned buffers as `Vec::from_raw_parts(ptr, len, len)`,
/// i.e. length AND capacity both equal `len`, so the buffer must be shrunk to
/// its exact length first. Otherwise deallocating with `len` mismatches the
/// allocation's real capacity and dlmalloc aborts (surface symptom: a bare
/// "RuntimeError: unreachable executed" on the JS side).
fn write_bytes_to_wasm(mut bytes: Vec<u8>) -> (*mut u8, usize) {
    bytes.shrink_to_fit();
    let len = bytes.len();
    let ptr = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    (ptr, len)
}

/// Helper: wrap a (ptr, len) pair into the boxed struct returned by WASM exports.
fn return_pair(ptr: *mut u8, len: usize) -> *mut u8 {
    let boxed = Box::new([ptr as u32, len as u32]);
    Box::into_raw(boxed) as *mut u8
}

/// Helper: return a null (0, 0) pair for error/empty results.
fn return_null_pair() -> *mut u8 {
    return_pair(std::ptr::null_mut(), 0)
}

/// Force a renderer before `init()`: 0 = auto (WebGL2 first, Canvas 2D
/// fallback), 1 = force WebGL2, 2 = force Canvas 2D. Wired from the `?r=gl`
/// / `?r=2d` URL params for A/B comparison.
#[no_mangle]
pub extern "C" fn set_renderer_mode(mode: i32) {
    crate::state::RENDERER_MODE.store(mode, std::sync::atomic::Ordering::Relaxed);
}

/// Initialize the terminal module and receive terminal config JSON.
///
/// # Parameters
/// * `canvas_id_ptr` - Pointer to canvas element ID string
/// * `canvas_id_len` - Length of canvas element ID string
///
/// Returns a JSON string pointer/len pair (caller must free).
#[no_mangle]
pub extern "C" fn init(
    canvas_id_ptr: *const u8,
    canvas_id_len: usize,
    cached_dims_ptr: *const u8,
    cached_dims_len: usize,
) -> *mut u8 {
    if canvas_id_ptr.is_null() || canvas_id_len == 0 {
        return write_string_to_wasm("".to_string()).0 as *mut u8;
    }
    let canvas_id = unsafe {
        std::str::from_utf8(std::slice::from_raw_parts(canvas_id_ptr, canvas_id_len))
            .unwrap_or("")
            .to_string()
    };

    // Parse optional cached cell dimensions JSON (e.g. {"w":8.0,"h":18.0}).
    let cached = if !cached_dims_ptr.is_null() && cached_dims_len > 0 {
        let json_str = unsafe {
            std::str::from_utf8(std::slice::from_raw_parts(cached_dims_ptr, cached_dims_len))
                .unwrap_or("")
        };
        serde_json::from_str::<serde_json::Value>(json_str)
            .ok()
            .and_then(|v| {
                let w = v.get("w")?.as_f64()?;
                let h = v.get("h")?.as_f64()?;
                Some((w, h))
            })
    } else {
        None
    };

    let term_state = TerminalState::new(&canvas_id, cached)
        .unwrap_or_else(|_| panic!("terminal init failed"));

    let state_json = serde_json::json!({
        "canvas_id": term_state.canvas_id(),
        "rows": term_state.size().0,
        "cols": term_state.size().1,
        "cell_width": term_state.cell_width,
        "cell_height": term_state.cell_height,
    })
    .to_string();

TERM_STATE.with(|s| *s.borrow_mut() = Some(term_state));
     let (ptr, len) = write_string_to_wasm(state_json);
     return_pair(ptr as *mut u8, len)
}

/// Process incoming ANSI bytes from the WebSocket.
///
/// # Parameters
/// * `bytes_ptr` - Pointer to bytes received from the WebSocket
/// * `bytes_len` - Length of bytes
///
/// Returns a JSON summary pointer/len pair (caller must free).
#[no_mangle]
pub extern "C" fn process_bytes(bytes_ptr: *const u8, bytes_len: usize) -> *mut u8 {
    let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    let result = TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = guard.as_mut().ok_or("init() not called");
        match state {
            Ok(s) => {
                s.process_bytes(bytes);
                s.schedule_render();
                Ok(serde_json::json!({
                    "processed": true,
                    "byte_count": bytes.len(),
                    "rows": s.rows,
                    "cols": s.cols,
                })
                .to_string())
            }
            Err(e) => Err(e),
        }
    });
    match result {
        Ok(json) => {
            let (ptr, len) = write_string_to_wasm(json);
            return_pair(ptr as *mut u8, len)
        }
        Err(_) => return_null_pair(),
    }
}

/// Detect device-query sequences in terminal output and return replies.
///
/// Returns reply bytes as (ptr, len) pair (caller must free).
#[no_mangle]
pub extern "C" fn query_replies(bytes_ptr: *const u8, bytes_len: usize) -> *mut u8 {
    let bytes = unsafe { std::slice::from_raw_parts(bytes_ptr, bytes_len) };
    let result = TERM_STATE.with(|cell| {
        let guard = cell.borrow();
        let state = guard.as_ref().ok_or("init() not called");
        match state {
            Ok(s) => {
                let (row, col) = s.parser_screen().cursor_position();
                Ok(collect_query_replies(bytes, row as usize, col as usize))
            }
            Err(_) => Err(()),
        }
    });
    match result {
        Ok(replies) => {
            let (ptr, len) = write_bytes_to_wasm(replies);
            return_pair(ptr, len)
        }
        Err(_) => return_null_pair(),
    }
}

/// Redraw the terminal immediately from the current parser state.
#[no_mangle]
pub extern "C" fn repaint() {
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        if let Some(state) = guard.as_mut() {
            state.mark_all_dirty();
            let _ = state.render();
        }
    });
}

/// Handle window/canvas resize - called from JS with new pixel dimensions.
#[no_mangle]
pub extern "C" fn handle_resize(width: i32, height: i32) {
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let state = guard.as_mut().ok_or(());
        if let Ok(s) = state {
            let (cw, ch) = if let Some(ctx) = s.ctx() {
                ffi::ctx_set_transform(ctx, 1.0, 0.0, 0.0, 1.0, 0.0, 0.0);
                measure_cell_dimensions(ctx)
            } else {
                (s.cell_width, s.cell_height)
            };
            s.set_cell_dims(cw, ch);
            let dpr = ffi::window_dpr(ffi::window());
            let phys_w = (width as f64) * dpr;
            let phys_h = (height as f64) * dpr;
            ffi::canvas_set_width(s.canvas_handle(), phys_w as u32);
            ffi::canvas_set_height(s.canvas_handle(), phys_h as u32);
            let cols = ((width as f64) / cw).floor() as u16;
            let rows = ((height as f64) / ch).floor() as u16;
            let cols = cols.max(2);
            let rows = rows.max(1);
            s.resize_screen(rows, cols);
            s.set_dims(rows, cols);
            s.mark_all_dirty();
            if let Some(w) = s.webgl_mut() {
                w.cell_w = (cw * dpr).ceil() as u32;
                w.cell_h = (ch * dpr).ceil() as u32;
                w.rows = rows;
                w.cols = cols;
                let _ = w.rebuild_atlas();
            }
            s.trigger_resize(rows, cols);
        }
    });
}

/// Get the version info for the terminal module.
#[no_mangle]
pub extern "C" fn version() -> *mut u8 {
    let s = "krust-terminal 0.3.0";
    let (ptr, len) = write_string_to_wasm(s.to_string());
    return_pair(ptr as *mut u8, len)
}

/// Get the selection mode.
#[no_mangle]
pub extern "C" fn selection_mode() -> *mut u8 {
    let s = TERM_STATE.with(|cell| {
        let state = cell.borrow();
        match state.as_ref() {
            Some(s) => s.selection_mode().to_string(),
            None => "None".to_string(),
        }
    });
    let (ptr, len) = write_string_to_wasm(s);
    return_pair(ptr as *mut u8, len)
}

/// Get the selected text.
#[no_mangle]
pub extern "C" fn selected_text() -> *mut u8 {
    let s = TERM_STATE.with(|cell| {
        let state = cell.borrow();
        match state.as_ref() {
            Some(s) => match (s.selection_start(), s.selection_end()) {
                (Some(start), Some(end)) => extract_selection(s.parser_screen(), start, end),
                _ => String::new(),
            },
            None => String::new(),
        }
    });
    let (ptr, len) = write_string_to_wasm(s);
    return_pair(ptr as *mut u8, len)
}

/// Record a text selection between two grid coordinates.
#[no_mangle]
pub extern "C" fn set_selection(start_row: u16, start_col: u16, end_row: u16, end_col: u16) {
    TERM_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.handle_selection_start(start_row, start_col);
            state.handle_selection_update(end_row, end_col);
        }
    });
}

/// Clear the active selection.
#[no_mangle]
pub extern "C" fn clear_selection() {
    TERM_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.clear_selection();
            let _ = state.render();
        }
    });
}

/// Handle a click at the given pixel coordinates.
/// Returns JSON with clicked cell coordinates.
#[no_mangle]
pub extern "C" fn handle_click(x: i32, y: i32) -> *mut u8 {
    let s = TERM_STATE.with(|cell| {
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
    });
    let (ptr, len) = write_string_to_wasm(s);
    return_pair(ptr as *mut u8, len)
}

/// Scroll the terminal view by `delta` lines.
/// Returns the resulting scroll offset (0 = live screen at the bottom).
#[no_mangle]
pub extern "C" fn scroll(delta: isize) -> u32 {
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

/// Snap the terminal view to the live screen.
#[no_mangle]
pub extern "C" fn scroll_to_bottom() {
    TERM_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.scroll_to_bottom();
            let _ = state.render();
        }
    });
}

/// Snap the terminal view to the oldest available history row.
#[no_mangle]
pub extern "C" fn scroll_to_top() {
    TERM_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            state.scroll_to_top();
            let _ = state.render();
        }
    });
}

/// Set the terminal view to an absolute scrollback offset.
#[no_mangle]
pub extern "C" fn scroll_to(offset: usize) -> u32 {
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

/// Return the current scrollback view offset.
#[no_mangle]
pub extern "C" fn scroll_offset() -> u32 {
    TERM_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| s.scroll_offset() as u32)
            .unwrap_or(0)
    })
}

/// Return the number of scrollback history rows.
#[no_mangle]
pub extern "C" fn scrollback_len() -> u32 {
    TERM_STATE.with(|cell| {
        cell.borrow_mut()
            .as_mut()
            .map(|s| s.scrollback_len() as u32)
            .unwrap_or(0)
    })
}

/// Map a browser keyboard event to raw PTY bytes.
///
/// Returns bytes as (ptr, len) pair (caller must free).
#[no_mangle]
pub extern "C" fn key_to_bytes(
    key_ptr: *const u8,
    key_len: usize,
    ctrl: bool,
    alt: bool,
    shift: bool,
    meta: bool,
) -> *mut u8 {
    let key = unsafe {
        std::str::from_utf8(std::slice::from_raw_parts(key_ptr, key_len))
            .unwrap_or("")
            .to_string()
    };
    let bytes = map_key(&key, ctrl, alt, shift, meta);
    let (ptr, len) = write_bytes_to_wasm(bytes);
    return_pair(ptr, len)
}

/// Install the system-monospace font bytes fetched from the local krust server
/// so the WebGL2 renderer can rebuild its glyph atlas with the *same* font the
/// Canvas 2D reference path resolves via its CSS `FONT_STACK`. The browser
/// cannot expose installed font files, so the server resolves the family via
/// fontconfig and serves the file; the bytes land here as a (ptr, len) pair.
/// No-op on the Canvas 2D path (it paints with CSS fonts directly).
#[no_mangle]
pub extern "C" fn set_system_font(ptr: *const u8, len: usize) {
    if ptr.is_null() || len == 0 {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) }.to_vec();
    TERM_STATE.with(|cell| {
        if let Some(state) = cell.borrow_mut().as_mut() {
            if let Err(e) = state.set_font_bytes(bytes) {
                ffi::console_log(&format!("KRUST: system font rejected: {}", e));
            }
        }
    });
}

/// Free memory allocated by an exported function.
/// # Parameters
/// * `ptr` - Pointer to the start of the data
/// * `len` - Length of the data
#[no_mangle]
pub extern "C" fn free_memory(ptr: *mut u8, len: usize) {
    if !ptr.is_null() {
        unsafe {
            let _ = Vec::from_raw_parts(ptr, len, len);
        }
    }
}

/// Free a string that was returned by an exported function.
/// # Parameters
/// * `ptr` - Pointer to the C string
#[no_mangle]
pub extern "C" fn free_string(ptr: *mut c_char) {
    if !ptr.is_null() {
        unsafe {
            let _ = CString::from_raw(ptr);
        }
    }
}

/// Free a (ptr, len) pair that was returned by an exported function.
/// # Parameters
/// * `ptr` - Pointer to the [u32; 2] array containing (data_ptr, data_len)
#[no_mangle]
pub extern "C" fn free_result(ptr: *mut u8) {
    if !ptr.is_null() {
        unsafe {
            let _ = Box::from_raw(ptr as *mut [u32; 2]);
        }
    }
}

/// Allocate memory in WASM linear memory.
#[no_mangle]
pub extern "C" fn alloc(size: usize) -> *mut u8 {
    let mut buf = Vec::with_capacity(size);
    let ptr = buf.as_mut_ptr();
    std::mem::forget(buf);
    ptr
}

/// Deallocate memory in WASM linear memory.
#[no_mangle]
pub extern "C" fn dealloc(ptr: *mut u8, size: usize) {
    if ptr.is_null() {
        return;
    }
    unsafe {
        let _ = Vec::from_raw_parts(ptr, size, size);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_alloc_dealloc_roundtrip() {
        let ptr = alloc(100);
        assert!(!ptr.is_null());
        dealloc(ptr, 100);
    }

    #[test]
    fn test_dealloc_null_is_noop() {
        dealloc(std::ptr::null_mut(), 0);
    }

    #[test]
    fn test_free_memory_null_is_noop() {
        free_memory(std::ptr::null_mut(), 0);
    }

    #[test]
    fn test_free_string_null_is_noop() {
        free_string(std::ptr::null_mut());
    }

    #[test]
    fn test_free_result_null_is_noop() {
        free_result(std::ptr::null_mut());
    }

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn test_version_returns_nonempty_string() {
        let ptr = version();
        assert!(!ptr.is_null());
        let result = unsafe { Box::from_raw(ptr as *mut [u32; 2]) };
        let (data_ptr, len) = (result[0] as *const u8, result[1] as usize);
        assert!(len > 0);
        let bytes = unsafe { std::slice::from_raw_parts(data_ptr, len) };
        let s = std::str::from_utf8(bytes).unwrap();
        assert!(s.contains("krust-terminal"));
        free_string(data_ptr as *mut c_char);
        free_result(ptr);
    }
}
