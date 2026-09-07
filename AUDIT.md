# Codebase Audit Summary

**Audit Target:** `krust`
**Date:** 2026-09-07

---

## Executive Summary

Krust is a Rust terminal emulator with a two-crate workspace (`backend/` PTY server, `client-wasm/` WASM client). The codebase has largely completed the migration from xterm.js to a Rust/WASM pipeline, but **documentation is severely stale** — ARCHITECTURE.md describes a different project entirely (a Git client called "Grit"), README.md still mentions xterm.js, and TASKS.md's Phase 4 migration tasks are unchecked. The `res/` directory retains xterm.js artifacts and a feature flag (`new-terminal`) exists but is unused. Missing `AGENTS.md` despite being referenced by CLAUDE.md and GEMINI.md.

## Key Metrics

- **Unused/Orphan Files:** 0 (res/ files serve as fallback; demo files are test assets)
- **Dead Functions/Exports:** 1 (`new-terminal` feature flag in client-wasm/Cargo.toml, never checked in code)
- **Commented-Out Code / Debug Logs:** 1 (`// eprintln!("[out:{}] {} bytes", sid_out, frame.len());` in backend/main.rs:333)
- **Open TODOs/FIXMEs:** 0

---

## Findings & Recommendations

### 1. Unused Files & Dead Code

| File Path | Type | Details | Recommended Action |
|---|---|---|---|
| `client-wasm/Cargo.toml:7` | Dead Feature Flag | `new-terminal = []` feature defined but never referenced in code | Remove `[features]` section |
| `res/` (xterm.js, .css, addons) | Stale artifacts | xterm.js files kept for fallback; backend.html still serves `/res/` paths | Keep for now (fallback path), mark for removal after migration proven |
| `client-wasm/demo/` (diag2-6, glprobe, etc.) | Demo/test assets | 15+ HTML/JS files, only `index.html`, `backend.html`, `server.py`, `smoke-test.sh` actively used | Archive unused demos or move to `demo/archive/` |
| `backend/target/`, `client-wasm/target/` | Build artifacts | Should be gitignored | Verify `.gitignore` covers them |

### 2. Code Structure & Complexity Smells

| File Path | Issue | Context / Severity | Suggested Refactor |
|---|---|---|---|
| `backend/src/main.rs` | 567 lines, high complexity | `handle_socket` is ~120 lines with 3 concurrent tasks | Extract task spawns into separate functions |
| `client-wasm/src/lib.rs` | 1139 lines | `TerminalState` + all WASM exports in single file | Split into `renderer.rs`, `input.rs`, `ws.rs` modules |
| `client-wasm/src/lib.rs` | `detect_webgl2()` stub | Always returns `Ok(true)` — not real WebGL2 detection | Implement with `web_sys::WebGl2RenderingContext` or remove |
| `client-wasm/src/lib.rs` | `CanvasRenderingContext2d` used, not WebGL | Docs claim WebGL2; code uses 2D canvas | Update docs or switch to WebGL2 renderer |

### 3. Comments & Technical Debt

| File Path | Type | Snippet / Context | Recommendation |
|---|---|---|---|
| `backend/src/main.rs:333` | Debug log (commented) | `// eprintln!("[out:{}] {} bytes", ...)` | Remove dead comment or uncomment if needed |
| `ARCHITECTURE.md` | Stale — describes different project | References "Grit", Iced GUI, TabRegistry, `src/git/`, `src/ui/` — none exist | Rewrite for current architecture |
| `README.md` | Stale | Says "xterm.js" in description | Update to WASM renderer |
| `NOTES.md` | Partially stale | Claims `beamterm-renderer` is used; code uses Canvas 2D | Update renderer references |
| `rust_wasm_guide.md` | Stale examples | Example code uses `beamterm-renderer` which isn't in Cargo.toml | Update to match actual vt100+Canvas 2D implementation |
| `CLAUDE.md` / `GEMINI.md` | Missing AGENTS.md | Reference `AGENTS.md` which does not exist | Create `AGENTS.md` |

### 4. Documentation Drift

| Doc | Problem |
|---|---|
| `ARCHITECTURE.md` | Describes a Git client ("Grit") — completely unrelated to current terminal emulator |
| `README.md` | Mentions xterm.js, single-binary desktop app |
| `TASKS.md` | Phase 4 migration tasks unchecked (4.1-4.5) though code is mostly migrated |
| `krust.spec` | Summary says "xterm.js" |
| `NOTES.md` | References beamterm-renderer (not a dependency) |

---

## Top Priority Action Plan

1. **[High]** Rewrite `ARCHITECTURE.md` to reflect actual current architecture (PTY server + WASM client)
2. **[High]** Update `README.md` to describe WASM terminal, remove xterm.js references
3. **[High]** Create `AGENTS.md` (referenced by CLAUDE.md/GEMINI.md)
4. **[Medium]** Update `TASKS.md` — mark Phase 4 complete, update milestones
5. **[Medium]** Update `NOTES.md` / `rust_wasm_guide.md` renderer references (Canvas 2D, not beamterm)
6. **[Medium]** Update `krust.spec` summary
7. **[Low]** Remove unused `new-terminal` feature flag from `client-wasm/Cargo.toml`
8. **[Low]** Clean up demo/ archive (diag2-6, glprobe, etc.)
