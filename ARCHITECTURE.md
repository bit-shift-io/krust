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
cell grid onto a `<canvas>` — Canvas 2D by default, WebGL2 as fallback.

### Key Technology Stack
* **Server:** Rust, Tokio (`full`), Axum (`ws`), `portable-pty`, `tower-http`
  (permissive CORS), `futures-util`, `serde`/`serde_json`
* **Client:** Rust compiled to WASM via `wasm-bindgen` (`--target web`),
  `vt100` parser crate, `web-sys`/`js-sys`, `ab_glyph` (WebGL2 glyph atlas)
* **Rendering:** Canvas 2D primary (WebGL2 two-pass instanced quads behind a
  fallback, currently unused because its text pass doesn't render glyphs yet)
  (text fill + geometry-drawn box/block glyphs)
* **Build:** `server/build.rs` builds the raw wasm client into `client/pkg/`
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
│   ├── build.rs             # wasm-pack build trigger (unless skipped)
│   └── src/main.rs          # Axum router, PTY sessions, WebSocket handler, tests
└── client/
    ├── Cargo.toml           # wasm-bindgen/vt100/web-sys/ab_glyph deps
    ├── fonts/
    │   └── Hack-Regular.ttf # Embedded monospace font (include_bytes!)
    ├── src/
    │   ├── lib.rs           # WASM terminal: parse, render, selection, input, tests
    │   └── renderer.rs      # WebGL2 glyph-atlas renderer (render dispatched from lib.rs)
    ├── pkg/                 # wasm-pack build output (served at /pkg/...)
    └── res/
        ├── server.html      # Production HTML served at "/" (include_str!)
        ├── index.html       # Minimal smoke-test HTML
        └── render-check.sh  # Headless-Chromium pixel verification (also render-test.html)
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

### 3.3 WASM Client (`client/src/lib.rs`)

* **State model:** single-threaded `thread_local!` `RefCell<Option<TerminalState>>`;
  public API exported with `#[wasm_bindgen]`.
* **`TerminalState`:** holds the `vt100::Parser`, the canvas element + active
  renderer, cached cell dimensions, and the selection range.
* **Renderer selection:** `TerminalState::new()` measures cell dimensions on a
  **scratch canvas** (never touching the real canvas — a canvas only supports
  one context type, so the WebGL2 attempt can never be poisoned by an earlier
  2D context). Canvas 2D is currently the **default**; WebGL2 is only used as a
  fallback when a 2D context cannot be obtained. Exactly one renderer is active.
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

### 3.4 WebGL2 Renderer (`client/src/renderer.rs`)

* **Font atlas (`GlyphAtlas`):** the 95 printable ASCII glyphs are rasterized
  at init from `EMBEDDED_FONT` (`client/fonts/Hack-Regular.ttf`, shipped via
  `include_bytes!`). `ab_glyph` handles layout; glyphs land in a WebGL2 texture.
* **Two-pass instanced drawing (`GlyphBrush`):**
  * Pass 0 (mode 0) — per-cell background rects using a solid 1×1 atlas pixel;
  * Pass 1 (mode 1) — text glyphs sampling atlas alpha.
  Both passes draw all cells in a single `draw_arrays_instanced` call driven by
  per-instance attributes: offset, size, UV, fg, bg, selection flag, cursor flag.
* **Selection & cursor:** text color goes black on selected cells (bg swaps to
  the cell's fg, matching the Canvas 2D behavior); the cursor is a block that
  swaps fg/bg and takes priority over selection.
* **Resize:** `rebuild_atlas()` re-rasterizes at the new cell pitch.

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
3. Add the corresponding `#[wasm_bindgen]` export in `client/src/lib.rs`.

### Changing Rendering
1. Canvas 2D geometry lives in `client/src/lib.rs`
   (`draw_graphic_cell`, `render_canvas2d`).
2. WebGL2 atlas/instancing lives in `client/src/renderer.rs`
   (`GlyphAtlas`, `GlyphBrush`, `build_instances`).
3. Keep both paths behind the `TerminalState { webgl, ctx }` dispatch so the
   Canvas 2D fallback never silently regresses.

---

## 7. Validation & Testing

```bash
cargo test                    # all workspace tests (28 client + 11 server)
cargo test -p krust           # server only
cargo test -p terminal-client # WASM client only (wgpu-free, runs native)
wasm-pack test client         # WASM-target tests (wasm-bindgen-test)
cargo check -p terminal-client --target wasm32-unknown-unknown
cargo build                   # triggers wasm-pack via build.rs unless skipped
```

Server tests use `tower::util::ServiceExt` one-shot HTTP requests; client tests
live in `#[cfg(test)] mod tests` alongside the code.

---

## 8. Key Files Quick Reference

| File | Purpose |
|---|---|
| `server/src/main.rs` | Axum router, PTY session management, WS handler, tests |
| `client/src/lib.rs` | WASM terminal: VT100 parse, Canvas 2D render, selection, input, tests |
| `client/src/renderer.rs` | WebGL2 glyph-atlas instanced renderer |
| `client/fonts/Hack-Regular.ttf` | Embedded font for the WebGL2 atlas |
| `client/res/server.html` | Production HTML page (served at `/`) |
| `client/res/index.html` | Minimal smoke-test page |
| `AGENTS.md` | Shared agent context and conventions |
| `TASKS.md` | Implementation roadmap |
| `NOTES.md` | Design rationale and decisions |