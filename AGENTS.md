# AGENTS.md — Krust Terminal Project Context

This document is the shared context for AI coding agents working on the krust
project. It describes the architecture, conventions, and key files.

---

## Project Overview

Krust is a Rust terminal emulator with a two-crate workspace:

- **`backend/`** — Axum WebSocket server that spawns a system shell in a
  `portable-pty` PTY and streams raw bytes to clients.
- **`client-wasm/`** — WASM client compiled with `wasm-bindgen` that parses
  VT100 ANSI bytes via the `vt100` crate and renders the terminal on a
  Canvas 2D surface.

The project is **not** a Git client. All references to "Grit" in older
documents (ARCHITECTURE.md, NOTES.md) are stale and should be ignored.

---

## Key Files

| File | Purpose |
|---|---|
| `backend/src/main.rs` | Axum router, PTY session management, WebSocket handler, tests |
| `client-wasm/src/lib.rs` | WASM terminal: VT100 parser, Canvas 2D renderer, input mapping, selection, tests |
| `client-wasm/demo/backend.html` | Production HTML served by the backend (`include_str!`) |
| `client-wasm/demo/index.html` | Minimal smoke-test HTML |
| `client-wasm/pkg/` | `wasm-pack` build output (committed) |
| `res/` | xterm.js fallback assets (used when WebGL2 unavailable) |
| `Cargo.toml` | Workspace manifest (`backend`, `client-wasm`) |
| `TASKS.md` | Implementation roadmap |
| `NOTES.md` | Design rationale and key decisions |
| `rust_wasm_guide.md` | Reference guide for the Rust/WASM terminal architecture |

---

## Conventions

- **Async runtime:** Tokio (`full` features) on the backend.
- **WebSocket protocol:** Binary frames (`ArrayBuffer`) for PTY output.
  JSON messages (`{"type":"Input","data":...}` and `{"type":"Resize",...}`)
  for client→server control. The backend also accepts raw binary input frames.
- **PTY:** `portable-pty` crate. Shell comes from `$SHELL` or `/bin/sh`.
  `TERM=xterm-256color`, `COLORTERM=truecolor`.
- **WASM client:** Single-threaded via `thread_local!` `RefCell<Option<TerminalState>>`.
  Public API exported with `#[wasm_bindgen]`.
- **Tests:** Unit tests live alongside code in `#[cfg(test)] mod tests`.
  Backend tests use `tower::util::ServiceExt` for one-shot HTTP requests.
- **Build:** `wasm-pack build --target web` then `cargo build`/`cargo run`.

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

| Function | Purpose |
|---|---|
| `init(canvas_id, on_resize)` | Initialize terminal, return JSON config |
| `process_bytes(bytes)` | Feed PTY output, render, return JSON summary |
| `query_replies(bytes)` | Detect DA1/DA2/CPR/OSC-11 queries, return reply bytes |
| `repaint()` | Force redraw from current parser state |
| `handle_resize(w, h)` | Update canvas dimensions, notify server |
| `key_to_bytes(key, ctrl, alt, shift, meta)` | Map keyboard event to PTY bytes |
| `set_selection(start_row, start_col, end_row, end_col)` | Set selection range |
| `selected_text()` | Extract selected text |
| `clear_selection()` | Clear active selection |
| `handle_click(x, y)` | Clear selection, return clicked cell |
| `is_webgl2_available()` / `check_fallback()` | WebGL2 availability |
| `show_fallback_ui(msg)` / `hide_fallback_ui()` | Fallback banner |
| `version()` | Module version string |

---

## Testing

```bash
cargo test                    # all workspace tests
cargo test -p terminal-backend  # backend only
cargo test -p terminal-client   # WASM client only
```

WASM tests run under `wasm-bindgen-test` via `wasm-pack test`.

---

## Roadmap

See `TASKS.md` for the implementation plan. Phase 4 (migration & deprecation)
is complete; the WASM path is the primary implementation.