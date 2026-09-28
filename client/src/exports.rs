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

// --- Minimal JSON support -------------------------------------------------
//
// The client deliberately has no `serde_json` dependency: it would add ~54 KB
// to the shipped `.wasm` (an 18% regression) to format four tiny,
// fixed-shape payloads. These two helpers cover the whole surface — three
// numeric/bool objects we build, and one `{w,h}` object we parse.

/// Render `s` as a quoted, escaped JSON string (quotes included).
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Format a float as a JSON number.
///
/// Non-finite values are not representable in JSON, so they become `null`
/// (matching what `serde_json` would have emitted) rather than the bare
/// `NaN` / `inf` that `{}` would produce.
fn json_f64(v: f64) -> String {
    if v.is_finite() {
        format!("{}", v)
    } else {
        "null".to_string()
    }
}

/// Look up a numeric field by key in a flat JSON object and parse it.
///
/// Sufficient for the only object the client parses, `{"w":<f64>,"h":<f64>}`,
/// and tolerant of key order and surrounding whitespace. Returns `None` when
/// the key is absent or its value is not a JSON number (including `null`).
fn json_number_field(json: &str, key: &str) -> Option<f64> {
    let needle = format!("\"{}\"", key);
    let after_key = json.find(&needle)? + needle.len();
    let rest = json[after_key..].trim_start();
    let rest = rest.strip_prefix(':')?.trim_start();

    let bytes = rest.as_bytes();
    let mut end = 0;
    if bytes.first() == Some(&b'-') {
        end += 1;
    }
    let int_start = end;
    while end < bytes.len() && bytes[end].is_ascii_digit() {
        end += 1;
    }
    if end == int_start {
        return None;
    }
    if bytes.get(end) == Some(&b'.') {
        end += 1;
        let frac_start = end;
        while end < bytes.len() && bytes[end].is_ascii_digit() {
            end += 1;
        }
        if end == frac_start {
            return None;
        }
    }
    if matches!(bytes.get(end), Some(b'e') | Some(b'E')) {
        let mut exp = end + 1;
        if matches!(bytes.get(exp), Some(b'+') | Some(b'-')) {
            exp += 1;
        }
        let exp_start = exp;
        while exp < bytes.len() && bytes[exp].is_ascii_digit() {
            exp += 1;
        }
        if exp > exp_start {
            end = exp;
        }
    }
    rest[..end].parse::<f64>().ok()
}

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
// --- Panic reporting ------------------------------------------------------

/// Report Rust panics to the browser console instead of losing them.
///
/// A wasm panic reaches JS as a bare `RuntimeError: unreachable` with no
/// message, and every call site in `server.html` wraps the wasm exports in
/// `try { ... } catch (_) {}` so the throw is swallowed. A panic inside the
/// render path is therefore completely invisible: the WebGL framebuffer keeps
/// presenting the last frame that finished drawing, which reads on screen as
/// stale text that no longer matches the terminal state. Logging the message
/// and source location makes that failure mode diagnosable.
#[cfg(target_arch = "wasm32")]
fn install_panic_hook() {
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::panic::set_hook(Box::new(|info| {
            let location = match info.location() {
                Some(l) => format!("{}:{}:{}", l.file(), l.line(), l.column()),
                None => "<unknown location>".to_string(),
            };
            let payload = info.payload();
            let message = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_string());
            ffi::console_log(&format!("krust panic: {} (at {})", message, location));
        }));
    });
}

/// No-op off wasm: the console import does not exist in host builds, and host
/// tests rely on the default hook to report their own failures.
#[cfg(not(target_arch = "wasm32"))]
fn install_panic_hook() {}

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
    install_panic_hook();
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
        json_number_field(json_str, "w").zip(json_number_field(json_str, "h"))
    } else {
        None
    };

    let term_state = TerminalState::new(&canvas_id, cached)
        .unwrap_or_else(|e| panic!("terminal init failed: {}", e));

    let state_json = format!(
        "{{\"canvas_id\":{},\"rows\":{},\"cols\":{},\"cell_width\":{},\"cell_height\":{}}}",
        json_string(term_state.canvas_id()),
        term_state.size().0,
        term_state.size().1,
        json_f64(term_state.cell_width),
        json_f64(term_state.cell_height),
    );

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
                Ok(format!(
                    "{{\"processed\":true,\"byte_count\":{},\"rows\":{},\"cols\":{}}}",
                    bytes.len(),
                    s.rows,
                    s.cols
                ))
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

/// Whether the WebGL context is currently lost. `1` when a renderer exists
/// and its context is gone, `0` otherwise.
///
/// The page checks this when the tab becomes visible again: a `repaint` into a
/// lost context silently paints nothing, so without this check a tab that
/// lost its context while hidden and then never saw
/// `webglcontextrestored` (e.g. because the page was frozen) stays blank.
#[no_mangle]
pub extern "C" fn webgl_context_lost() -> i32 {
    TERM_STATE.with(|cell| {
        let guard = cell.borrow();
        match guard.as_ref() {
            Some(state) if state.webgl_is_lost() => 1,
            _ => 0,
        }
    })
}

/// Drop the parser's state and start over from a blank screen.
///
/// Driven by the server's `{"type":"Reset"}` control frame, sent when this
/// client has fallen so far behind that the bytes it is missing have already
/// aged out of the retained window, so no contiguous tail can be sent.
#[no_mangle]
pub extern "C" fn reset_terminal() {
    install_panic_hook();
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        if let Some(state) = guard.as_mut() {
            state.reset();
            ffi::console_log("KRUST: terminal reset after server-side resync");
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
            let dpr = ffi::window_dpr(ffi::window()).max(1.0);
            // Round, don't truncate: the buffer size and the fit below have to
            // agree to the pixel or the grid's margins end up lopsided.
            let phys_w = ((width as f64) * dpr).round();
            let phys_h = ((height as f64) * dpr).round();
            ffi::canvas_set_width(s.canvas_handle(), phys_w as u32);
            ffi::canvas_set_height(s.canvas_handle(), phys_h as u32);
            // Fit the grid to the canvas in whole device pixels and center it in
            // the leftover, so the terminal never draws past the window edge.
            s.refit(phys_w as i64, phys_h as i64);
            let (rows, cols) = s.size();
            s.resize_screen(rows, cols);
            s.mark_all_dirty();
            if let Some(w) = s.webgl_mut() {
                let _ = w.rebuild_atlas();
            }
            s.trigger_resize(rows, cols);
        }
    });
}

/// Grid geometry after the last fit: `{"rows":R,"cols":C,"x":X,"y":Y}`.
///
/// `x`/`y` are the CSS-pixel origin of the centered grid, which the page needs
/// to map mouse coordinates onto cells.
#[no_mangle]
pub extern "C" fn grid_metrics() -> *mut u8 {
    let json = TERM_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| {
                let dpr = ffi::window_dpr(ffi::window()).max(1.0);
                let (x, y) = s.origin_css(dpr);
                format!(
                    "{{\"rows\":{},\"cols\":{},\"x\":{},\"y\":{}}}",
                    s.rows, s.cols, x, y
                )
            })
            .unwrap_or_else(|| "{\"rows\":0,\"cols\":0,\"x\":0,\"y\":0}".to_string())
    });
    let (ptr, len) = write_string_to_wasm(json);
    return_pair(ptr as *mut u8, len)
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
            let (row, col) = state.cell_at(x as f64, y as f64);
            format!("{{\"row\":{},\"col\":{}}}", row, col)
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

/// Whether the alternate screen is active (1 = alt screen, 0 = normal).
///
/// The page uses this to suppress the scrollback scrollbar while a full-screen
/// TUI owns the screen, since such apps manage their own scrolling.
#[no_mangle]
pub extern "C" fn is_alt_screen() -> i32 {
    TERM_STATE.with(|cell| {
        cell.borrow()
            .as_ref()
            .map(|s| i32::from(s.is_alt_screen()))
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

/// Recreate the entire WebGL2 renderer (program, buffers, glyph atlas) after
/// the browser restored a lost WebGL context. Fires from the page's
/// `webglcontextrestored` handler: the browser drops every GL object when it
/// loses the context (e.g. the tab was hidden), so all rendering silently
/// stops until krust rebuilds them. Returns 1 on success (and triggers a full
/// redraw), 0 when there is nothing to rebuild or the rebuild failed.
#[no_mangle]
pub extern "C" fn rebuild_webgl() -> i32 {
    TERM_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return 0;
        };
        match state.rebuild_webgl() {
            Ok(()) => {
                ffi::console_log("KRUST: WebGL2 renderer rebuilt after context restore");
                let _ = state.render();
                1
            }
            Err(e) => {
                ffi::console_log(&format!("KRUST: WebGL2 rebuild failed: {}", e));
                0
            }
        }
    })
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

    #[test]
    fn test_json_string_quotes_and_leaves_plain_text_alone() {
        assert_eq!(json_string("term"), "\"term\"");
        assert_eq!(json_string(""), "\"\"");
    }

    #[test]
    fn test_json_string_escapes_specials() {
        assert_eq!(json_string(r#"a"b"#), r#""a\"b""#);
        assert_eq!(json_string(r"a\b"), r#""a\\b""#);
        assert_eq!(json_string("a\nb\tc"), r#""a\nb\tc""#);
        // Control characters become \uXXXX escapes, not raw bytes.
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
        assert_eq!(json_string("\u{1b}"), "\"\\u001b\"");
    }

    #[test]
    fn test_json_f64_matches_plain_number_format() {
        assert_eq!(json_f64(8.0), "8");
        assert_eq!(json_f64(18.5), "18.5");
        assert_eq!(json_f64(-0.25), "-0.25");
    }

    #[test]
    fn test_json_f64_maps_non_finite_to_null() {
        // Bare `NaN`/`inf` would not be valid JSON for the JS side to parse.
        assert_eq!(json_f64(f64::NAN), "null");
        assert_eq!(json_f64(f64::INFINITY), "null");
    }

    #[test]
    fn test_json_number_field_parses_the_cached_dims_shape() {
        let json = r#"{"w":8.0,"h":18.5}"#;
        assert_eq!(json_number_field(json, "w"), Some(8.0));
        assert_eq!(json_number_field(json, "h"), Some(18.5));
    }

    #[test]
    fn test_json_number_field_tolerates_key_order_and_whitespace() {
        let json = r#"{ "h" : -12 , "w" : 7 }"#;
        assert_eq!(json_number_field(json, "w"), Some(7.0));
        assert_eq!(json_number_field(json, "h"), Some(-12.0));
    }

    #[test]
    fn test_json_number_field_handles_exponents_and_stops_at_delimiter() {
        assert_eq!(json_number_field(r#"{"w":1e2}"#, "w"), Some(100.0));
        assert_eq!(json_number_field(r#"{"w":2E-3}"#, "w"), Some(0.002));
        assert_eq!(json_number_field(r#"{"w":5,"h":6}"#, "w"), Some(5.0));
        assert_eq!(json_number_field(r#"{"w":5.25e1}"#, "w"), Some(52.5));
    }

    #[test]
    fn test_json_number_field_rejects_non_numbers() {
        assert_eq!(json_number_field(r#"{"w":null}"#, "w"), None);
        assert_eq!(json_number_field(r#"{"w":"8"}"#, "w"), None);
        assert_eq!(json_number_field(r#"{"w":true}"#, "w"), None);
        assert_eq!(json_number_field(r#"{"w":}"#, "w"), None);
        assert_eq!(json_number_field(r#"{"w":.}"#, "w"), None);
        assert_eq!(json_number_field(r#"{"w":-}"#, "w"), None);
        assert_eq!(json_number_field(r#"{"width":8}"#, "w"), None);
        assert_eq!(json_number_field(r#"{"w":1.}"#, "w"), None);
        assert_eq!(json_number_field("not json", "w"), None);
    }
}
