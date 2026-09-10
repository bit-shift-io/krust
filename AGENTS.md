# AGENTS.md — Krust Terminal Project Context

This document is the shared context for AI coding agents working on the krust
project. It describes the architecture, conventions, and key files.

---

## Project Overview

Krust is a Rust terminal emulator with a two-crate workspace:

- **`server/`** — Axum WebSocket server that spawns a system shell in a
  `portable-pty` PTY and streams raw bytes to clients.
- **`client/`** — WASM client compiled to raw WebAssembly. No `wasm-bindgen`,
  `web-sys`, or `js-sys`: the browser DOM/Canvas/WebGL APIs are reached through
  raw `extern "C"` imports from a single hand-written JS FFI module (`krust`).
  Parses VT100 ANSI bytes via the `vt100` crate and renders on a Canvas 2D
  surface (with a WebGL2 fast path).

The project is **not** a Git client. All references to "Grit" in older
documents (ARCHITECTURE.md, NOTES.md) are stale and should be ignored.

---

## Key Files

| File | Purpose |
|---|---|
| `server/src/main.rs` | Axum router, PTY session management, WebSocket handler, tests |
| `client/src/lib.rs` | WASM terminal: VT100 parser, Canvas 2D renderer, input mapping, selection, tests |
| `client/src/ffi.rs` | Raw `extern "C"` imports from the `krust` JS module + safe wrappers |
| `client/res/krust_runtime.js` | Browser-side FFI runtime (`window.KRUST_RUNTIME`); embedded into the server binary (`include_str!` in `handlers.rs`) |
| `client/res/server.html` | Production HTML served by the server (embedded via `include_str!`) |
| `target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm` | WASM client; embedded into the server binary (`include_bytes!` in `handlers.rs`) |
| `client/res/index.html` | Minimal smoke-test HTML |
| `target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm` | Raw wasm build output |
| `Cargo.toml` | Workspace manifest (`server`, `client`) |
| `TASKS.md` | Implementation roadmap |
| `NOTES.md` | Design rationale and key decisions |

---

## Conventions

- **Async runtime:** Tokio (`full` features) on the server.
- **WebSocket protocol:** Binary frames (`ArrayBuffer`) for PTY output.
  JSON messages (`{"type":"Input","data":...}` and `{"type":"Resize",...}`)
  for client→server control. The server also accepts raw binary input frames.
- **PTY:** `portable-pty` crate. Shell comes from `$SHELL` or `/bin/sh`.
  `TERM=xterm-256color`, `COLORTERM=truecolor`.
- **WASM client:** Single-threaded via `thread_local!` `RefCell<Option<TerminalState>>`.
  Public API exported with `#[no_mangle] pub extern "C" fn` (raw WASM ABI).
  Browser access is via `#[link(wasm_import_module = "krust")] extern "C"`
  imports in `client/src/ffi.rs`; the import object is provided by
  `client/res/krust_runtime.js` (`window.KRUST_RUNTIME.imports`). Every page
  must call `window.KRUST_RUNTIME.install(instance.exports.memory)` immediately
  after instantiation, before any wasm call. Strings/bytes cross the boundary
  as `(ptr, len)` pairs allocated with `alloc` and freed with `free_string` /
  `free_result`; the runtime keeps `i32` handles into a heap registry
  (0 = null).
- **Tests:** Unit tests live alongside code in `#[cfg(test)] mod tests`.
  Server tests use `tower::util::ServiceExt` for one-shot HTTP requests.
- **CORS:** `tower-http::cors::CorsLayer::permissive()` is enabled on all
  routes. The krust server serves cross-origin requests from the Grit
  web UI (running on `localhost:5000`).
- **Build:** `server/build.rs` runs `cargo build --release --target wasm32-unknown-unknown`
  into `target/wasm/` (alongside the main workspace target) when stale,
  so a plain `cargo build`/`cargo run` suffices (skip with
  `KRUST_SKIP_WASM_BUILD=1`). No wasm-bindgen CLI or JS glue is required.
  The server embeds all client assets (`server.html`, `krust_runtime.js`, and
  the wasm) into the binary, so a single-compiled `krust` executable is fully
  self-contained and needs no extra files at install time.

---

## WebSocket Protocol

### Client → Server

| Message | Format | Purpose |
|---|---|---|
| `Input` | JSON `{"type":"Input","data":"..."}` | Keyboard input (UTF-8 string) |
| `Resize` | JSON `{"type":"Resize","cols":N,"rows":M,"pixel_width":W,"pixel_height":H}` | Terminal resize |
| Binary | Raw bytes | Alternative raw input path |

### Server → Client

- Binary frames (`ArrayBuffer`) containing raw PTY output bytes.
- On connect, the server replays the session's scrollback history (up to 512 KB)
  as a sequence of ≤16 KB binary frames.
- Backpressure: per-client `ByteBudget` (1 MB pending). When exceeded, stale
  frames are dropped and the latest history is replayed.

---

## WASM Client API

All functions below use the raw ABI: `(ptr, len)` pairs for strings/bytes,
buffers allocated/owned by the caller, `i32` (`JsHandle`) object handles into
the `krust` FFI registry.

| Function | Purpose |
|---|---|
| `init(canvas_id_ptr, canvas_id_len)` | Initialize terminal, return JSON config (boxed pair) |
| `process_bytes(bytes_ptr, bytes_len)` | Feed PTY output, render, return JSON summary (boxed pair) |
| `query_replies(bytes_ptr, bytes_len)` | Detect DA1/DA2/CPR/OSC-11 queries, return reply bytes (boxed pair) |
| `repaint()` | Force redraw from current parser state |
| `handle_resize(w, h)` | Update canvas dimensions, notify server |
| `key_to_bytes(key_ptr, key_len, ctrl, alt, shift, meta)` | Map keyboard event to PTY bytes (boxed pair) |
| `set_selection(start_row, start_col, end_row, end_col)` | Set selection range |
| `selected_text()` | Extract selected text (boxed pair) |
| `clear_selection()` | Clear active selection |
| `handle_click(x, y)` | Clear selection, return clicked cell (boxed pair) |
| `scroll_to*`, `scroll_offset`, `scrollback_len`, `selection_mode` | Scrollback/selection introspection |
| `version()` | Module version string (boxed pair) |

Memory: `alloc`/`dealloc` (caller buffers), `free_memory`, `free_string`,
`free_result` (free boxed-pair payload / pair allocation).

---

## Testing

```bash
cargo test                    # all workspace tests
cargo test -p krust  # server only
cargo test -p terminal-client   # WASM client only
```

WASM client host unit tests run with plain `cargo test -p terminal-client`.
Browser verification (headless Chromium + Firefox) is in
`client/res/render-check.sh` and `client/res/smoke-test.sh`.

---

## Roadmap

See `TASKS.md` for the implementation plan. Phase 4 (migration & deprecation)
is complete; the WASM path is the primary implementation.