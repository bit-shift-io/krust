# Codebase Audit Summary

**Audit Target:** `krust` — Rust/WASM terminal emulator client + Axum/`portable-pty` server
**Date:** 2026-10-07

---

## Executive Summary

The architecture is sound and unusually well-documented, but the hot path has a
severe performance defect: **every WebSocket frame clones the entire `vt100`
screen twice, scrollback included, to compute a dirty-cell set that the WebGL
renderer never reads.** This is the dominant cost behind the reported
minutes-long paste and heavy live output. Two secondary themes emerge: the GL
renderer rebuilds and re-uploads the whole grid unconditionally every frame, and
several common terminal standards (bracketed paste, focus reporting, OSC 52,
OSC 8, bell) are not implemented. None of the fixes below are structural
rewrites; the highest-value ones are localized.

## Key Metrics

- **Unused/Orphan Files:** 0 found (all modules reachable from `lib.rs`; assets embedded via `include_str!`/`include_bytes!`).
- **Dead Functions/Exports:** 0 found.
- **Commented-Out Code / Debug Logs:** 0 (`console.log` only in the `SELFTEST` diagnostic path).
- **Open TODOs/FIXMEs:** 0 in source (tracked narratively in `NOTES.md` / `TASKS.md`).

---

## Resolution Status (2026-10-07)

All findings below are addressed. Tracked in `TASKS.md` (phases 0–4); the
performance claims were re-verified with the in-repo harness after the fix.

- **Finding 1 (paste/live-output, CRITICAL) — resolved.** Dirty diffing moved to
  render time (`DirtyTracker`) and skipped entirely on the GL path; the
  remaining clone removed via `prev_screen.take()`; server PTY read buffer
  widened to 16 KiB; client paste throttle removed; PTY writes wait on the lock
  (`pty_write_locked`) instead of dropping.
  - **Harness (`server.html?selftest=N`, headless Chromium, 1024-byte chunks):**
    after = **0.10 ms/frame** at 1029 frames (≈1 MB, 103.9 ms total) and
    **0.086 ms/frame** at 10284 frames (≈10 MB, 892.9 ms total); per-frame cost
    is flat with scrollback (no growth from run 1 to run 2).
  - **Before:** a benchmark of the old path (two `Screen::clone()`s per 1 KB
    frame at 50×200/1024 scrollback) measured **6.81 ms/frame → ≈7.0 s per
    1 MB paste**, i.e. ~68× the current per-frame cost, before any parse work.
- **Finding 2 (WebGL2 waste, HIGH) — resolved.** Default-bg cells emit no bg
  quad, blank cells no text quad; `HashSet`-based needed/dedup; cached attribute
  locations (`AttrLocations`); reused instance scratch buffers; selection set
  skipped when empty; `GraphicCellRects` fixed array; ASCII UV LUT; changed-span
  `bufferSubData` uploads.
- **Finding 3 (missing standards, MEDIUM) — resolved.** Bracketed paste
  (`?2004`), focus reporting (`?1004`), `OSC 52` (opt-in), `OSC 8`, bell,
  dynamic colors (`OSC 4`/`10`/`11`, query reply kept consistent), `DECRQM`,
  `DA3`, and `OSC 7`/`OSC 133` shell integration all implemented.
- **Finding 4 (docs/complexity, LOW) — documented.** `renderer.rs`/`state.rs`/
  `server.html` remain large but cohesive; split left optional. The kitty
  keyboard / `modifyOtherKeys` limitation is now written up in `ARCHITECTURE.md`
  (§9) and `NOTES.md`.
- **Tests:** `cargo test --workspace` = 164 client + 33 server, all green;
  render regression checks pass on both `?r=2d` and `?r=gl`.

---

## Findings & Recommendations

### 1. Paste slowness and live-output cost (CRITICAL)

**Root cause (confirmed by code path).** `client/src/state.rs:437`
`compute_dirty_cells()` runs on **every** `process_bytes` call
(`state.rs:354` → `feed_parser` → `state.rs:394`), i.e. once per inbound
WebSocket frame, and:

- `state.rs:441` clones the stored previous screen — `self.prev_screen.clone()` —
  purely to satisfy the borrow checker.
- `state.rs:445` clones the current screen — `self.parser.screen().clone()`.
- `state.rs:450-473` then diffs the full `rows × cols` grid.

`vt100::Screen` derives `Clone` and owns **two** `Grid`s (`screen.rs:54-65`),
each with its own `scrollback: VecDeque<Row>` (`grid.rs:3-15`). At 200×50 with a
full 1024-row scrollback, one clone copies ~200k `Cell`s (each with a heap
`String`), so **two clones ≈ 400k allocations per frame**. The PTY reader feeds
the server 1024-byte reads (`server/src/session.rs:261`), so a 1 MB paste echo
arrives as ~1000 frames → ~400M cell copies. That is the minutes-scale stall.

**Aggravators.**
- The WebGL renderer **ignores `dirty_cells` entirely** (`state.rs:613-629`;
  `renderer.rs:947`); the Canvas 2D renderer is the only consumer
  (`state.rs:674`). The entire clone+diff is therefore pure waste on the default
  GL path.
- Client `sendPaste` throttles to 4096 B / 16 ms ≈ **256 KB/s**
  (`client/res/server.html:733-747`), an artificial floor unrelated to real
  backpressure.
- The server input task `try_lock()`s the PTY writer (`handlers.rs:482`,
  `handlers.rs:513`) and silently drops bytes on contention (currently rare,
  since only that task locks it, but latent data loss).

**Recommendations.**
1. **Move the diff to render time.** Compute dirty cells inside `render()` (once
   per rAF) instead of `process_bytes()` — coalescing is already in place
   (`scheduleRender`), so this removes the per-frame amplification by itself.
2. **Skip it on the GL path entirely.** GL does not consume the set; only
   Canvas 2D needs it.
3. **Stop cloning `Screen` to diff.** Compare via `prev_screen.take()` /
   `std::mem::replace` so the borrow is not needed, and retain only the visible
   region (rebuild a `Parser::new(rows, cols, 0)` snapshot) rather than dragging
   scrollback. Better still, drive Canvas 2D from `vt100::Screen::state_diff`,
   which already emits a minimal repaint stream.
4. Remove the 16 ms paste delay (WebSocket + TCP already flow-control), and bump
   the server read buffer from 1024 B to 16 KB (`session.rs:261`) to cut frame
   count ~16×.
5. Replace `try_lock` drop-on-contention with `lock().await` (the write task is
   already sequential, so waiting is correct).

*Verification tool already in the repo:* `server.html:500-521` `SELFTEST` times
each `process_bytes` over 1024-byte chunks while `rows` lines are parsed.
Running it (`?selftest=N`) at increasing N shows per-call latency rising with
scrollback — the exact regression.

### 2. WebGL2 renderer optimizations (HIGH)

`renderer.rs:947` `render()` and `renderer.rs:1104` `build_instances()` recompute
and re-upload the entire grid every frame regardless of what changed:

| Issue | Location | Impact | Fix |
|:---|:---|:---|:---|
| A full-cell background quad is emitted for **every** cell | `renderer.rs:1166-1174` | ~rows×cols instances even though `gl_clear` already paints `default_bg` | Emit bg quads only when resolved bg ≠ default, or on selection/cursor cells |
| Text quad emitted for blank cells | `renderer.rs:1144-1146`, `1225-1233` | one instance per space | Skip when `contents()` is empty and not cursor/selection |
| `needed` scan is O(cells × unique) via `Vec::contains` | `renderer.rs:967-980`, `:973` | quadratic on CJK/emoji screens | Use a `HashSet`, or fold into the `build_instances` pass |
| `plan_missing` uses `to_bake.contains` then `push` | `renderer.rs:551-565` | O(n²) glyph-baking plan | `HashSet` for `to_bake` |
| 9 `gl_get_attrib_location` calls **per frame** | `renderer.rs:1024-1032` | redundant JS round-trips | Cache locations at program creation |
| Two fresh `Vec<f32>` allocated per frame | `renderer.rs:1124-1125` | wasm GC/alloc pressure | Reuse persistent scratch buffers (`clear()` + `extend`) |
| `sel_set` HashSet built even when selection empty | `renderer.rs:1122` | needless alloc | Early-out / reuse buffer |
| `graphic_cell_rects` returns an owned `Vec` per call | `graphics.rs`, used `renderer.rs:1197` | alloc per graphic cell | Return `SmallVec`/array or write into an out-param |
| `uv_for` scans ranges + hashes for ASCII | `renderer.rs:528` | per-cell overhead | Direct 128-entry LUT fast path |
| No dirty-range updates; full `bufferData` each frame | `renderer.rs:1080`, `:1087` | full re-upload | After dirty tracking is restored, `bufferSubData` changed spans (or at minimum hoist the two rows above to shrink the upload) |

### 3. Missing terminal standards (MEDIUM)

`vt100` covers the core; these are unimplemented in the client:

| Standard | State | Notes / Recommendation |
|:---|:---|:---|
| **Bracketed paste** (`DECSET 2004`) | Missing | `vt100` exposes `screen().bracketed_paste()` (used in `server/src/replay.rs:138`), but `sendPaste` (`server.html:735`) always sends raw text. Multi-line paste executes line-by-line in shells and misbehaves in vim/nano. Wrap in `ESC[200~`/`ESC[201~` when the mode is set. |
| **Focus reporting** (`DECSET 1004`, `CSI I`/`CSI O`) | Missing | `server.html:208` handles `focus` only to repaint; TUIs (vim, tmux, opencode) that request 1004 never receive events. Forward on `focus`/`blur` when the mode is enabled. |
| **OSC 52 clipboard** | Missing | Apps cannot set the system clipboard. Security-sensitive; needs opt-in. |
| **OSC 8 hyperlinks** | Missing | Links render as plain text; no ctrl/cmd-click open. |
| **Bell** (`BEL 0x07`) | Missing | No audible/visual bell. |
| **Dynamic colors** (`OSC 4`, `OSC 10/11/12` sets) | Partial | `OSC 11` is *queried* and answered with a constant (`query.rs`), but set requests never reach the renderer. |
| **DECRQM / DA3** (`CSI ? Ps $p`, `CSI = c`) | Missing | Some apps probe mode support; unanswered = assume unsupported. |
| **Kitty keyboard / modifyOtherKeys** | Deliberately unsupported | `query.rs` answers `CSI ? u` with flags 0; XTGETTCAP may still advertise `km`. Intentional, but document it. |
| **OSC 133 shell integration / OSC 7 cwd** | Missing | Prompt marking / cwd tracking absent. |

### 4. Comments, docs, and complexity observations (LOW)

- Documentation quality is high and mostly current; `NOTES.md` correctly records
  the focus-loss root cause. No stale comments found.
- File/function sizes exceed the skill's thresholds (`renderer.rs` 1595 lines;
  `state.rs` 1346; `handlers.rs` 540; `server.html` 1010), but the code is
  cohesive and heavily sectioned; splitting is optional, not debt.
- `handlers.rs:474` `ws_recv_task` serializes every input frame through
  `spawn_blocking(...).await`, one at a time. Correct, but a batching writer
  would reduce per-frame overhead during paste.

---

## Top Priority Action Plan

1. **[Critical]** Stop cloning `vt100::Screen` per WebSocket frame: compute dirty
   cells only at render time, and skip it on the GL path (`state.rs:394`,
   `state.rs:437-477`). This is the paste fix.
2. **[High]** Cut GL instance count: skip default-bg and blank-cell quads; cache
   attribute locations; reuse scratch buffers
   (`renderer.rs:947-1243`).
3. **[High]** Remove the client paste throttle and widen the server PTY read
   buffer (`server.html:733-747`, `server/src/session.rs:261`); make PTY writes
   wait, not drop (`handlers.rs:482`, `handlers.rs:513`).
4. **[Medium]** Implement bracketed paste and focus reporting — the two standards
   users hit immediately.
5. **[Low]** Document the deliberate kitty-keyboard limitation.
