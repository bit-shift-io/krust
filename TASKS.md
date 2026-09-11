# Plan: Finish WebGL2 Port

## Status: COMPLETE

All five tasks are done and verified: `cargo test` (11 server + 46 client
tests) passes, `cargo build` is clean, and `render-check.sh` is green under
headless Chromium with the WebGL2 renderer active — the new per-cell text
assertion reports `min_ink_per_cell: 17, cells_with_ink: 8`.

Two extra fixes were required beyond the original plan:
- `client/res/krust_runtime.js` `gl_vertex_attrib_pointer`: the `offset`
  `i64` crosses the WASM ABI as a BigInt; it is now converted with
  `Number(offset)` before the WebGL call, or `vertexAttribPointer` throws
  a TypeError and nothing renders.
- `client/res/server.py` `WASM_PATH`: computed from the script's own
  location instead of the doc-root argument, because `render-check.sh`
  starts the server with the doc root at `client/`.

Known limitation (out of scope for this plan): the WebGL2 glyph atlas
rasterizes all 128 ASCII glyphs (0x00–0x7F). Non-ASCII characters such as
braille (`⠋⠁⠂…`) still have no atlas entry (`uv_for` → `None`), so they
render invisible under WebGL2 (the Canvas 2D path falls back to the system
font).

Post-completion fixes: the atlas UV rows were swapped (glyphs rasterize
top-down but the screen quad samples v0 at its top edge) so text renders
upright instead of mirrored across the horizontal axis, and the render viewport
grew to the full drawing buffer (was `cols*cell_w × rows*cell_h`) to silence
Firefox's "Drawing to a destination rect smaller than the viewport rect"
warning; the grid is laid out from pixel origin (0,0) so its position is
unchanged.

Second round of fixes (all verified):
- **Glyph baseline alignment:** `GlyphAtlas` was gluing every glyph's bounding
  box to the top of its atlas slot, so baselines landed at different rows per
  glyph. All glyphs are now shifted by the font ascent, sharing one text
  baseline (descenders and `_` hang below it). Verified by new client tests
  and a browser-level assertion (a `g` row reaches the bottom of its cell and
  `_` sits in the bottom band; `render-check.sh` reports `descender_ink: 150,
  underscore_ink: 140`).
- **Glyph horizontal spacing (advance scale):** the atlas rasterized every
  glyph at an em equal to the cell *height* (~18px), whose Hack advance
  (~0.606em ≈ 11px) was wider than the 8px cell — wide glyphs touched the next
  cell and narrow glyphs left uneven gaps. Glyphs are now rasterized at the em
  where Hack's advance equals one *cell width* (`em = glyph_w / h_advance`,
  ≈13.2px for an 8px cell), reproducing the Canvas 2D monospace invariant
  (every character advances exactly one cell width), with the baseline
  re-derived from that scale. Verified: `render-check.sh` still green
  (`text min_ink_per_cell: 15`, `orient top 180/bottom 0`,
  `baseline descender 70/underscore 80` at the smaller glyph size). Compare
  WebGL2 vs Canvas 2D visually with `?r=gl` / `?r=2d`.
- **WebGL2/Canvas 2D pixel-convention unification (block-element & box-drawing
  glyphs, opencode logo mispaint).** The GL path previously used a bottom-up
  pixel convention (row 0 at `py=(rows-1-r)*cell_h`), which the vertex shader
  translated to an upright NDC grid; `graphic_rects` compensated text glyphs
  but applied mirrored math to block/box glyphs, so `▀`/`▄`/`┌`/`└` painted
  upside-down (the opencode block logo collapsed into solid bars). Fix: the
  GL renderer now shares the Canvas 2D **top-left** convention — the shader
  negates NDC y and flips `a_texcoord.y` (atlas glyphs stay upright), `py =
  r * cell_h`, and `graphic_rects` mirrors `draw_graphic_cell`'s math exactly.
  `render-test.html` GL readback (`yoff`) no longer assumes the bottom-aligned
  grid. Verified: 52 client + 12 server tests, `render-check.sh` green with
  gl/2d alignment parity ≤2px, and the opencode block-logo crown rows render
  pixel-identically in GL vs 2D.
- **Slow first load — investigated, root cause NOT in krust.** Symptoms: only
  the *first* page load waits ~10-20s for the shell prompt; refreshes are
  instant (the session and its shell persist). Measurements: raw PTY spawn of
  `/bin/sh`, `/bin/bash`, and `/usr/bin/fish` — ~21ms each. The `portable-pty`
  API (openpty + spawn_command + read) — **2ms** for an interactive shell. The
  full krust server launched normally — **prompt visible in 40ms** (7ms for a
  second session). Conclusion: not the pty library, not the shell, not krust's
  spawn code. A flat ~10s appeared only when the compiled server ran as a
  subprocess of a python process in the test sandbox (excluded env vars,
  inherited fds, new session, launch intermediary) — an environment-specific
  artifact that could not be pinned to a mechanism and does not reproduce via
  the normal launch path. No code change was kept for this (a boot-time
  pre-warm + stable default session id experiment worked around it but was
  reverted on request).
- **Renderer override (`?r=`):** `set_renderer_mode()` accepts 0 = auto
  (WebGL2 first, Canvas 2D fallback), 1 = `?r=gl` (force WebGL2, fail hard if
  unavailable), 2 = `?r=2d` (force Canvas 2D). Wired into `server.html` and
  `render-test.html` for A/B comparison of the two renderers.
- `render-test.html` now reports per-stage boot timing (`timing.fetch_ms`,
  `instantiate_ms`, `init_ms`, `resize_ms`, `feed_ms`, `repaint_ms`,
  `readback` total) — headless total is ~11 ms, dominated by the wasm fetch.

---

## Context

The WebGL2 renderer in `client/src/renderer.rs` is architecturally complete
(765 lines): glyph atlas, two-pass instanced drawing, selection/cursor
handling, and resize support all exist. Background rendering works —
headless Chromium tests pass for colors, dash seams, pipe seams, and block
seams. But the text pass (glyph quads sampling the atlas alpha) never renders
in the real browser. Three root causes stand between "code exists" and
"working WebGL2 terminal."

---

## Root Causes

### A. Initialization order: Canvas 2D always wins
`state.rs:234` calls `canvas_get_2d(canvas)` first. Since every browser
supports Canvas 2D, this always succeeds, permanently claiming the canvas
for 2D. `try_webgl` (`state.rs:236`) is then always `false`, so WebGL2
is never attempted.

### B. `render-test.html` loads the wrong WASM binary
`render-test.html:21` fetches `../pkg/terminal_client_bg.wasm` (the old
wasm-pack output). The current architecture builds raw WASM to
`target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm`. The
test harness never exercises the current renderer code.

### C. `render-test.html` doesn't test text rendering
The pixel analysis checks: bold blue, dark blue absence, dash seams, pipe
seams, block seams. None of these verify that ASCII glyphs actually render.
The text pass could be completely broken and the test would still pass.

---

## Tasks

### Task 1: Flip renderer initialization — WebGL2 first
**File:** `client/src/state.rs`
**Lines:** 220–252

Change `TerminalState::new()` to try WebGL2 first, fall back to Canvas 2D:

1. Attempt `WebGL2Renderer::new(...)` first (requires cell dims from
   `measure_cell_dimensions_scratch()` or cached values — both already work
   without a 2D context on the real canvas).
2. If WebGL2 fails, attempt `canvas_get_2d(canvas)` as before.
3. If both fail, return the existing error.

No new FFI needed — scratch canvas measurement already avoids the real
canvas. The only change is the order of the two branches.

**Verify:** `cargo test -p terminal-client` passes; `cargo build` clean.

---

### Task 2: Update `render-test.html` to load correct WASM
**File:** `client/res/render-test.html`
**Line:** 21

Change the fetch URL from `../pkg/terminal_client_bg.wasm` to
`/pkg/terminal_client_bg.wasm` (matching the server route that serves
the embedded raw WASM from `target/wasm/`). Also update the JS import
path for `krust_runtime.js` to match the current raw-WASM pattern
(instead of the wasm-pack pattern).

**Verify:** `cargo build -p krust && render-check.sh` passes.

---

### Task 3: Add text-rendering assertions to `render-test.html`
**File:** `client/res/render-test.html`

After the existing dash/pipe/block seam checks, add a **text glyph
check**:

- Feed content that includes a known ASCII line (e.g. the word
  `"boldblue"` on row 0, or the spinner chars `⠋⠁⠂`).
- For each cell in that row, read pixels and verify that at least some
  pixels differ from the background color (i.e. glyph ink was drawn).
- Under WebGL2, use `gl.readPixels()` (already in the test); under
  Canvas 2D, use `getImageData()` (already in the test).
- Assert: every non-space cell in the text row has at least N pixels
  brighter than the background threshold.

This ensures the text pass (mode=1, atlas alpha sampling) is actually
producing visible output.

**Verify:** `render-check.sh` passes with the new assertion.

---

### Task 4: Add debug logging for WebGL2 initialization
**File:** `client/src/state.rs`

Add a `ffi::console_log` call when WebGL2 is successfully obtained and
when it fails, so browser DevTools shows which renderer was selected:

```
KRUST: WebGL2 renderer initialized (cols=80, rows=24, cell=8x18)
KRUST: WebGL2 unavailable, falling back to Canvas 2D
```

Also log any shader compilation or atlas build errors to the console.

**Verify:** Browser DevTools console shows the renderer choice on load.

---

### Task 5: Update ARCHITECTURE.md for current build system
**File:** `ARCHITECTURE.md`

- Section 1: Remove `wasm-bindgen` / `web-sys` / `js-sys` from the
  technology stack. Add raw WASM ABI + hand-rolled FFI.
- Section 3.3: Update state model description to reflect the new
  module layout (`state.rs`, `exports.rs`, `ffi.rs`, etc.).
- Section 7: Remove `wasm-pack test` and `wasm-pack build` commands.
  Update to `cargo build --release --target wasm32-unknown-unknown`.
- Section 8: Add missing files (`ffi.rs`, `exports.rs`, `state.rs`,
  `graphics.rs`, `color.rs`, `measure.rs`, `input.rs`, `query.rs`,
  `selection.rs`).

---

## Verification

After all tasks:
- `cargo test` — all workspace tests pass
- `cargo test -p terminal-client` — client tests pass
- `cargo build` — clean build (triggers WASM build via build.rs)
- `render-check.sh` — headless Chromium passes all checks including
  the new text glyph assertion
- Manual browser test: `cargo run`, open `http://localhost:3000`:
  - Text glyphs render correctly
  - Box-drawing / block elements tile seamlessly
  - Selection highlight works
  - Cursor block renders
  - Scrollback works
  - Resize re-renders cleanly
