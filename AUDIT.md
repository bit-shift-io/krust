# Codebase Audit Summary

**Audit Target:** `krust` (Rust terminal emulator: `server/` Axum+PTY + `client/` raw-WASM)
**Date:** 2026-09-26
**Method:** read-only static review. All dependency-removal claims below were empirically
verified in a throwaway copy of the repo at `/tmp` (`cargo tree` crate-count deltas +
`cargo test --workspace`).

> **Status:** the recommended cleanup has been **applied** (committed as `bfeb961`). See
> [Outcome](#outcome-of-the-cleanup) at the end for final measured numbers.
> The findings below are preserved as the pre-cleanup baseline.
>
> A second, separate investigation followed: a **phantom-glyph rendering bug** in the WebGL2
> path. See [Phase 2](#phase-2--phantom-glyph-investigation-webgl2).

---

## Executive Summary

The dependency surface is already lean — 7 direct deps across two crates, no `wasm-bindgen`,
no font crates, no `async_trait`. Three of the seven are genuinely removable or reducible,
and one (`tokio`'s `features = ["full"]`) is a free win that drops 6 transitive crates for
a one-line manifest change.

The headline finding: **`serde_json` in the WASM client is pure overhead.** It is used at
exactly 4 call sites, all of which build or parse fixed-shape objects. Removing it drops the
client dependency tree from 10 crates to 7 and shrinks the shipped `.wasm` by **~50 KB (16.5%)**.
That is the single highest-value change available.

The second finding: the `tokio-tungstenite` dev-dependency is **redundant** — `axum`'s `ws`
feature already depends on the exact same `tokio-tungstenite 0.24`. It costs nothing but is
misleading, as it implies a second WebSocket stack exists.

`tokio = { features = ["full"]` is over-broad; the code touches none of `signal`, `process`,
`fs`, or `time`. Trimming to four explicit features removes `parking_lot`, `signal-hook-registry`,
`lock_api`, `parking_lot_core`, `scopeguard`, and `errno` from the build graph.

Separately from dependencies: `cargo clippy` reported **23 warnings**, one dead binding
(`handlers.rs:189`), two stale directories from the pre-raw-WASM era (`client-wasm/`, and a
`setup.sh` that installs `wasm-pack` — a tool the project no longer uses), and a
set of docs describing a superseded state of the project.

> A later bug report (phantom characters in the WebGL2 render path) is covered separately in
> [Phase 2](#phase-2--phantom-glyph-investigation-webgl2). It is unrelated to the dependency
> findings above but is the reason two WebGL2 defects are now covered by tests.

## Key Metrics

- **Unused/Orphan Files:** 3 (`client-wasm/`, `setup.sh`, `client/res/slow.png`)
- **Redundant Dependencies:** 1 (`tokio-tungstenite` dev-dep, already in graph via `axum`)
- **Removable Dependencies:** 1 (`serde_json` from client); **Reducible:** 2 (`tokio` features, `futures-util` features)
- **Dead Functions/Exports:** 1 (`_sid` in `handlers.rs:189`)
- **Clippy Warnings:** 23 → **5** (18 auto-fixed; the 5 remaining are the deliberate complexity findings)
- **Commented-Out Code / Debug Logs:** 0 (the `console_log` calls in `state.rs`/`exports.rs` are
  deliberate diagnostics — `KRUST:` for renderer-selection info, `krust panic:` for the Phase 2
  panic hook — not leftovers)
- **Open TODOs/FIXMEs:** 0

Phase 2 added:

- **WebGL2 correctness bugs fixed:** 2 (stale glyph after a failed atlas bake; `unwrap_or` zero-UV
  fallback painting an unrelated glyph)
- **Silent-failure defects fixed:** 2 (wasm panics reaching JS with no message and being
  swallowed by `catch (_) {}`; `init` discarding the real error text)
- **Hypotheses tested and ruled out:** 5
- **New tests:** 2 (both in `client/src/renderer.rs`)
- **Test total:** 78 → **80**
- **WASM cost of the fixes:** +2,487 B (**+1.0%**); net vs original baseline still **−15.7%**
- **New clippy warnings:** 0

### Dependency graph: measured crate-count deltas

Server graph is `normal + build + dev`. All figures measured with `cargo tree`.

| Change | Server graph | Client graph | WASM size |
| :--- | :--- | :--- | :--- |
| Baseline | 99 crates | 10 crates | 301,901 B |
| Drop `serde_json` from client | 99 | **7** (−3) | **251,943 B (−49,958 B, −16.5%)** |
| Trim `tokio` features | **93** (−6) | 10 | — |
| Lean `futures-util` features | **92** (−1) | 10 | — |
| **All applied (tower-http kept)** | **92 (−7, −7.1%)** | **7 (−3, −30%)** | **−16.5%** |
| *Also drop `tower-http` (not recommended)* | *91 (−8)* | *7* | *−16.5%* |

Crates removed server-side: `errno`, `futures-macro`, `lock_api`, `parking_lot`,
`parking_lot_core`, `scopeguard`, `signal-hook-registry`.
Crates removed client-side: `serde_json`, `serde_core`, `zmij`.
All tests pass with every change applied.

> **Note on the WASM figure.** An earlier draft of this report measured −18.0% from a
> throwaway prototype whose number parser was a naive `str::find`. The shipped
> `json_number_field` handles exponents, whitespace, and key ordering, which costs ~4 KB
> more but is correct. **16.5% is the real number.**

---

## Findings & Recommendations

### 1. Dependencies

| # | Change | Verdict | Evidence / Risk |
| :--- | :--- | :--- | :--- |
| 1 | Drop `serde_json` from `client/Cargo.toml`; hand-roll 4 JSON sites | **Done** | 10→7 client crates; wasm −16.5%. 67 client tests pass. See caveats below. |
| 2 | `tokio` `features = ["full"]` → `["macros", "rt-multi-thread", "net", "sync"]` | **Done** | Removes 6 crates. Verified `cargo test -p krust` passes. Zero `signal`/`process`/`fs`/`time` usage in the codebase. |
| 3 | Remove `tokio-tungstenite` dev-dep | **Done** (with #4) | `cargo tree -i tokio-tungstenite` → `└── axum v0.7.9`. Already in the graph at the identical version. Removes 0 crates; removes a false implication. |
| 4 | Delete `server/examples/wsprobe.rs` (220 lines) | **Done** | Unreferenced by any script, doc, or Cargo target. A manual `//!`-documented debug probe. Recoverable from git history if wanted back. |
| 5 | Lean `futures-util` → `default-features = false, features = ["sink", "std"]` | **Done**, low value | Compiles clean; drops `futures-macro` (proc-macro). `sink` is mandatory — `SinkExt::send` needs it. |
| 6 | Drop `tower-http`, hand-roll CORS | **Rejected** | Works (~20-line `axum::middleware::from_fn`; 11 tests pass) but removes only the 1 crate — **zero** transitive savings, since `axum` already pulls `http`, `http-body`, `tower-layer`, `tower-service`, `tracing`, `base64`. Trades a battle-tested preflight implementation for hand-rolled CORS bug risk. Poor trade. |
| 7 | Remove `futures-util` entirely | **Rejected** | `axum` depends on it anyway, so 0 crate savings. `socket.split()` needs `StreamExt`+`SinkExt`; switching to axum's native `WebSocket::send`/`recv` requires an `Arc<Mutex<WebSocket>>` — *more* code for no gain. |

#### Caveats for #1 (the `serde_json` removal)

The 4 call sites in `client/src/exports.rs` are: parse `{"w":f,"h":f}`, build the
`init` config object, build the `process_bytes` summary, and build `handle_click`'s
`{row, col}`. Replacing `json!` with `format!` is mechanical, but:

- **Float formatting differs.** `json!(8.0f64)` emits `8.0`; `format!("{}", 8.0f64)` emits `8`.
  Both are valid JSON and both `=== 8` in JS, and the only consumer
  (`server.html:399-400`, `cfg.cell_width || 8`) is unaffected. Safe, but worth knowing.
- **NaN/Inf would produce invalid JSON.** `format!` emits bare `NaN`/`inf`; `serde_json` emits
  `null`. Handled explicitly: the `json_f64` helper maps non-finite to `null`, matching
  `serde_json`.
- **`canvas_id` must be escaped.** The other three sites are all numeric/bool and need no
  escaping; the `init` payload interpolates a caller-supplied string and requires the
  `json_string` helper.

The `serde_json` dep stays on the **server**, where it is genuinely load-bearing
(`ClientMessage` deserialization in `handlers.rs:194`).

### 2. Unused Files & Dead Code

| File Path | Type | Details | Recommended Action |
| :--- | :--- | :--- | :--- |
| `client-wasm/` | Unused Directory | 196 KB of stale `wasm-bindgen` output (`terminal_client.js`, `terminal_client.d.ts`, `terminal_client_bg.wasm`) from before the raw-WASM migration. Git-ignored, zero references. **Its `terminal_client_bg.wasm` was a decoy** — it looked like a build artifact but was 3 months stale and unrelated to `target/wasm/`. | **Deleted** |
| `setup.sh` | Stale Script | Installed `wasm-pack` — a tool the project explicitly no longer uses (`AGENTS.md:77`: "No wasm-bindgen CLI or JS glue is required"). The script's only action was a pointless ~2 min install. | **Deleted** |
| `client/res/slow.png` | Unused Asset | Zero references anywhere in the repo. | **Deleted** |
| `server/examples/wsprobe.rs` | Unused Dev Tool | See dependency finding #4. | **Deleted** |
| `server/src/handlers.rs:189` | Dead Binding | `let _sid = session_id.clone();` — never read. | **Deleted** |
| `NOTES.md` | Empty Document | Header only, no content — yet `AGENTS.md` listed it as "Design rationale and key decisions". | Kept as a stub; `AGENTS.md` row now says so |

### 3. Code Structure & Complexity Smells

| File Path | Issue | Context / Severity | Suggested Refactor |
| :--- | :--- | :--- | :--- |
| `client/src/renderer.rs` | File too large | 1,270 lines at audit time; **1,369** after Phase 2. Largest in the project. Medium | Split the WebGL2 backend out of `renderer.rs` (glyph atlas / instance building / draw loop are already separable) |
| `client/src/state.rs` | File too large | 925 lines, plus the project's longest function. Medium | Extract `TerminalState` renderer-agnostic core from the render path |
| `client/src/state.rs:720` | Function too long | `paint_cell` spans ~78 lines with a 10-arg signature. Medium | Extract bg-quad, glyph, and overlay passes into helpers |
| `client/src/renderer.rs:1010` | 13 parameters | `build_instances` — worst offender in the codebase. Medium | Bundle into a `CellMetrics`/`DrawParams` struct |
| `server/src/handlers.rs:124` | Function too long | `handle_socket` ~129 lines, 4 params. Low | Extract the two spawned task bodies into `fn`s |
| `client/src/ffi.rs:330` | 10 parameters | Exceeds clippy's 7-param threshold. Low | — |
| `client/src/state.rs:617` | 8 parameters | Exceeds threshold. Low | — |
| `client/src/graphics.rs:166` | Complex type | Clippy: "very complex type used". Low | Introduce a `type` alias |

> These 8 rows are the **5 clippy warnings that remain** after `--fix`, plus file-size and
> function-length notes that clippy does not check. They are **deliberately not auto-fixed**:
> each is a refactor of the rendering hot path, not a cleanup, and each is a judgment call
> about the right abstraction. Left for a deliberate pass.

### 4. Comments & Technical Debt

| File Path | Type | Snippet / Context | Recommendation |
| :--- | :--- | :--- | :--- |
| `AUDIT.md` | Stale Document | Claimed "Canvas 2D is temporarily the default", "28 client + 11 server tests", "`wasm-pack build` is clean". Reality: WebGL2 is primary, 67 + 11 tests, no wasm-pack anywhere. | **Rewritten** |
| `AGENTS.md` | Stale Comment | "The project is **not** a Git client. All references to 'Grit' in older documents (ARCHITECTURE.md, NOTES.md) are stale…" — `ARCHITECTURE.md` had since been rewritten, making the disclaimer itself stale. | **Deleted** |
| `AGENTS.md` | Stale / Duplicated Rows | The Key Files table listed the same `terminal_client.wasm` path twice, merged `session.rs`/`handlers.rs` into the `main.rs` row, and omitted `server/src/handlers.rs` entirely. | **Rewritten** |
| `AGENTS.md` | Stale Claim | "Tokio (`full` features)" and an implicit `serde_json` client dep. | **Updated** to state the explicit feature list and the no-`serde_json` rule, with the reason |
| `NOTES.md` | Stale Pointer | `AGENTS.md` advertised it as a design-rationale doc; the file is empty. | `AGENTS.md` row now says "(currently an empty stub)" |
| `server/build.rs:6` | Historical Note | "without a separate `wasm-pack build` stage" — accurate as history, mildly confusing as a present-tense docstring. | Left as-is; it explains *why* the build script exists |
| `client/src/handlers.rs` | Commented-Out Code | None found. | — |

### 5. Clippy Warnings (23 → 5)

| Category | Before | After | Locations |
| :--- | :--- | :--- | :--- |
| `needless_borrow` | 6 | 0 | auto-fixed (`selection.rs`, `lib.rs`) |
| `unnecessary_cast` (`u16`→`u16`) | 6 | 0 | auto-fixed (`renderer.rs`, `state.rs`) |
| `manual_div_ceil` | 1 | 0 | auto-fixed (`renderer.rs`) |
| `len() == 1` | 1 | 0 | auto-fixed (`state.rs`) |
| `redundant_closure` | 1 | 0 | auto-fixed (`state.rs`) |
| `clone` on `Copy` | 1 | 0 | auto-fixed (`state.rs`) |
| `collapsible_if` | 1 | 0 | auto-fixed (`state.rs`) |
| unit `let` binding | 1 | 0 | auto-fixed (`state.rs`) |
| `unused_mut` / unused var | 2 | 0 | auto-fixed / moot (`wsprobe.rs` deleted) |
| `too_many_arguments` | 4 | **4** | `renderer.rs:1010`, `state.rs:720`, `ffi.rs:330`, `state.rs:617` — see §3 |
| complex type | 1 | **1** | `graphics.rs:166` — see §3 |

All auto-fixed edits were reviewed and are semantically equivalent (`div_ceil`,
`!is_empty()`, dropping a redundant `as u16`, removing a needless borrow). No
behavioural change.

---

## Phase 2 — Phantom-Glyph Investigation (WebGL2)

**Trigger:** a user report that phantom letters/numbers appear in krust's input line while
running OpenCode — visually present, not submitted to the shell, and cleared by typing.

**Scope:** diagnosis only, plus fixes for defects found on the way. No dependency or
structural changes; the 5 remaining clippy findings are untouched.

### Bugs found and fixed

| # | Location | Defect | Symptom | Fix |
| :--- | :--- | :--- | :--- | :--- |
| A | `client/src/renderer.rs` `ensure_glyphs` | `assign_slots` registered a char in `dynamic_index` **before** its pixels reached the GPU. When `rasterize_dynamic` returned fewer bitmaps than chars, the `continue` left the reservation in place, so `uv_for` returned a texel still holding a **different** glyph — and `plan_missing` then saw the char as present, so it was never re-baked. | A permanently wrong character in a cell, with no way for it to self-correct. Exactly the reported "phantom letter". | New `release_slots` rolls the chunk's reservations back on failure, returning the chars to the not-yet-baked state so the next frame retries. |
| B | `client/src/renderer.rs` `build_instances` | A codepoint with no atlas entry fell back to `uv_for(ch).unwrap_or((0.0, 0.0, 0.0, 0.0))`, sampling the atlas's top-left texel. | An unrelated character painted at that cell. | Emit no glyph quad at all when `uv_for` returns `None`; the next frame re-bakes the glyph. |

Both are **WebGL2-only** failure modes. Canvas 2D has no atlas and no UVs, which is why the
`?r=2d` vs `?r=gl` split is the decisive A/B test for this class of bug.

Two tests were added for A (`a_failed_upload_releases_the_slot_instead_of_showing_a_stale_glyph`,
`release_slots_leaves_a_reassigned_slot_alone`); the second pins that a rollback must not evict
a char that has since taken over the same slot.

### Diagnosability defects found (these masked the bug)

| # | Location | Defect | Fix |
| :--- | :--- | :--- | :--- |
| C | `client/src/exports.rs`, every `server.html` call site | A wasm panic reaches JS as a bare `RuntimeError: unreachable` with no message, and every call site wraps exports in `try { ... } catch (_) {}`. A panic in the render path was therefore **completely silent** while the WebGL framebuffer went on presenting the last frame that finished drawing — which itself looks like stale text. | `install_panic_hook` logs the message and source location through `krust_console_log` (wasm-only; no-op on host, where the console import does not exist and tests rely on the default hook). |
| D | `client/src/exports.rs` `init` | `TerminalState::new(...).unwrap_or_else(\|_\| panic!("terminal init failed"))` discarded the underlying error, which already carried a specific message. | Panic with the real error text. This is what identified the missing-WebGL2 finding below. |

### Hypotheses tested and ruled out

| Hypothesis | Verdict | Evidence |
| :--- | :--- | :--- |
| Dirty-cell coalescing leaves stale cells | **Ruled out** | `repaint()` calls `mark_all_dirty()`, so every repaint is a full redraw. |
| Canvas 2D accumulates stale pixels | **Ruled out** | The 2D path clears the whole canvas and repaints every cell each frame. |
| WebGL instance buffer holds stale tail data | **Ruled out** | `gl_buffer_data_f32` uses `bufferData` (full realloc at exact size) each frame, and `drawArraysInstanced` uses the current instance count — not `bufferSubData` into a fixed capacity. |
| Glyph-atlas capacity thrashing | **Ruled out** | The dynamic region is `DYNAMIC_ROWS 32 × 32 cols` = **1024** slots. OpenCode's live set (braille spinner, box drawing in static ranges, occasional CJK) is far below that, so LRU eviction does not thrash. |
| WebGL context loss | Not reproduced | `rebuild_webgl()` already exists for `webglcontextrestored`; no loss observed in the headless runs. |

### Environment finding (not a code defect)

`client/res/render-check.sh` reports `renderer=gl: FAIL` on this machine because **headless
Chromium 152 provides no WebGL2 context at all**. Retried with `--enable-unsafe-swiftshader`,
`--use-gl=angle --use-angle=swiftshader`, and `--use-gl=swiftshader`; all three report
`webgl2 context unavailable`.

This was verified to be **pre-existing**: it reproduces identically with the Phase 2 changes
stashed. The GL half of the render regression suite simply cannot run here, and its failure
must not be read as a regression.

The GL path was instead validated in **Firefox 155**, which does provide software WebGL. Driving
it required a throwaway Marionette client (`--dump-dom` screenshots fire before the async wasm
init completes, and Chromium here has no GL), kept in `/tmp` — no repo files were changed for it.

### Verification

| Check | Result |
| :--- | :--- |
| `cargo test --workspace` | **80 pass** (11 server, 69 client — 2 new), 0 fail |
| `cargo clippy --workspace --all-targets` | **5 warnings** — the same deliberate complexity findings as §3; no new ones |
| `client/res/smoke-test.sh` (Firefox, production page) | **OK** — glyphs rendered |
| Firefox render-test `?r=2d` | **pass** — align `T{3,12} g{5,15} _{16,16} x{5,12}`, `ondemand` 4 cells, `braille` 8, 30 animation frame deltas |
| Firefox render-test `?r=gl` | **pass** — **identical** align metrics, `ondemand` 4, `braille` 8, 54 animation frame deltas |
| WASM size | 251,943 B → **254,430 B** (+2,487 B, **+1.0%**) for the panic hook and rollback path |
| `krust` release binary | 3,119,816 B, still fully self-contained |

> **Note on the +1.0% WASM growth.** The panic hook and the `release_slots` rollback are the
> entire delta. This is a deliberate trade: they convert a silent, self-perpetuating wrong-pixel
> bug into a logged, self-healing one. The 16.5% saving from Phase 1 is unaffected in relative
> terms (301,901 B → 254,430 B is still **−15.7%** against the original baseline).

### Open item — needs user confirmation

Bugs A and B are a mechanism that produces exactly the reported symptom, but the specific
glitch was **not reproduced end-to-end**. Confirming it is the same defect requires an A/B run
on the affected machine:

1. Load krust with `?r=2d`, then with `?r=gl`.
2. When a phantom appears, select and copy it.
3. If the clipboard **contains** the phantom text, OpenCode emitted real terminal state and this
   is not a rendering bug.
4. If phantoms appear **only** under `?r=gl` and are **absent** from the clipboard, it is bug A/B
   and this fix resolves it.

### Process note

One self-inflicted error is worth recording. The first draft of finding C inserted the new
function *between* `#[no_mangle]` and `pub extern "C" fn init`, so `#[no_mangle]` attached to
`install_panic_hook` instead — silently un-exporting `init` and breaking every page. It surfaced
immediately as `TypeError: fn is not a function` in the render test, and was caught by checking
the wasm **export section** rather than trusting a green build. `client/src/exports.rs` is now
the only file where a raw-WASM export is defined, so re-verify the export list after any edit
there:

```bash
python3 - <<'PY'
import sys
d=open("target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm","rb").read()
def uleb(b,i):
    r=s=0
    while True:
        x=b[i];i+=1;r|=(x&0x7f)<<s;s+=7
        if not x&0x80: return r,i
i=8
while i<len(d):
    sid=d[i];i+=1; size,i=uleb(d,i)
    if sid==7:
        n,j=uleb(d,i); out=[]
        for _ in range(n):
            ln,j=uleb(d,j); out.append(d[j:j+ln].decode()); j+=ln
            j+=1; _,j=uleb(d,j)
        print(len(out), "exports:", " ".join(sorted(out))); break
    i+=size
PY
```

---

## Outcome of the cleanup

Everything marked **Done** above is applied and verified:

- `cargo test --workspace` — **78 pass** (67 client incl. 8 new JSON-helper tests, 11 server), 0 fail.
- `cargo build --release` — clean; `wasm-opt` is not installed so the build script skips it.
- `cargo clippy --workspace --all-targets` — **5 warnings**, all the deliberate complexity findings in §3.
- Server graph 99 → **92** crates; client graph 10 → **7** crates.
- Shipped WASM 301,901 B → **251,943 B** (−49,958 B, **−16.5%**).
- `krust` release binary 3,117,392 B, still fully self-contained (HTML + runtime JS + WASM embedded).
- Committed as `bfeb961` ("audit and clean"). Phase 2 is uncommitted, for review.

## Top Priority Action Plan

1. **[High] Drop `serde_json` from `client/Cargo.toml`** — *DONE*. `json_string` / `json_f64` /
   `json_number_field` helpers added to `client/src/exports.rs` with 8 unit tests.
2. **[High] Trim `tokio` to `["macros", "rt-multi-thread", "net", "sync"]`** — *DONE*.
3. **[High] Delete `client-wasm/` and `setup.sh`** — *DONE*.
4. **[Medium] Delete `server/examples/wsprobe.rs` + the `tokio-tungstenite` dev-dep** — *DONE*.
5. **[Medium] Clear the 18 auto-fixable clippy warnings** — *DONE*.
6. **[Low] Reconcile the docs** (`AGENTS.md` Grit disclaimer + Key Files table, `NOTES.md` row,
   `_sid`, `slow.png`) — *DONE*.
7. **[Low] Lean `futures-util`** — *DONE*. **Kept `tower-http`.**
8. **[Open, not started] Refactor the rendering hot path** — `build_instances` (13 args),
   `paint_cell` (~78 lines), `renderer.rs` (1,369 lines), `state.rs` (924 lines). Deliberately
   deferred: these are structural refactors with real regression risk, not cleanup.

### Phase 2 follow-ups

9. **[High] Confirm the phantom-glyph fix on the affected machine** — *OPEN, awaiting user*.
   Run `?r=2d` vs `?r=gl` and select/copy a phantom. Clipboard contains it → real terminal
   state from OpenCode, not a rendering bug. WebGL-only and absent from the clipboard → fixed
   by bugs A/B above.
10. **[Medium] Make `render-check.sh` degrade honestly when WebGL2 is unavailable** — *OPEN*.
    The script currently reports `renderer=gl: FAIL` for what is an environment gap, which reads
    as a regression. It should detect the missing context and either skip the GL mode explicitly
    or fall back to a browser that has software WebGL. Until then, treat a GL failure here as
    inconclusive and confirm with Firefox before believing it.
11. **[Medium] Surface WebGL errors instead of swallowing them** — *OPEN, not started*.
    `krust_runtime.js` calls `bufferData` / `texSubImage2D` / `drawArraysInstanced` and never
    calls `getError`, so every GL error is silent. Bug A was reachable precisely because a
    failed upload was indistinguishable from a successful one. A single `glGetError` check after
    the atlas upload loop in `ensure_glyphs` would close the remaining half of that hole: a GL
    error there can still leave a reserved slot pointing at a stale texel, which `release_slots`
    does not currently catch because it only triggers on a short rasterize result.
12. **[Low] Guard the raw-WASM export list** — *PARTIAL*. The one-liner in the Phase 2 process
    note documents how to dump the export section, and `AGENTS.md` now records that
    `client/src/exports.rs` is the only file where an export is defined. Not yet wired into
    `render-check.sh` or `smoke-test.sh` as an automated assertion, which is where it belongs.

