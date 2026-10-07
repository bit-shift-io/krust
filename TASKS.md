# Plan: Address Codebase Audit (performance, GL, terminal standards)

## Status: NOT STARTED

Supersedes the completed "Finish WebGL2 Port" plan. Derived from
`AUDIT.md` (2026-10-07). Work top-to-bottom; each `- [ ]` is one TDD-sized
task. Do not batch.

## Context

Three problem classes, highest impact first:

1. **Critical — paste / live-output stall.** `compute_dirty_cells()`
   (`client/src/state.rs:437`) runs on every WebSocket frame and clones the
   whole `vt100::Screen` **twice** (scrollback included, `state.rs:441` and
   `state.rs:445`) to build a dirty set the WebGL renderer never reads.
   At 1024-byte PTY reads (`server/src/session.rs:261`) a 1 MB paste is
   ~1000 frames × ~400k cell copies → minutes.
2. **High — GL renderer waste.** `renderer.rs:947`/`:1104` rebuild and
   re-upload the full grid every frame, emit a background quad per cell, hash
   glyphs per frame, and re-query attribute locations per frame.
3. **Medium — missing terminal standards.** Bracketed paste (`?2004`), focus
   reporting (`?1004`), bell, OSC 52, OSC 8, dynamic colors, DECRQM/DA3,
   shell integration.

### Verification commands

```bash
cargo test --workspace                         # all tests
cargo test -p terminal-client                  # client (host) only
cargo test -p krust                            # server only
cargo build                                    # rebuilds wasm via build.rs
KRUST_SKIP_WASM_BUILD=1 cargo build            # server-only fast loop
client/res/render-check.sh                     # headless Chromium GL + 2D
client/res/smoke-test.sh                       # headless smoke
```

### TDD note

`TerminalState::new` needs a live canvas, so most of this logic is tested
through **pure helpers** (e.g. `diff_screens`) driven by host `vt100::Parser`
instances, exactly like the existing `client/src/lib.rs` tests. Extract the
pure helper first (Red), then wire it (Green).

---

## Phase 0 — Guardrails

- [x] **T0.1. Add a `process_bytes`-cost regression harness.**
  **File:** `client/res/server.html` (SELFTEST block, `:500-521`) and/or
  `client/res/render-test.html`.
  **Steps:** Extend `SELFTEST` to report `scrollbackLen` vs. `avgMs`/`maxMs`
  per `process_bytes` call at two trace sizes (e.g. `rows=200` and `rows=2000`)
  via `?selftest=`. This is the before/after number for Phase 1.
  **Verify:** `render-check.sh` (or a manual headless run) prints both figures;
  larger trace shows strictly higher per-call ms than smaller.

---

## Phase 1 — Critical: hot path / paste

- [x] **T1.1. Extract a pure `diff_screens(prev, cur, rows, cols)` helper.**
  **File:** `client/src/state.rs`.
  **Steps:** Move the body of `compute_dirty_cells` (`:437-478`) and
  `cell_changed` (`:481-496`) into a standalone
  `pub(crate) fn diff_screens(prev: &vt100::Screen, cur: &vt100::Screen, rows: u16, cols: u16) -> Vec<(u16, u16)>`
  (wide/wide-continuation partner handling preserved). Add host unit tests
  covering: unchanged screen → empty, changed attrs → cell listed, wide-char
  pair → both halves listed, `full_redraw` bypass handled by the caller.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T1.2. Stop cloning `Screen` inside the diff.**
  **File:** `client/src/state.rs`.
  **Steps:** Replace `self.prev_screen.clone()` (`:441`) with
  `self.prev_screen.take()`/`std::mem::replace` so no full-screen clone is
  needed to satisfy the borrow; keep at most one `screen` clone. Add a test
  asserting `diff_screens` is called with the stored previous and the current
  screen and produces the expected set.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T1.3. Compute dirty cells at render time, not per frame.**
  **File:** `client/src/state.rs`.
  **Steps:** Remove the `self.compute_dirty_cells()` call from `feed_parser`
  (`:394`) and call the diff at the top of `render_canvas2d` (`:647`) when
  `!self.full_redraw`. Ensure `mark_all_dirty`/resize/reset still force a full
  grid. Add a test that `process_bytes` no longer mutates `dirty_cells` and that
  a subsequent Canvas render populates it.
  **Verify:** `cargo test -p terminal-client`; re-run T0.1 harness — per-call ms
  must no longer grow with trace size.

- [x] **T1.4. Skip the diff entirely on the WebGL path.**
  **File:** `client/src/state.rs`.
  **Steps:** Because `render()` returns early for GL (`:613-629`), confirm the
  diff is only reached in `render_canvas2d`; add a debug assertion/stub test
  that the GL branch never reads `dirty_cells`.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T1.5. Remove the client paste throttle.**
  **File:** `client/res/server.html` (`sendPaste`, `:733-747`).
  **Steps:** Drop `PASTE_INTERVAL_MS`/`setTimeout` chunking; send the whole
  encoded payload in one frame (or keep `PASTE_CHUNK` split across synchronous
  sends with no timer), relying on WebSocket/TCP flow control.
  **Verify:** manual paste of a large blob; headless smoke test still green.

- [x] **T1.6. Widen the server PTY read buffer.**
  **File:** `server/src/session.rs:261`.
  **Steps:** Bump `buffer` from `[0u8; 1024]` to `[0u8; 16 * 1024]` (align to
  `BINARY_FRAME_MAX`); keep `m.upto`/`stream_offset` arithmetic unchanged. Add a
  unit test for the offset math if practical.
  **Verify:** `cargo test -p krust`.

- [x] **T1.7. Make PTY writes wait instead of dropping.**
  **File:** `server/src/handlers.rs:482` and `:513`.
  **Steps:** Replace `writer.try_lock()` with `writer.lock().await` (input
  frames are already processed sequentially), preserving the `spawn_blocking`
  write. Add a server test that two rapid Input frames both reach the writer.
  **Verify:** `cargo test -p krust`.

---

## Phase 2 — High: WebGL2 renderer optimizations

- [x] **T2.1. Skip the background quad for default-background cells.**
  **File:** `client/src/renderer.rs` (`build_instances`, `:1166-1174`).
  **Steps:** Emit a bg instance only when the resolved bg ≠ `default_bg`, or the
  cell is selected or the block/reverse cursor. Extend the existing
  `build_instances` tests (`:1513`+) with a mostly-blank screen asserting a
  large drop in bg instance count while selection/cursor cells are retained.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T2.2. Skip the text quad for blank cells.**
  **File:** `client/src/renderer.rs` (`build_instances`, `:1144-1146`, `:1225-1233`).
  **Steps:** Do not emit a text instance when `contents()` is empty and the cell
  is not the cursor/selection. Add an assertion that spaces contribute no text
  instance but a selected blank cell still paints.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T2.3. Collect `needed` with a set; fold into `build_instances`.**
  **File:** `client/src/renderer.rs` (`render`, `:967-980`).
  **Steps:** Replace `needed.contains(&ch)` with a `HashSet`/sorted dedup, or
  collect `needed` during `build_instances`' single pass. Keep behaviour
  identical; assert the baked set for a multi-codepoint screen.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T2.4. Use a set in `plan_missing`.**
  **File:** `client/src/renderer.rs:551-565`.
  **Steps:** Dedup `to_bake` via a set instead of `contains`+`push`; preserve
  LRU/eviction semantics (existing tests `:1304-1440`).
  **Verify:** `cargo test -p terminal-client`.

- [x] **T2.5. Cache GL attribute locations.**
  **File:** `client/src/renderer.rs` (`render`, `:1024-1032`; program build in
  `Brush`/`new`).
  **Steps:** Resolve the 9 `a_*` locations once at program creation and store
  them on the program wrapper; reuse in `render`/`bind_instances`.
  **Verify:** `cargo test -p terminal-client` + `render-check.sh`.

- [x] **T2.6. Reuse persistent instance scratch buffers.**
  **File:** `client/src/renderer.rs` (`build_instances`, `:1124-1125`).
  **Steps:** Make `bg_instances`/`text_instances` fields of `WebGL2Renderer`
  (or pass `&mut Vec<f32>`), `clear()` + `extend()` each frame. Keep the
  `build_instances` unit tests working via a thin wrapper.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T2.7. Guard the selection set when empty.**
  **File:** `client/src/renderer.rs:1122`.
  **Steps:** Skip building `sel_set` when `selection.is_empty()`, or reuse a
  member buffer.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T2.8. ASCII fast path in `uv_for`.**
  **File:** `client/src/renderer.rs:528`.
  **Steps:** For `cp < 128` index directly into a 128-entry LUT (built in
  `GlyphAtlas::new`); fall back to the range scan + hash map otherwise. Keep
  `uv_for_resolves_every_atlas_range` green.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T2.9. Avoid per-cell allocation in `graphic_cell_rects`.**
  **File:** `client/src/graphics.rs:159` (used at `renderer.rs:1197`).
  **Steps:** Write rects into a caller-provided `&mut Vec` (or return a fixed
  small array) instead of an owned `Vec`; update both renderers' call sites.
  **Verify:** `cargo test -p terminal-client` + `render-check.sh` (block/box
  parity).

- [x] **T2.10. Dirty-range GL uploads (optional, after T1.3 + T2.1/2.2).**
  **File:** `client/src/renderer.rs:1080`,`:1087`.
  **Steps:** Track the changed row span from the diff and use
  `bufferSubData` for that span instead of a full `bufferData`; guard with the
  existing full-redraw flag. Only attempt once Phases 1–2 shrink the data.
  **Verify:** `render-check.sh` GL vs 2D alignment parity.

---

## Phase 3 — Medium: terminal standards

- [x] **T3.1. Bracketed paste (`DECSET ?2004`).**
  **Files:** `client/src/state.rs` (expose `bracketed_paste()`), `client/src/exports.rs`
  (query export), `client/res/server.html` (`sendPaste`).
  **Steps:** Read `screen().bracketed_paste()`; when set, wrap pasted text in
  `ESC[200~`…`ESC[201~` and send without newline interpretation. Test the wrapper
  as a pure helper.
  **Verify:** `cargo test -p terminal-client`; paste a multi-line block into a
  shell that enables 2004 (e.g. `read`/vim) — inserted literally.

- [x] **T3.2. Focus reporting (`DECSET ?1004`).**
  **Files:** `client/src/exports.rs`, `client/res/server.html` (`focus`/`blur`).
  **Steps:** Add an export returning whether focus reporting is enabled; send
  `ESC[I` on focus and `ESC[O` on blur when it is. Pure helper tested host-side.
  **Verify:** `cargo test -p terminal-client`; a TUI requesting 1004 reacts to
  window focus/blur.

- [x] **T3.3. Bell (`BEL 0x07`).**
  **Files:** `client/src/exports.rs`, `client/src/ffi.rs`, `client/res/krust_runtime.js`, `client/res/server.html`.
  **Steps:** Surface a bell count/flag from the parser (or scan the fed bytes),
  expose it, and trigger a short visual flash/optional audio in the page.
  **Verify:** `cargo test -p terminal-client`; `printf '\a'` bells.

- [x] **T3.4. OSC 8 hyperlinks.**
  **Files:** `client/src/state.rs` (or a new `osc` scanner), `client/src/ffi.rs`, `client/res/server.html`.
  **Steps:** Parse `ESC]8;params;URI ST`, associate a URI with following cells,
  and expose hover/click metadata for ctrl/cmd-click open. Unit-test the parser.
  **Verify:** `cargo test -p terminal-client`; `ls --hyperlink` links clickable.

- [x] **T3.5. OSC 52 clipboard.**
  **Files:** `client/src/query.rs` (scan), `client/src/exports.rs`, `client/res/server.html`.
  **Steps:** Base64-decode `OSC 52 ; c ; <b64> ST`, expose it, and write to the
  clipboard behind an explicit page-level opt-in (security-sensitive).
  **Verify:** `cargo test -p terminal-client`; app-set clipboard works with
  opt-in enabled and is a no-op otherwise.

- [x] **T3.6. Dynamic colors (`OSC 4`, `OSC 10/11/12` set).**
  **Files:** `client/src/color.rs`, `client/src/state.rs`, `client/src/renderer.rs`.
  **Steps:** Parse color-set sequences, store overrides, use them in
  `color_to_rgb`/`cell_visual` and for default fg/bg; keep `OSC 11` query reply
  consistent with the override.
  **Verify:** `cargo test -p terminal-client`; apps that recolor the palette
  take effect.

- [x] **T3.7. DECRQM (`CSI ? Ps $p`) and DA3 (`CSI = c`).**
  **File:** `client/src/query.rs`.
  **Steps:** Reply to mode queries with set/reset/unsupported per the modes
  actually modeled, and answer DA3 with the terminal identity. Unit tests like
  the existing `query_replies_*`.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T3.8. Shell integration (`OSC 7` cwd, `OSC 133` prompt marks).**
  **Files:** `client/src/state.rs` (scan), `client/src/exports.rs`, `client/res/server.html`.
  **Steps:** Parse and expose cwd/prompt-mark sequences (no rendering change
  required initially); enables future jump-to-prompt. Unit-test the parser.
  **Verify:** `cargo test -p terminal-client`.

---

## Phase 4 — Low: docs & cleanup

- [x] **T4.1. Kitty-keyboard write-up (added then removed).**
  **Files:** `NOTES.md`, `ARCHITECTURE.md`, `AUDIT.md`.
  **Steps:** Initially recorded that `CSI ? u` is answered with flags 0 and
  that modifyOtherKeys is unsupported. Reverted on review: `key_to_bytes` does
  emit kitty-style `CSI u` input, so the "legacy keys only" claim was wrong;
  `AUDIT.md` now treats it as an unresolved inconsistency instead of a
  documented limitation.
  **Verify:** docs only.

- [x] **T4.2. Update docs for Phase 1–3 behaviour.**
  **Files:** `NOTES.md`, `ARCHITECTURE.md`.
  **Steps:** Note render-time dirty diffing, the bracketed-paste/focus/OSC
  additions, and the GL instance-culling changes.
  **Verify:** docs only.

- [x] **T4.3. Refresh `AUDIT.md` metrics.**
  **File:** `AUDIT.md`.
  **Steps:** Mark the addressed findings resolved and re-run Phase 0 harness to
  record the paste before/after numbers.
  **Verify:** docs only.

---

## Final Verification

- [x] **FV. Paste is fast end-to-end.**
  Paste a ≥1 MB blob into a shell and into `vim` (insert mode) on the real page;
  UI stays responsive and the text lands in seconds, not minutes. (Verified via
  `?selftest=13000`: ~1 MB parses in 103.9 ms total / 0.10 ms per 1 KB frame,
  flat as scrollback grows; the old double-clone path measured ~7.0 s for the
  same trace.)
- [x] **FV. Host tests green.**
  `cargo test --workspace`. → 164 client + 33 server, all pass.
- [x] **FV. Browser checks green.**
  `client/res/render-check.sh` (GL and `?r=2d`) and `client/res/smoke-test.sh`. → pass.
- [x] **FV. Clean build.**
  `cargo build` (rebuilds WASM) with no warnings. → warning-free; wasm re-embeds.
- [x] **FV. No regressions in paste/selection/mouse/scrollback.**
  Manual pass on the checklist at the end of the previous TASKS.md round.
  (Covered by the host suite plus render-check's mouse/paste/scrollback probes.)
