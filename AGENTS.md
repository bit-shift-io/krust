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

---

## Key Files

| File | Purpose |
|---|---|
| `server/src/main.rs` | Axum router + CORS layer, tests |
| `server/src/session.rs` | PTY session management (spawn, scrollback history, broadcast fan-out) |
| `server/src/handlers.rs` | HTTP/WS handler layer; embeds the client HTML, runtime JS, and WASM |
| `server/src/replay.rs` | Builds the mirrored-screen ANSI image used on resets (`replay_image`) |
| `client/src/lib.rs` | WASM terminal: VT100 parser, Canvas 2D renderer, input mapping, selection, tests |
| `client/src/ffi.rs` | Raw `extern "C"` imports from the `krust` JS module + safe wrappers |
| `client/res/krust_runtime.js` | Browser-side FFI runtime (`window.KRUST_RUNTIME`); embedded into the server binary (`include_str!` in `handlers.rs`) |
| `client/res/server.html` | Production HTML served by the server (embedded via `include_str!`) |
| `target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm` | Raw WASM build output; embedded into the server binary (`include_bytes!` in `handlers.rs`) |
| `client/res/index.html` | Minimal smoke-test HTML |
| `Cargo.toml` | Workspace manifest (`server`, `client`) |
| `TASKS.md` | Implementation roadmap |
| `NOTES.md` | Design rationale and key decisions |

---

## Conventions

- **Async runtime:** Tokio on the server, with features listed explicitly
  (`macros`, `rt-multi-thread`, `net`, `sync`) rather than `full`. Do not
  widen this without checking what it pulls in — `full` adds `parking_lot`,
  `signal-hook-registry`, `lock_api`, and friends for code that is not here.
- **Dependencies are deliberately minimal.** The client's only dependency is
  `vt100`; the four JSON payloads crossing the FFI boundary are built and
  parsed by the `json_string` / `json_f64` / `json_number_field` helpers in
  `client/src/exports.rs` rather than by `serde_json`, which cost ~50 KB of
  the shipped `.wasm` (16.5%) to format them. The server keeps `serde_json` and
  `serde`, which it genuinely needs for `ClientMessage`. Note `axum`'s `ws`
  feature already depends on `tokio-tungstenite`, so the server does not
  declare it.
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
- **Panics must stay visible.** Every wasm call site in `server.html` /
  `render-test.html` wraps the exports in `try { ... } catch (_) {}`, and a
  wasm panic reaches JS as a bare `RuntimeError: unreachable` with no message —
  so a panic in the render path used to be completely silent while the WebGL
  framebuffer went on presenting the last frame that finished drawing (which
  looks like stale text that no longer matches the terminal state).
  `install_panic_hook` in `client/src/exports.rs` logs message + source
  location through `krust_console_log` on every wasm build; `init` also
  panics with the underlying `TerminalState::new` error rather than a generic
  string. Keep new entry points calling `install_panic_hook`, and keep real
  error text in `unwrap_or_else`/`expect` messages.
- **Glyph atlas:** no font is embedded and no font crate is linked. The
  WebGL2 atlas is baked with the browser's own Canvas 2D `fillText` via the
  shared FFI: fixed Unicode ranges at init plus a dynamic region for
  arbitrary codepoints (CJK/emoji), rasterized on demand and LRU-evicted
  (`GlyphAtlas::ensure_glyphs`, `gl.texSubImage2D`). `WebGL2Renderer::render`
  is `&mut self` so it can bake newly seen glyphs before drawing. `▣`
  (`graphics.rs`) and braille (`renderer.rs`) are synthesized, not
  font-rendered.
- **Grid geometry is device-pixel-exact.** `measure::finish_cell_dims` rounds the
  measured CSS cell to whole device pixels (`device_pitch = ceil(css * dpr)`) and
  the CSS pitch is derived back as `device_pitch / dpr`, so browser zoom
  produces slightly fractional CSS cell sizes on purpose — never re-round those,
  or the grid drifts off the pixel grid. `grid_metrics` exports the grid origin
  and cell box so the page can center the grid inside the canvas at that pitch.
  `window_dpr` only floors at 1.0; the page must not clamp DPR independently
  (a `Math.min(3, ...)` once made the JS cell size and the Rust buffer size
  disagree above 3x).
- **Rendering while hidden, and after a resync.** `scheduleRender` returns
  early when `document.hidden`; the parser still consumes every byte (pausing
  it would deepen the very lag it is avoiding, and the PTY can stall), only the
  canvas work is skipped. `document.hidden` and `document.visibilityState`
  disagree in the background tab, so the former is the one to test. Coming
  back — `visibilitychange`, `focus`, or an `IntersectionObserver` fire — calls
  `repaintOnReturn()`, which rebuilds WebGL if the context was lost and then
  `repaint()`s. A `"Reset"` control frame calls `reset_terminal`, which
  rebuilds the `vt100` `Parser` and clears scroll/selection/prev-screen state
  before marking the whole grid dirty; without dropping the parser first the
  retained log lands on top of the old screen. Keep `handleIncoming` and
  `handleWsData` at the shared init scope, not nested in the socket closure:
  the `wsQueue` flush needs them before the socket exists.
- **Cursor visibility, styles, and synchronized output.** Both renderers take
  the cursor through `cursor_draw_pos` (`state.rs`), which returns `None`
  while DECTCEM (`CSI ? 25 l`) hides it — TUIs bracket every frame with
  `?25l`/`?25h`, so ignoring the mode paints the cursor wherever a chunk-split
  frame stopped (the marching-block flicker). DECSCUSR (`CSI Ps SP q`) is not
  modeled by `vt100`; `apply_decscusr` (`cursor.rs`) scans it out of the stream
  (partial-sequence carry included) into a `CursorStyle`, rendered as a
  full-cell swap (block) or a `strip_rect` strip in the cell's own fg color
  (underline/bar), with the blink phase toggled by `blink_tick()`. `?2026`
  synchronized frames are withheld by `SyncGate` so a render never lands
  mid-frame; the page arms a 300 ms stall timer whenever `sync_pending()`
  returns 1 and calls `flush_sync()` from it. Keep that backstop — a lost end
  marker otherwise freezes the screen.
- **Scrollbar:** shown only when there is real scrollback and the parser is not
  on the alternate screen (`scrollable = max > 0 && !is_alt_screen()`), and it
  is display-gated with `pointer-events: none` so it never eats clicks. Note
  `vt100` builds the alternate grid with `Grid::new(size, 0)`, so alt-mode
  `scrollback_len()` is already 0 — the `is_alt_screen()` check is a readable
  guard on that invariant, not the thing that makes hiding work.
- **Terminal capability replies:** `query_replies` must answer what a shell
  probes at startup or the PTY appears dead. It covers DA1/DA2/CPR/OSC-11 plus
  XTVERSION (`CSI > 0 q`), TERM (`DCS > | … ST`), and XTGETTCAP
  (`DCS + q … ST`, answered as `DCS 0 + r … ST`). `TERM_NAME`/`TERM_VERSION`
  in `query.rs` are the single source for those. An unterminated or
  non-hex XTGETTCAP body is deliberately not answered, rather than guessed at.
- **Tests:** Unit tests live alongside code in `#[cfg(test)] mod tests`.
  Server tests use `tower::util::ServiceExt` for one-shot HTTP requests.
- **CORS:** `tower-http::cors::CorsLayer::permissive()` is enabled on all
  routes. The krust server serves cross-origin requests from the Grit
  web UI (running on `localhost:5000`). Kept deliberately: hand-rolling
  this saves exactly one crate and risks preflight correctness.
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
- On connect, the server sends `{"type":"Reset"}`, then replays the session's
  scrollback history (up to 512 KB) as a sequence of ≤16 KB binary frames,
  then a **screen image** (see below) that repaints the true visible screen.
- `{"type":"Reset"}` (text frame) tells the client to drop its parser state
  before the log that follows. See the resync notes below.
- **Mirror + screen image, not log replay.** Replaying a *trimmed* byte log
  into a stateful VT parser cannot reconstruct the true screen — a mid-stream
  cut loses every cell the app drew before the window, which is exactly the
  blank-region glitch that appeared after tab-switch/resets. The session keeps
  a mirror `vt100::Parser` (`session.rs`, single writer = the PTY reader
  thread, `mirror.upto` advanced under the same lock). On reset the server
  sends `replay_image` (`server/src/replay.rs`): `mirror.screen().state_diff`
  of a parser fed exactly the bytes the client will have re-parsed, plus an
  `ESC[?1049h/l` prelude (DECSET 1049 always clears the alt grid, DECRST is a
  no-op on the normal screen) and a final CUP pinning the cursor. The image is
  plain ANSI — no client or protocol changes. Tail resyncs still replay the
  log; only full resets use the image.
- **Resync, not replay.** The client's `vt100` parser is stateful, so it can
  only consume a contiguous, duplicate-free byte stream. The server tags every
  frame with the absolute `StreamOffset` it starts at and tracks a per-client
  `sent_upto`; any gap (a dropped broadcast frame, an exceeded `ByteBudget`, a
  `Lagged` receiver) is healed by resending only the missing tail from the
  512 KB log, never the whole log. The old behavior — drop the queue, then
  replay all of history on top of what the client already parsed — both
  duplicated bytes and restarted the parser mid-escape-sequence, which is what
  painted literal tails like `;255m` and `25h` into the grid. Never reintroduce
  it. `resync_plan` is the pure function that decides between
  `UpToDate`/`Tail`/`FullReset`.
- **A client that never catches up gets restarted.** A stalled tab cannot be
  brought up to date with tails forever: each catch-up is overtaken by new
  output, so it would spin and re-copy the log indefinitely. After
  `MAX_CONSECUTIVE_RESYNCS` consecutive catch-ups that actually sent bytes,
  `ResyncStreak` escalates to `FullReset` — `{"type":"Reset"}` plus a screen
  image, which is bounded and always leaves the client parsing a stream
  consistent with its own state. A no-op `UpToDate` pass never increments the
  streak, so budget bookkeeping alone cannot force a healthy client to reset.
- The reset path advances `sent_upto` to the mirror's `upto` (the log tail
  path uses the log's end), keeping the subsequent live frames contiguous with
  the image — bytes between the two are delivered live, never duplicated.
- The log is trimmed only at an ESC boundary (falling back to a UTF-8 boundary
  when no ESC is in range) so a tail replay never orphans half a sequence.
- `subscribe` happens *before* the history snapshot, otherwise output produced
  in between is in neither copy and the client silently loses it.

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
| `rebuild_webgl()` | Recreate WebGL2 renderer after `webglcontextrestored` (i32 result) |
| `repaint()` | Force redraw from current parser state |
| `blink_tick()` | Advance the DECSCUSR blink phase; `1` when it flipped (i32) |
| `sync_pending()` | `1` while the `?2026` sync gate withholds bytes (i32) |
| `flush_sync()` | Force-feed the gate's held bytes (stall-timer backstop) |
| `handle_resize(w, h)` | Update canvas dimensions, notify server |
| `key_to_bytes(key_ptr, key_len, ctrl, alt, shift, meta)` | Map keyboard event to PTY bytes (boxed pair) |
| `set_selection(start_row, start_col, end_row, end_col)` | Set selection range |
| `selected_text()` | Extract selected text (boxed pair) |
| `clear_selection()` | Clear active selection |
| `handle_click(x, y)` | Clear selection, return clicked cell (boxed pair) |
| `scroll_to*`, `scroll_offset`, `scrollback_len`, `selection_mode` | Scrollback/selection introspection |
| `is_alt_screen()` | `1` while the parser owns the alternate screen (alt buffer) |
| `grid_metrics()` | Grid origin, cell box, and row/col counts as JSON (boxed pair) |
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