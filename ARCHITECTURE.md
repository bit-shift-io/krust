# ARCHITECTURE.md — Krust System Architecture

> **Purpose:** Structural map, data flow, and module breakdown for human
> developers and AI assistants working on krust. Keep this file updated as
> modules, protocols, or data flows evolve.

> **Note:** Krust is a terminal emulator, **not** a Git client. Older
> documents described a "Grit" Git client — those references are stale and
> should be ignored (see `NOTES.md`).

---

## 1. Executive Overview

**Project Goal:** A fast, self-contained web terminal. A Rust server spawns a
system shell inside a `portable-pty` PTY and streams raw bytes to browser
clients over WebSocket; a WASM client parses VT100 ANSI bytes and renders the
cell grid onto a `<canvas>` — WebGL2 by default, Canvas 2D as fallback.

### Key Technology Stack
* **Server:** Rust, Tokio (`full`), Axum (`ws`), `portable-pty`, `tower-http`
  (permissive CORS), `futures-util`, `serde`/`serde_json`
* **Client:** Rust compiled to raw WASM (`wasm32-unknown-unknown`, no
  `wasm-bindgen`/`web-sys`/`js-sys`), `vt100` parser crate, `ab_glyph` (WebGL2
  glyph atlas); the DOM/Canvas/WebGL2 APIs are reached through a hand-written
  FFI import module (`krust`) implemented in `client/res/krust_runtime.js`
* **Rendering:** WebGL2 primary (two-pass instanced quads with a rasterized
  glyph atlas + geometry-drawn box/block glyphs); Canvas 2D fallback when
  WebGL2 is unavailable
* **Build:** `server/build.rs` builds the raw wasm client into `target/wasm/`
  when stale, so a plain `cargo build` / `cargo run` is sufficient
  (`KRUST_SKIP_WASM_BUILD=1` disables it). All client assets are then embedded
  into the server binary (`server.html`, `krust_runtime.js`, and the wasm), so
  the compiled `krust` executable is fully self-contained.

### Core Design Principle: Raw Bytes In, Cell Grid Out

The **server is a thin router**: PTY master → raw ANSI bytes → WebSocket binary
frames. The **client owns all terminal state**: it feeds raw bytes into a
stateful VT100 parser and derives the screen grid from pre-rendered glyphs.
No grid state ever crosses the wire — the parser is the frame delimiter.

---

## 2. Directory & Module Hierarchy

```text
.
├── Cargo.toml               # Workspace manifest (server + client)
├── server/
│   ├── build.rs             # raw-wasm build trigger (unless skipped)
│   ├── Cargo.toml
│   └── src/
│       ├── main.rs          # Axum router, PTY sessions, WebSocket handler, tests
│       └── handlers.rs      # HTTP/WS handlers; embeds server.html, krust_runtime.js, the wasm
└── client/
    ├── Cargo.toml           # vt100/ab_glyph deps (no wasm-bindgen)
    ├── fonts/
    │   └── Hack-Regular.ttf # Embedded monospace font (include_bytes!)
    ├── src/
    │   ├── lib.rs           # Module root + re-exports + tests
    │   ├── ffi.rs           # Raw `extern "C"` imports from the `krust` JS module
    │   ├── exports.rs       # `#[no_mangle] pub extern "C"` WASM API surface
    │   ├── state.rs         # TerminalState: parser, renderer dispatch, Canvas 2D path
    │   ├── renderer.rs      # WebGL2 glyph-atlas instanced renderer (GlyphAtlas, GlyphBrush)
    │   ├── graphics.rs      # Shared box-drawing/block-cell geometry (both renderers)
    │   ├── color.rs         # xterm-256 palette + bold-bright rules
    │   ├── measure.rs       # Cell dimension measurement on scratch canvases
    │   ├── input.rs         # Keyboard event → PTY byte mapping
    │   ├── query.rs         # DA1/DA2/CPR/OSC-11 reply detection
    │   └── selection.rs     # Selection range + text extraction
    ├── target/wasm/…        # raw wasm build output (build.rs writes here)
    └── res/
        ├── krust_runtime.js # Browser FFI runtime (`window.KRUST_RUNTIME`)
        ├── server.html      # Production HTML served at "/" (include_str!)
        ├── index.html       # Minimal smoke-test HTML
        ├── render-test.html # Pixel-verification test page
        ├── render-check.sh  # Headless-Chromium pixel verification
        └── server.py        # Test HTTP server (mirrors /pkg/ wasm route)
```

---

## 3. Core Subsystems

### 3.1 Server (`server/src/main.rs`)

* **Routes:** `/` (serves embedded `server.html`), `/ws` (WebSocket upgrade),
  `/pkg/terminal_client_bg.wasm` and `/krust_runtime.js` (served from assets
  embedded in the binary, `no-store`), plus `CorsLayer::permissive()` on all.
* **Sessions (`Session`):** write end (`Arc<Mutex<Box<dyn Write>>`), PTY master
  (`Box<dyn MasterPty>`), a `broadcast::Sender<Vec<u8>>` (512-capacity ring),
  a shared scrollback `history` buffer, and an `AtomicUsize` connection count.
* **Session lookup (`get_or_create_session`):** keyed by `?s=<session_id>` from
  the `WsQuery` (`s`, `dir`). Sessions are created on demand; `?dir=` sets the
  shell start directory. The last connection dropping marks the session for
  cleanup.
* **Scrollback replay:** up to `MAX_HISTORY_BYTES` (512 KB) replayed to a fresh
  client as a sequence of `BINARY_FRAME_MAX` (16 KB) binary chunks.
* **Backpressure (`ByteBudget`):** 1 MB pending per client
  (`MAX_PENDING_BYTES`). When exceeded, stale frames are dropped and the newest
  history is replayed — the client never falls arbitrarily far behind.

### 3.2 WebSocket Protocol

**Client → Server** (JSON, `ClientMessage` tagged by `"type"`), plus raw binary
input frames accepted as an alternative input path:

| Message | Format | Purpose |
|---|---|---|
| `Input` | `{"type":"Input","data":"..."}` | Keyboard input (UTF-8 string) |
| `Resize` | `{"type":"Resize","cols":N,"rows":M,"pixel_width":W,"pixel_height":H}` | Terminal resize → `master.resize(PtySize)` → SIGWINCH |

**Server → Client:** binary frames (`ArrayBuffer`) of raw PTY output.

### 3.3 WASM Client (`client/src/lib.rs` + modules)

* **State model:** single-threaded `thread_local!` `RefCell<Option<TerminalState>>`
  in `state.rs`; the public API is exported with `#[no_mangle] pub extern "C" fn`
  in `exports.rs` (raw WASM ABI — strings/bytes cross the boundary as `(ptr, len)`
  pairs, object handles are `i32`s into the FFI runtime's heap registry).
* **FFI (`ffi.rs`):** raw `extern "C"` imports from the `krust` JS module —
  DOM/canvas/WebGL2 operations implemented by `client/res/krust_runtime.js`.
  Call sites never touch the browser API directly.
* **`TerminalState`:** holds the `vt100::Parser`, the canvas element + active
  renderer, cached cell dimensions, and the selection range.
* **Renderer selection:** `TerminalState::new()` measures cell dimensions on a
  **scratch canvas** (never touching the real canvas — a canvas only supports
  one context type, so the WebGL2 attempt can never be poisoned by an earlier
  2D context). WebGL2 is currently the **default** (`WebGL2Renderer::new()` is
  tried first); Canvas 2D is only used as a fallback when WebGL2 cannot be
obtained. On init it logs `KRUST: WebGL2 renderer initialized` or
   `KRUST: WebGL2 unavailable, falling back to Canvas 2D`. Exactly one renderer
   is active.
* **Renderer override (`?r=`):** `set_renderer_mode(mode)` (exported by
  `exports.rs`, sets a module-level static in `state.rs`) is read by
  `TerminalState::new()` before the context is chosen — `?r=gl` forces WebGL2
  (init fails if unavailable), `?r=2d` forces Canvas 2D, and the default
  auto-picks WebGL2-first. Both `server.html` and `render-test.html` wire it
  from the URL param for A/B comparison of the two renderers.
* **Rendering:** `render()` dispatches to the active renderer. The WebGL2 path
  builds a per-frame selection cell list from the stored selection rectangle
  and passes (screen, default fg/bg, selection, cursor position) to
  `WebGL2Renderer::render()`. The Canvas 2D path is a five-pass draw
  (clear bg → bg rects → selection rects → text → cursor).
* **Cell geometry:** `measure_cell_dimensions` sizes cells from the actual
  font — advance width from `"W"`, height from the tallest painted glyph across
  `MEASURE_PROBES` rasterized on scratch canvases — then **rounds to whole
  device pixels** so adjacent glyphs share exact pixel boundaries (xterm-style),
  eliminating anti-aliased hairline seams in box-drawing UIs.
* **Graphic glyphs:** block elements (U+2580–2593) and box-drawing (U+2500–257F)
  are painted as vector geometry (`draw_graphic_cell`, `block_geometry`,
  `box_geometry`) with a `GRAPHIC_EPS = 0.7` overflow so grids/borders tile
  seamlessly; anything else falls back to the font path.
* **Selection & input:** shift-drag selection extracted natively
  (`set_selection`/`selected_text`/`clear_selection`); `key_to_bytes` maps
  keyboard events to PTY byte sequences (arrows, modifiers, home/end, etc.).

### 3.4 WebGL2 Renderer (`client/src/renderer.rs`, primary path)

* **Font atlas (`GlyphAtlas`):** the 128 ASCII glyphs (0x00–0x7F) are
  rasterized at init from `EMBEDDED_FONT` (`client/fonts/Hack-Regular.ttf`,
  shipped via `include_bytes!`). `ab_glyph` handles layout; glyphs land in a
  WebGL2 texture. Each glyph is rasterized at an em scale derived from the
  **cell width** (`em = glyph_w / h_advance(1.0)`), so every character
  advances exactly one cell width — the same monospace invariant the Canvas 2D
  path gets from its font — instead of an em sized to the cell height (whose
  wider advance overflowed the slot and made wide glyphs touch the next cell
  while narrow ones left uneven gaps). All glyphs share a single text
  **baseline** (placement is offset by the ascent, so descenders hang below
  the line instead of every glyph being glued to the top of its cell); the
  atlas UV rows are swapped when uploading so glyphs render upright. The atlas
  is the alpha source for every text pass; a reserved opaque texel supplies
  flat fills for graphic cells.
* **Two-pass instanced drawing (`GlyphBrush`):**
  * Pass 0 (mode 0) — per-cell background rects using a solid 1×1 atlas pixel;
  * Pass 1 (mode 1) — text glyphs sampling atlas alpha.
  Both passes draw all cells in a single `draw_arrays_instanced` call driven by
  per-instance attributes: offset, size, UV, fg, bg, selection flag, cursor flag.
* **Selection & cursor:** text color goes black on selected cells (bg swaps to
  the cell's fg, matching the Canvas 2D behavior); the cursor is a block that
  swaps fg/bg and takes priority over selection.
* **Resize:** `rebuild_atlas()` re-rasterizes at the new cell pitch.
* **Viewport:** the render viewport and `u_resolution` cover the full drawing
  buffer (`canvas.width`/`canvas.height`); the grid is laid out from pixel
  origin `(0,0)` (bottom-left), which keeps its position identical to a
  grid-sized viewport while avoiding Firefox's "Drawing to a destination rect
  smaller than the viewport rect" warning.

---

## 4. Data & Event Flow

### Startup
```
cargo run
  └─ server::main()
       ├─ AppState { sessions: Arc<RwLock<HashMap>> }
       ├─ Axum Router: "/" | "/ws" | "/pkg/*" (+ CorsLayer::permissive)
       └─ bind 0.0.0.0:${PORT:-3000} → axum::serve
```

### One PTY Session
```
GET /ws?s=<id>[&dir=<path>]
  └─ ws_handler → get_or_create_session(id, dir)
       ├─ first connection: spawn $SHELL in portable-pty (TERM=xterm-256color,
       │    COLORTERM=truecolor), start history capture
       ├─ replay scrollback history as ≤16 KB binary chunks
       └─ read loop: PTY stdout → tx broadcast → each client (ByteBudget-gated)

Client:
  ├─ process_bytes(bytes) → vt100::Parser → render()
  ├─ key_to_bytes()/WS Input → PTY master write
  └─ handle_resize(w,h) → {"type":"Resize",...} → master.resize() → SIGWINCH
```

### Cleanup
```
last connection drops
  └─ drop_connection() → when count hits 0, session marked for removal
       └─ PTY master killed; session removed from the map
```

---

## 5. Architectural Invariants & Key Rules

1. **Raw bytes, not grid state**: the wire only carries ANSI bytes; the client
   owns parsing and rendering. This keeps payloads tiny and the server stateless
   w.r.t. screen content.
2. **One context type per canvas**: cell measurement must happen on a scratch
   canvas so the real terminal canvas stays free for `get_context("webgl2")`.
3. **Whole-pixel cell grid**: cells are rounded to integer device pixels so
   glyphs align exactly; fractional advances would reintroduce seams.
4. **Geometry over fonts for graphic glyphs**: box-drawing and block elements
   are vector fills so adjacent cells tile seamlessly without glyph gaps.
5. **Bounded everything**: history capped (512 KB), per-client budget (1 MB),
   broadcast ring (512) — no unbounded buffers.
6. **CORS is permissive**: the terminal is embedded cross-origin from the host
   app (e.g. `localhost:5000`); every route must remain CORS-open.

---

## 6. How to Extend

### Adding a Control Message
1. Add a variant to `ClientMessage` in `server/src/main.rs` (serde tag = type).
2. Dispatch it in `handle_socket`.
3. Add the corresponding `#[no_mangle]` export in `client/src/exports.rs`.

### Changing Rendering
1. Canvas 2D geometry lives in `client/src/state.rs` (`render_canvas2d`,
   `paint_cell`) with shared geometry in `client/src/graphics.rs`
   (`draw_graphic_cell`, `block_geometry`, `box_geometry`).
2. WebGL2 atlas/instancing lives in `client/src/renderer.rs`
   (`GlyphAtlas`, `GlyphBrush`, `build_instances`).
3. Keep both paths behind the `TerminalState { webgl, ctx }` dispatch so the
   Canvas 2D fallback never silently regresses.

---

## 7. Validation & Testing

```bash
cargo test                    # all workspace tests (client + server)
cargo test -p krust           # server only
cargo test -p terminal-client # WASM client only (host unit tests, no browser)
cargo build                   # triggers the raw-wasm build via build.rs
                             # (skip with KRUST_SKIP_WASM_BUILD=1)
```

Server tests use `tower::util::ServiceExt` one-shot HTTP requests; client tests
live in `#[cfg(test)] mod tests` alongside the code.

Browser verification (headless Chromium) runs via
`client/res/render-check.sh`, which serves `res/` over a tiny Python HTTP
server (`res/server.py`) and asserts on read-back pixels in `render-test.html`:
color fidelity, box/block seamlessness, and per-cell text-glyph ink under
whichever renderer is active.

---

## 8. Key Files Quick Reference

| File | Purpose |
|---|---|
| `server/src/main.rs` | Axum router, PTY session management, WS handler, tests |
| `server/src/handlers.rs` | HTTP/WS handlers; embeds `server.html`, `krust_runtime.js`, the wasm |
| `client/src/exports.rs` | `#[no_mangle]` WASM API surface (init, process_bytes, key_to_bytes, resize, selection, …) |
| `client/src/state.rs` | `TerminalState`: vt100 parser, renderer dispatch, Canvas 2D path |
| `client/src/renderer.rs` | WebGL2 glyph-atlas instanced renderer (`GlyphAtlas`, `GlyphBrush`) |
| `client/src/ffi.rs` | Raw `extern "C"` imports from the `krust` JS module (DOM/Canvas/WebGL2) |
| `client/src/graphics.rs` | Shared box-drawing/block-cell geometry (both renderers) |
| `client/src/measure.rs` | Cell dimension measurement on scratch canvases |
| `client/src/color.rs` | xterm-256 palette + bold-bright rules |
| `client/src/input.rs` | Keyboard event → PTY byte mapping |
| `client/src/query.rs` | DA1/DA2/CPR/OSC-11 reply detection |
| `client/src/selection.rs` | Selection range + text extraction |
| `client/res/krust_runtime.js` | Browser FFI runtime (`window.KRUST_RUNTIME`) |
| `client/fonts/Hack-Regular.ttf` | Embedded font for the WebGL2 atlas |
| `client/res/server.html` | Production HTML page (served at `/`) |
| `client/res/render-check.sh` | Headless-Chromium pixel verification |
| `client/res/render-test.html` | Pixel-assertion test page |

---

## 9. Known Issues & Open Investigations

### First-load shell prompt delay (unexplained, environment-dependent)

**Symptom:** on a *first* page load the terminal paints background + cursor
immediately but the shell prompt appears only after ~10-20 s. Reloads are
instant: the session (and its shell) persist server-side, so a fresh page
simply replays the scrollback history.

**Investigated (2026-09):** measurements ruled out the obvious candidates —
raw PTY spawn of `/bin/sh`, `/bin/bash`, `/usr/bin/fish` all emit their first
bytes in ~21 ms; the `portable-pty` openpty + spawn_command + read path takes
~2 ms; and the full krust server launched normally renders the prompt in
~40 ms (7 ms for a second session). A flat ~10 s reproduced *only* when the
compiled server ran as a subprocess of a python process in the test sandbox;
env vars, inherited fds, controlling session, and launch intermediary were
each excluded without changing the ~10 s. The mechanism was never pinned and
no code defect was found — the delay does not reproduce via a normal launch
path (terminal, `cargo run`, systemd).

**Why it matters / what to check next:** the symptom is likely an external
one-time cost at the first PTY fork/exec on the host (e.g. auditd, AppArmor,
cgroup/IO throttling, or a slow first interactive-shell init). If it reappears,
repro it with the server under `strace -f -e trace=fork,execve,openat` or
profile the shell spawn directly, and confirm whether the wait is in the
server (before first PTY read) or in the shell (before first write). A
boot-time pre-warm of the default session was tried as a workaround and
reverted by request; it is the current fallback option if this is ever
prioritized.
| `AGENTS.md` | Shared agent context and conventions |
| `TASKS.md` | Implementation roadmap |
| `NOTES.md` | Design rationale and decisions |