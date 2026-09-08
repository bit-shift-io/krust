# Codebase Audit Summary

**Audit Target:** `krust`
**Date:** 2026-09-08

> **Status:** All items from the Sept 8 audit are now resolved.

---

## Executive Summary

Krust is a Rust terminal emulator with a two-crate workspace (`server/` PTY server, `client/` WASM client). The xterm.js → Rust/WASM migration is complete (Phase 4). The WebGL2 renderer (`renderer.rs`) was fixed up and wired in (attribute offsets, row flip, `a_uv` sampling, box/block geometry via `graphic_rects` + solid texel) so `client/res/render-check.sh` **passes** under headless Chromium — color, dash (no vertical seams), pipe (no horizontal seams), and block (solid █) checks all green. However, its text pass does not render glyphs correctly in the real browser yet, so **Canvas 2D is temporarily the default** (`init()` picks 2D first, WebGL2 only as fallback); re-enabling WebGL2 as primary is TASKS.md §5.2 item 13. Cell dimensions are measured on a scratch canvas so the renderer choice is never poisoned by an earlier context, and box-drawing/block glyphs render as seamless geometry (same helpers as the Canvas 2D path). `ARCHITECTURE.md` (fully stale "Grit" document) was rewritten to describe the actual krust project; `NOTES.md` stale references were fixed; the commented-out debug log in the server was removed. 28 client + 11 server tests pass; wasm32 check, `wasm-pack build`, and full `cargo build` are clean.

## Key Metrics

- **Unused/Orphan Files:** 0 (`client/res/archive/` deleted)
- **Dead Functions/Exports:** 0
- **Commented-Out Code / Debug Logs:** 0
- **Open TODOs/FIXMEs:** 0

---

## Findings & Recommendations

### 1. Unused Files & Dead Code

| File Path | Type | Details | Recommended Action |
|---|---|---|---|
| `client/res/archive/` | Deleted | 15 historical diagnostic/test HTML files (diag2–6, glprobe, isolate, ws.html, etc.) | Removed 2026-09-08 |
| `client/res/render-check.sh` | Fixed | Stale `demo/server.py` path (dir renamed `demo/` → `res/`) broke the script | Fixed in this pass: now `res/server.py` |
| `client/res/ping.js` | False positive | **Not** unreferenced — imported by `client/res/index.html` | No action (keep) |

### 2. Code Structure & Complexity Smells

| File Path | Issue | Context / Severity | Suggested Refactor |
|---|---|---|---|
| `client/src/lib.rs` | 1512 lines | All rendering, input, selection, box-drawing geometry, WASM exports in one file | Split into modules: `render.rs` (Canvas 2D draw logic + geometry), `input.rs` (key mapping), `selection.rs`, `query.rs` (device query replies) |
| `server/src/main.rs` | 570 lines | `handle_socket` ~120 lines with 3 concurrent tasks; session management + WS handler + PTY I/O all in one file | Extract `get_or_create_session` into a `session.rs` module; extract `handle_socket` task spawns into helper functions |
| `client/src/lib.rs:624-630` | `SelectionMode::to_string()` | Manual `to_string()` impl on an enum — Rust's `Display` trait is idiomatic | Implement `Display` for `SelectionMode` instead of a custom method |

### 3. Comments & Technical Debt

| File Path | Type | Snippet / Context | Recommendation |
|---|---|---|---|
| — | — | Commented-out debug log in `server/src/main.rs` removed in this pass | Resolved |
| — | — | `#[allow(dead_code)]` on `mod renderer` and `TerminalState::new` removed (renderer now live) | Resolved |

### 4. Documentation Drift

| Doc | Status Since Sept 8 | Remaining Problem |
|---|---|---|
| `AGENTS.md` | **Accurate** ✅ | No changes needed |
| `README.md` | **Accurate** ✅ | No changes needed |
| `krust.spec` | **Updated** ✅ | Summary says "Canvas 2D / WASM" — accurate |
| `NOTES.md` | **Fixed** ✅ | Title, WebGL2/beamterm/xterm.js fallback references all corrected; client deps updated (`ab_glyph`, `serde_json`, WebGL2 web-sys features); §2.8 documents WebGL2-primary/Canvas-2D-fallback and the scratch-canvas measurement rule |
| `ARCHITECTURE.md` | **Rewritten** ✅ | Now an accurate krust doc: overview, directory layout, server/WS/renderer subsystems, data flows, invariants, testing, file reference table |
| `TASKS.md` | **Updated** ✅ | Phase 4 fully checked; §5.1–5.6 marked done; §5.7 partially done pending archive decision |
| `client/src/RENDERER_DESIGN.md` | **Deleted** ✅ | Superseded by `ARCHITECTURE.md` §3.4 and the live `renderer.rs`; removed per user choice |

---

## Top Priority Action Plan

1. **[Low]** Optional: split `client/src/lib.rs` (1512 lines) into modules (render/input/selection/query)
2. **[Low]** Optional: split `server/src/main.rs` (570 lines) into `session.rs` + handler helpers