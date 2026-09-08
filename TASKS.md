# Implementation Plan: Krust Rust/WASM Terminal

---

## Phase 0: Prototype & Validation ✅

- [x] 0.1: Set up Cargo workspace (`server/`, `client/`)
- [x] 0.2: WASM client compiles with `wasm-pack build --target web`
- [x] 0.3: VT100 parser + Canvas 2D renderer integration
- [x] 0.4: WebSocket binary message pipeline (server → WASM)
- [x] 0.5: Feature-flag xterm.js vs new renderer (later removed)

---

## Phase 1: Server Refactor ✅

- [x] 1.1: Remove xterm.js resources, add PTY session management
- [x] 1.2: WebSocket binary protocol (raw PTY bytes)
- [x] 1.3: Resize handling (JSON `{"type":"Resize",...}` over WS)
- [x] 1.4: Input pipeline (keyboard → PTY raw bytes)
- [x] 1.5: Session management (per-WS PTY pairs, `?s=<id>`)
- [x] 1.6: CORS (`CorsLayer::permissive()` for cross-origin Grit iframes)

---

## Phase 2: WASM Client ✅

- [x] 2.1: `client/Cargo.toml` dependencies (wasm-bindgen, vt100, web-sys, etc.)
- [x] 2.2: HTML structure (canvas + transparent selection overlay)
- [x] 2.3: WASM entry point (`init()`, `process_bytes()`, `repaint()`)
- [x] 2.4: VT100 parser integration
- [x] 2.5: Selection overlay (Shift-drag, `set_selection()`/`selected_text()`/`clear_selection()`)
- [x] 2.6: Input pipeline (`key_to_bytes()`)
- [x] 2.7: Resize handling (WASM side: `handle_resize()`)
- [x] 2.8: Scrollback & initial buffer state

---

## Phase 3: Polish & Fallback ✅

- [x] 3.1: Backpressure (`ByteBudget`, 1 MB per-client, coalescing)
- [x] 3.2: Large paste handling
- [x] 3.3: Fallback path (Canvas 2D is the production path)
- [x] 3.4: Mobile/responsive considerations
- [x] 3.5: Error boundaries (`console-error-panic-hook`)
- [x] 3.6: Performance benchmarking + end-to-end test

---

## Phase 4: Migration & Deprecation ✅

- [x] 4.1: Remove xterm.js resources from `src/main.rs`
- [x] 4.2: Update grit integration
- [x] 4.3: `AGENTS.md` created, `krust.spec` updated
- [x] 4.4: Remove `new-terminal` feature flag
- [x] 4.5: `README.md` updated (no xterm.js references)

---

## Phase 5: Audit Cleanup

> Fix issues identified in the Sept 8 2026 audit. See `AUDIT.md`.

### 5.1: Rewrite `ARCHITECTURE.md` [High] ✅

The entire 410-line document described a different project ("Grit" Git client with
Iced GUI, TabRegistry, `src/git/`, `src/ui/`). None of these exist in the krust
codebase. Replaced with accurate krust content:

- Project overview (Rust terminal emulator, two-crate workspace)
- Architecture diagram (server PTY → WS binary → WASM renderer)
- Directory layout (`server/src/main.rs`, `client/src/lib.rs`, `client/res/`)
- WebSocket protocol (binary output, JSON control messages)
- WASM client API table
- Testing commands

### 5.2: Decide on `renderer.rs` and clean up [High] ✅

**Option A chosen — WebGL2 with Canvas 2D fallback (Canvas 2D currently default).**

> **Status note:** As of 2026-09-08, `init()` picks **Canvas 2D first** and only
> falls back to WebGL2 when a 2D context cannot be obtained. WebGL2 text does
> not render correctly in the real browser yet (background + cursor only, glyphs
> missing), so it is disabled by default until the text pass is fixed. See
> §5.2 item 13 for the re-enable step.

Done:
1. Added `client/fonts/Hack-Regular.ttf` (embedded monospace font)
2. Rewrote `renderer.rs`: two-pass instanced rendering (background + text),
   per-instance fg/bg colors, selection highlight, cursor block
3. Wired `WebGL2Renderer` into `init()` — tries WebGL2, falls back to Canvas 2D
4. `process_bytes()`/`repaint()` dispatch to the active renderer
5. `handle_resize()` syncs renderer cell dims and rebuilds the atlas
6. Removed `#[allow(dead_code)]` on `mod renderer`
7. `TerminalState` holds both `webgl: Option<WebGL2Renderer>` and
   `ctx: Option<CanvasRenderingContext2d>` (exactly one is active)
8. Cell dims measured via a **scratch canvas** (`measure_cell_dimensions_scratch`)
   so the real terminal canvas is never given a 2D context before the WebGL2
   attempt — a canvas only supports one context type, so acquiring a 2D context
   first would have made every WebGL2 attempt fail
9. WebGL2 context is acquired on the real canvas first; the 2D context is only
    obtained as the fallback. `cargo build`, `cargo test` (28 client + 11 server),
    `wasm-pack build --target web`, and the wasm32 check all pass clean
10. Box-drawing/block glyphs (U+2500..U+257F, U+2580..U+2593) render as **geometry**
    via the same `block_geometry`/`box_geometry` helpers as the Canvas 2D path:
    `build_instances` emits two buffers (full-cell bg quads + text quads), graphic
    cells use sub-rects with a reserved opaque "solid" atlas texel, shaded blocks
    (░▒▓) pre-blend `fg*a + bg*(1-a)`, and `GRAPHIC_EPS` overdraw hides seams
11. Debugging fixed three WebGL2 bugs: attribute byte offsets (fg@32/bg@44/sel@56/
    cur@60), the missing `a_uv` attribute (cells sampled the whole atlas instead of
    the glyph sub-rect), and the row-flip (`py = (rows-1-r)*cell_h`)
12. `client/res/render-test.html` updated: reads back via `readPixels` (bottom-up →
    top-down flip) with a 2D `getImageData` fallback; grid offset `yoff = H - rows*ch`
    accounts for the bottom-aligned GL viewport; region bounds fixed (pipe spans all
    40 cells); criteria reflect achievable invariants (a ─ row asserts no dim
    *column*, a │ column no dim *row*, █ both). **`render-check.sh` PASSES**
13. **Temporary default flip (2026-09-08):** `init()` now tries Canvas 2D first and
    only uses WebGL2 if a 2D context fails. `render-test.html` computes `yoff` from
    the active renderer (0 for Canvas 2D, `H - rows*ch` for WebGL2). **Re-enable
    WebGL2 as primary once its text pass renders glyphs in the real browser.**

### 5.3: Remove `#[allow(dead_code)]` on `TerminalState::new` [Medium] ✅

Removed as part of §5.2 (the constructor is now called from `init()`).

### 5.4: Update `NOTES.md` [Medium] ✅

Stale references fixed:
- Title says "WebGL2 Pipeline" → "Rust/WASM Renderer"
- "WebGL canvas" (line 57) → "the renderer"
- "beamterm-renderer receives..." → the `vt100` client + active renderer
- WebGL2 fallback discussion (§2.8) → WebGL2 primary with Canvas 2D fallback,
  measured on a scratch canvas so WebGL2 context creation can never be poisoned
- "beamterm-renderer kept as optional future upgrade" → removed
- "Feature-flag xterm.js fallback" → xterm.js fully removed
- Client dependency list updated (`ab_glyph`, `serde_json`, WebGL2 web-sys features)

### 5.5: Update `TASKS.md` Phase 4 items [Medium] ✅

Phase 4 lines (293-295) referenced an older TASKS.md layout; the current Phase 4
is fully checked. All `beamterm-renderer` references were inside §5.4's map of
stale NOTES.md lines — removed in the §5.5 rewrite. No stale references remain.

### 5.6: Remove commented-out debug log [Low] ✅

`server/src/main.rs:339`: `// eprintln!("[out:{}] {} bytes", ...)` deleted;
the `_sid_out` variable it used (`_sid_out` line 325) removed too. `cargo check -p krust` passes
with zero warnings.

### 5.7: Clean up `client/res/` [Low] ✅

- `ping.js` — **audit was wrong**: it IS referenced by `client/res/index.html`.
  Not deleted.
- `render-check.sh:24` — fixed stale path `demo/server.py` → `res/server.py`
  (the `demo/` dir was renamed to `res/`; the script was broken)
- `client/res/archive/` (15 diagnostic files: diag2-6, glprobe, isolate, isync,
  test2, ticks, timing, trivial, ws, adef, fftest.sh) — **deleted** (2026-09-08);
  superseded by `render-check.sh` and re-creatable
- `client/src/RENDERER_DESIGN.md` — briefly marked **SUPERSEDED**, then deleted
  (2026-09-08); its content is superseded by `ARCHITECTURE.md` §3.4 and the live
  `renderer.rs` (16-float instances, two-pass, per-cell colors, Hack font)
