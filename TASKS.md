# Implementation Plan: Krust Rust/WASM Terminal Refactor

## Phase 0: Prototype & Validation (Weeks 1-2)

### Goal
Get a minimal WASM + WebGL2 terminal running before touching the production backend.

### Milestones

**0.1: Set up Cargo workspace with two crates**
- Create `backend/` and `client-wasm/` directories
- Root `Cargo.toml` with `[workspace]` members
- Verify `cargo check` passes across workspace

**0.2: Add `beamterm-renderer` dependency to WASM client**
- `beamterm-renderer = "0.10"` (per the guide)
- Test `wasm-pack build --target web` compiles successfully
- Verify the generated JS `init()` works in a minimal HTML page
- Check that the canvas renders something (even if just a blank grid)

**0.3: Minimal VT100 parser + beamterm integration**
- Add a VT100 parser crate (e.g., `vt100` crate or custom state machine compiled to WASM)
- Feed parsed cell matrix to `beamterm-renderer`
- Verify: incoming ANSI bytes → cell updates → WebGL render
- **Success criteria**: A simple test pattern (e.g., "Hello World" with cursor) renders correctly

**0.4: WebSocket binary message pipeline (backend → WASM)**
- Backend: stream PTY bytes over WS as `Message::Binary(ArrayBuffer)`
- WASM: receive bytes, feed into VT100 parser
- **Success criteria**: Spawning a shell via `portable-pty` and seeing output appear in the WASM canvas

**0.5: Feature-flag xterm.js vs new renderer**
- Add `new-terminal` feature flag
- When disabled: serve xterm.js resources (current behavior)
- When enabled: render using the new WASM pipeline
- **Success criteria**: Both paths work; can toggle at runtime

---

## Phase 1: Backend Refactor (Weeks 3-4)

### Goal
Replace xterm.js-dependent backend with portable-pty + raw binary WS pipeline.

### Milestones

**1.1: Reorganize `src/main.rs`**
- Remove embedded xterm.js/CSS/JS resources (lines 25-27)
- Remove xterm-related route handlers
- Add PTY session management using `portable-pty`
- Backend spawning shell, cloning master reader, broadcasting raw bytes over WS

**1.2: WebSocket binary protocol**
- Upgrade WS to `BinaryType::Arraybuffer`
- Send raw PTY output chunks (no JSON framing needed for streaming parser)
- Remove JSON deserialization for input/resize — keep those as separate message types if needed, or integrate them

**1.3: Resize handling**
- Add `/ws?resize_cols=N&rows=M` or JSON `{"type":"resize","cols":N,"rows":M}` over WS
- Intercept and call `pair.master.resize(PtySize { cols, rows, ... })`
- Trigger `SIGWINCH` to the shell process

**1.4: Input pipeline — keyboard → PTY**
- Accept keyboard events from WASM client
- Map to raw PTY bytes (Ctrl-C → `\x03`, arrows → ANSI sequences, etc.)
- Write to PTY master writer
- **Important**: Local echo must be disabled (PTY raw mode)

**1.5: Session management**
- Per-session PTY pairs (each WebSocket connection gets its own PTY)
- Session IDs passed as query param `?s=<id>`
- On connect: send initial scrollback/history (or start fresh)
- On disconnect: clean up PTY, drop session

**1.6: CORS & cross-origin frames**
- Keep `CorsLayer::permissive()` (krust serves cross-origin to grit iframes)
- Ensure WS upgrade works from `localhost:5000` → `localhost:3000`

### Deliverable (end of Phase 1)
- `cargo check` passes
- Backend spawns a shell via PTY
- WS binary stream carries PTY output
- Resize works
- Keyboard input reaches the shell

---

## Phase 2: WASM Client (Weeks 5-6)

### Goal
Build the WebGL2 rendering pipeline + VT100 parser + selection overlay.

### Milestones

**2.1: `client-wasm/Cargo.toml` configuration**
- Add: `wasm-bindgen`, `wasm-bindgen-futures`, `web-sys`, `js-sys`
- Add: `beamterm-renderer = "0.10"`
- Add: VT100 parser crate (choice: `vt100` crate, or custom minimal parser)
- Add: `console-error-panic-hook = "0.1"`

**2.2: HTML structure — transparent overlay + canvas**
- Create `index.html` with the design from the guide:
  - `#terminal-canvas` (WebGL2, z-index: 1)
  - `#selection-layer` (invisible, position:absolute, user-select:text, pointer-events:auto/none toggled via Shift)
- Verify the layout: canvas fills viewport, overlay covers it completely

**2.3: WASM entry point (`lib.rs`)**
- `#[wasm_bindgen(start)]` function
- Initialize `Terminal::builder("#terminal-canvas").build()`
- Connect WebSocket: `ws.set_binary_type(BinaryType::Arraybuffer)`
- Set onmessage handler: feed incoming ArrayBuffer bytes into VT100 parser
- Wire `requestAnimationFrame` → `terminal.render_frame()` (or beamterm equivalent)

**2.4: VT100 parser integration**
- Parse incoming byte streams into a cell matrix
- Each cell: character, foreground color, background color, attributes (bold, underline, etc.)
- Update beamterm-renderer's cell buffer on each parser tick
- **Success criteria**: ANSI sequences (colors, cursor movement, erase) are rendered correctly

**2.5: Selection overlay mechanics**
- Mousemove / mousedown on canvas coordinates → map to grid cell
- If Shift key held: toggle overlay `pointer-events: auto`, allow native selection
- On selection release: extract text, send as paste batch over WS, clear overlay
- Without Shift: translate mouse click to VT100 mouse reporting (for TUIs like htop/vim)
- IME candidate windows anchor to the hidden caret (contenteditable + opacity: 0.01)

**2.6: Input pipeline — browser keyboard → PTY raw bytes**
- `keydown` event listener in WASM
- Map key + modifiers to PTY escape sequences
- Send as `Message::Binary` over WebSocket to backend
- Backend writes bytes to PTY master (raw mode, no echo)

**2.7: Resize handling (WASM side)**
- On window resize: calculate cols = pixel_width / cell_width, rows = pixel_height / cell_height
- Send JSON `{"type":"resize","cols":N,"rows":M}` over WS
- (Backend already handles this from Phase 1)

**2.8: Scrollback & initial buffer state**
- On first connect: either start with blank grid (fresh shell) OR
- Server sends initial grid state snapshot (for persistent/tmux sessions)
- Parser resumes from sent state

### Deliverable (end of Phase 2)
- Minimal terminal renders in the WASM canvas
- Cursor blinks
- Basic ANSI colors work
- Shift-drag selects text, copies to clipboard
- Without Shift, mouse clicks don't interfere with TUI tracking
- Keyboard input sends correct PTY bytes

---

## Phase 3: Polish & Fallback (Weeks 7-8)

### Goal
Handle edge cases, backpressure, and fallback paths.

### Milestones

**3.1: Backpressure & bounded buffers**
- Replace unbounded `broadcast::channel(100)` with coalescing byte buffer
- Strict memory threshold per client (e.g., 1MB)
- When threshold exceeded: drop stale frames, flush latest screen state
- Handle `RecvError::Lagged` gracefully

**3.2: Large paste handling**
- `contenteditable` paste event → extract full text → chunk + send as batch payload
- Throttle/pacing to prevent PTY buffer overflow
- Immediately clear `element.innerHTML = ""` after paste is processed

**3.3: Fallback path — WebGL2 unavailable**
- Feature-detect WebGL2 support on initial load
- If unavailable: fall back to xterm.js path (feature-flagged)
- Display fallback UI: "WebGL2 not available, using xterm.js fallback"

**3.4: Mobile/responsive considerations**
- Test on various screen sizes
- Cell width/height ratios adjust gracefully
- Touch event handling doesn't interfere with selection overlay

**3.5: Error boundaries & panic handling**
- `console-error-panic-hook` catches WASM panics
- Graceful degradation if WebGL context is lost
- WebSocket reconnection logic

**3.6: Performance benchmarking**
- Measure FPS with light output (idle prompt) vs heavy output (`cat large_file.txt`)
- Measure memory usage over time (scrollback buffer growth)
- Verify render loop stays < 1ms for typical grid sizes (80×24 to 120×40)

**3.6: End-to-end test**
- Spawn `krust` backend
- Open `index.html` via local web server
- Verify: shell prompt appears, commands execute, output renders, copy/paste works, resize works, Shift-drag selection works

---

## Phase 4: Migration & Deprecation (Weeks 9-10)

### Goal
Migrate the existing `krust` codebase and deprecate xterm.js.

### Milestones

**4.1: Remove xterm.js resources from `src/main.rs`**
- Delete the `include_str!` lines for `xterm.js`, `xterm-addon-fit.js`, `xterm-addon-webgl.js`
- Remove corresponding route handlers
- Remove CORS/configuration that was only needed for xterm.js

**4.2: Update grit integration (`src/krust.rs` / `AGENTS.md`)**
- Update the iframe `src` to point to the new WASM endpoint
- If grit still embeds krust, update the URL from `http://localhost:3000/` to the new path
- Update session id scheme if needed

**4.3: Update `NOTES.md` / `ARCHITECTURE.md` / `AGENTS.md`**
- Document the new architecture
- Update conventions for the new codebase
- Remove xterm.js-specific notes

**4.4: Remove feature flag (once proven)**
- After the new path is proven stable in production, remove the xterm.js fallback flag
- Make the WASM path the default and only option

**4.5: Final `cargo check` / `cargo test`**
- Ensure all warnings are resolved
- All existing tests still pass (or update them for the new pipeline)

---

## Success Criteria Checklist

| # | Criterion | Target |
|---|---|---|
| 1 | `cargo check` passes (workspace) | ✅ |
| 2 | WASM builds with `wasm-pack build --target web` | ✅ |
| 3 | VT100 parser renders ANSI correctly | ✅ |
| 4 | Cursor blinks and moves correctly | ✅ |
| 5 | Basic colors (16/256/truecolor) work | ✅ |
| 6 | Shift-drag text selection works natively | ✅ |
| 7 | Without Shift, mouse clicks pass through to TUI | ✅ |
| 8 | Keyboard input reaches shell with correct encoding | ✅ |
| 9 | Resize (browser → PTY → SIGWINCH) works | ✅ |
| 10 | Large paste doesn't overflow PTY buffer | ✅ |
| 11 | 60 FPS render loop ( < 16.6ms per frame) | ✅ |
| 12 | Memory stable over time (no leak from scrollback) | ✅ |
| 13 | Fallback path if WebGL2 unavailable | ✅ |
| 14 | CORS + cross-origin iframe works | ✅ |
| 14 | `cargo test` passes | ✅ |

---

## Risks & Mitigations

| Risk | Mitigation |
|---|---|
| WebGL2 context loss | Listen for `webglcontextlost` event; show fallback UI |
| VT100 parser misses edge cases | Start with well-tested crate (e.g., `vt100`); add custom handling for missing sequences |
| PTY resize race conditions | Use `SIGWINCH`; avoid resizing while output is in flight |
| Backpressure overflow | Bounded buffer + coalescing; drop stale frames deliberately |
| Selection overlay vs TUI mouse tracking | Shift-modifier distinction; default `pointer-events: none` |
| IME / composition failures | Test with CJK input; overlay `contenteditable` + `pointer-events: auto` must work |
| Large prime to rewrite vs fix xterm.js bugs | If xterm.js bugs are workable, stick with xterm.js. Only refactor if hitting unfixable structural issues. |
| Team bandwidth | Prototype first (Phase 0) before committing full effort. Get VT100 + beamterm working minimal test before scaling up. |

---

## Resource Estimate

| Area | Estimated Effort |
|---|---|
| Prototype WASM + beamterm + VT100 | 1 week |
| Backend PTY + WS binary pipeline | 1 week |
| WASM client: overlay, input, resize | 1 week |
| Backpressure, pastes, fallbacks | 1 week |
| Migration & deprecation | 1 week |
| Buffer/contingency | 1 week |
| **Total** | **~5-6 weeks of focused work** |

---