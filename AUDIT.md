# Codebase Audit Summary

**Audit Target:** `krust` — Rust/WASM terminal emulator client + Axum/`portable-pty` server  
**Audit Date:** 2026-10-09  
**Status:** Findings Active (Remediation Pending)

---

## Executive Summary

Krust is a single-binary web terminal emulator featuring an Axum WebSocket server backed by `portable-pty` and a raw WebAssembly client rendering via WebGL2 (with Canvas 2D fallback).

Following the October 7 performance remediation, the hot-path terminal diffing and frame throughput are significantly improved. However, this audit identified **two high-severity security vulnerabilities** (Cross-Site WebSocket Hijacking allowing arbitrary remote shell command execution, and unauthenticated/unbounded session spawning DoS), a **continuous WASM heap memory leak** occurring on every inbound WebSocket frame, a **functional bug dropping all non-ASCII unicode keystrokes**, and a **session lockup defect** that leaves sessions permanently dead after a shell terminates.

## Key Metrics

- **Workspace Test Suite:** 197 passing tests (164 client, 33 server).
- **Headless Browser Harness:** Headless Chromium (`render-check.sh` GL & 2D) and headless Firefox (`smoke-test.sh`) pass.
- **Compiler / Linter Warnings:** 5 Clippy warnings in `terminal-client` (unused variables, argument counts, manual multiple checks).
- **Security Exposures:** 3 identified (Remote Code Execution via CSWSH / LAN bind, Session Exhaustion DoS, Unchecked OSC 8 URI scheme).
- **Memory Leaks:** 2 identified (frame-level CString leak in `process_bytes`, unreleased FFI object handles in `krust_runtime.js`).
- **Input / Protocol Deficiencies:** 3 identified (non-ASCII character input drop, keyboard paste bypassing bracketed paste, permanent session lockup on shell exit).
- **Untracked / Stray Files:** 3 accidental debug logs/dumps committed to git (`console-export-*.log`, `dump.txt`).

---

## Findings & Recommendations

### 1. Security & Networking (HIGH / CRITICAL)

| Finding | Location | Severity | Description & Remediation |
|:---|:---|:---:|:---|
| **Cross-Site WebSocket Hijacking (CSWSH) & LAN Shell Access** | `server/src/main.rs:35-40`<br>`server/src/handlers.rs:308-320` | **Critical** | The server defaults to binding `0.0.0.0:3000`, exposing shell access to the entire local network without authentication. Furthermore, `ws_handler` does not validate the HTTP `Origin` header. WebSockets are not restricted by CORS preflights; any website visited in a user's browser can initiate `new WebSocket("ws://localhost:3000/ws")` and send arbitrary shell commands to run under the local user's account.<br>**Fix:** Support configuring `host` via `.config/bitshift/krust/config.json` (defaulting to `127.0.0.1` to restrict access by default, with an option to set `0.0.0.0` for trusted LAN environments). In `ws_handler`, validate the `Origin` header against an allowlist (same host and explicitly allowed origins like `localhost:5000`). |
| **Unbounded Session Spawning (Denial of Service)** | `server/src/handlers.rs:313-328`<br>`server/src/session.rs:204-245` | **High** | Connecting with `?s=<id>` triggers `get_or_create_session`, which spawns a system shell subprocess (`portable-pty`), background reader thread, 512 KB history buffer, and mirror parser. Sessions are never evicted from `state.sessions`. Rapid connections with randomized session IDs can exhaust system PTYs, file descriptors, and process limits.<br>**Fix:** Enforce a maximum concurrent session cap (e.g. 16) and implement idle cleanup for abandoned sessions. |
| **Unsanitized URI Scheme in OSC 8 Hyperlinks** | `client/res/server.html:940-945` | **Medium** | When opening an OSC 8 link on Ctrl/Cmd-click, the page invokes `window.open(uri, '_blank', 'noopener,noreferrer')` without scheme validation. Untrusted output emitting `\x1b]8;;javascript:...` or `data:...` links can trigger client-side script execution (XSS).<br>**Fix:** Validate that the URI protocol is strictly `http:`, `https:`, or `mailto:` before calling `window.open`. |

---

### 2. Memory & Resource Management (HIGH / MEDIUM)

| Finding | Location | Severity | Description & Remediation |
|:---|:---|:---:|:---|
| **Per-Frame CString Leak in `process_bytes`** | `client/src/exports.rs:252-278`<br>`client/res/server.html:662-665` | **High** | `process_bytes` allocates a JSON string in WASM linear memory (`write_string_to_wasm(json)`) and returns a boxed `[u32; 2]` pair. In `handleWsData` (and `SELFTEST`, `render-test.html`, `index.html`), callers only execute `free_result(summaryPtr)`, which frees the 8-byte boxed pair but **never calls `free_string(dataPtr)`**. Over thousands of frames, ~70 bytes per frame are leaked continuously into WASM memory. The JSON summary is never even consumed by JS.<br>**Fix:** Change `process_bytes` to return `void` (or an integer code), avoiding both the heap allocation and the leak. |
| **JS Object Handle Leaks in `krust_runtime.js`** | `client/res/krust_runtime.js:64-66`<br>`client/src/measure.rs:43-50`<br>`client/src/exports.rs:473` | **Medium** | `krust_window()` and `krust_window_document()` append `window` and `document` to the JS `heap` array on every resize, measurement, and `grid_metrics()` call without corresponding `krust_release()` calls. Similarly, `rebuild_webgl` creates new GL resources while old texture/shader handles remain in `heap`.<br>**Fix:** Assign static reserved handle IDs in `krust_runtime.js` for permanent objects like `window` (1) and `document` (2). |
| **Potential Allocator Mismatch in `alloc` / `dealloc`** | `client/src/exports.rs:948-964` | **Low** | `alloc(size)` creates a `Vec::with_capacity(size)` and forgets it. Allocators may round up capacity (`capacity >= size`). `dealloc(ptr, size)` reconstructs `Vec::from_raw_parts(ptr, size, size)`. If the allocated capacity exceeded `size`, this violates the safety precondition that capacity must equal the original allocation capacity.<br>**Fix:** Use `std::alloc::alloc` and `std::alloc::dealloc` with explicit `Layout`. |

---

### 3. Terminal Protocol & Usability (MEDIUM)

| Finding | Location | Severity | Description & Remediation |
|:---|:---|:---:|:---|
| **Non-ASCII Unicode Keystrokes Dropped** | `client/src/input.rs:29-31, 133-140`<br>`client/res/server.html:843-847` | **Medium** | `input.rs` uses `is_printable_ascii(c)` to gate single-character keyboard input. Any accented character (`é`, `ü`, `ñ`), non-Latin character (Cyrillic, Greek, Arabic, CJK), or emoji returns `false`. `map_key` returns an empty byte slice and `server.html` drops the event. Users cannot type non-ASCII characters directly into the terminal.<br>**Fix:** Replace `is_printable_ascii(c)` on the single-character pass-through with `!c.is_control()`, allowing all non-control unicode scalar values to be encoded as UTF-8. |
| **Keyboard Paste Bypasses Bracketed Paste** | `client/res/server.html:805-818` | **Medium** | When pasting via Ctrl+Shift+V, Cmd+V, or Shift+Insert, the event handler reads clipboard text and calls `ws.send(data)` directly with raw text instead of calling `sendPaste(text)`. Bracketed paste (`DECSET 2004`) wrapping (`ESC[200~` ... `ESC[201~`) is bypassed for keyboard paste, breaking multiline paste and auto-indent in Vim, Nano, and shells.<br>**Fix:** Call `sendPaste(text)` in the clipboard read promise handler. |
| **Permanent Session Lockup After Shell Exit** | `server/src/session.rs:278-321`<br>`server/src/handlers.rs:523-536` | **Medium** | When a shell process exits (e.g. typing `exit`), the PTY reader thread encounters EOF and exits. The WebSocket connection closes. However, because sessions persist indefinitely in `state.sessions`, refreshing the page or reconnecting returns the dead session. The reconnected terminal is permanently unresponsive.<br>**Fix:** Track session vitality (e.g. `is_alive: Arc<AtomicBool>`); when `get_or_create_session` encounters a dead session, purge it and spawn a new shell. |
| **Stale Hyperlink (`OSC 8`) Coordinates on Scroll / Clear** | `client/src/state.rs:141-155` | **Low** | `LinkSpan` stores absolute `(row, col)` screen coordinates. When the terminal scrolls or screen clears, link spans are not shifted or cleared. Newly drawn text at those cell positions can falsely match old hyperlink coordinates.<br>**Fix:** Clear or adjust link spans when the grid scrolls or screen is erased. |

---

### 4. Code Hygiene & Maintenance (LOW)

| Finding | Location | Severity | Description & Remediation |
|:---|:---|:---:|:---|
| **Compiler & Clippy Warnings** | `client/src/state.rs:904-905`<br>`client/src/query.rs:37`<br>`client/src/ffi.rs:348`<br>`client/src/mouse.rs:67` | **Low** | Unused variables `css_w` and `css_h` in `render_dirty_cells` (left over from canvas-clear removal); `manual_is_multiple_of` in `query.rs`; `too_many_arguments` in FFI/mouse reporting.<br>**Fix:** Prefix unused variables with underscores (or remove), and apply suggested Clippy idioms. |
| **Accidental Debug Logs & Dumps Committed to Git** | Repository Root | **Low** | `console-export-2026-10-8_17-40-56.log`, `console-export-2026-10-8_21-4-17.log`, and `dump.txt` are tracked in git and contain local filesystem paths and escape dumps. In addition, `client/pkg/` and `client/target/` are missing from `.gitignore`.<br>**Fix:** Remove the files from git tracking and add `*.log`, `dump.txt`, `client/pkg`, and `client/target` to `.gitignore`. |

---

## Remediation Plan

1. **[Immediate / Security]** Implement `.config/bitshift/krust/config.json` for host/port binding (defaulting `host` to `127.0.0.1`, configurable to `0.0.0.0`), and restrict WebSocket origins in `handlers.rs`.
2. **[Immediate / Performance]** Remove JSON string allocation from `process_bytes` in `exports.rs` to stop WASM frame memory leakage.
3. **[Immediate / Usability]** Permit non-ASCII unicode characters in `input.rs` (`!c.is_control()`).
4. **[High / Usability]** Route keyboard paste shortcuts in `server.html` through `sendPaste()` to respect bracketed paste mode.
5. **[High / Stability]** Detect terminated shell sessions in `session.rs` and replace dead sessions on reconnect.
6. **[Medium / Security]** Sanitize OSC 8 hyperlink schemes in `server.html` before executing `window.open`.
7. **[Low / Cleanliness]** Clean up Clippy warnings, remove git-tracked logs/dump, and update `.gitignore`.

---

## Previous Audit Status (2026-10-07)

All findings from the previous audit (double screen cloning on live output, GL full-grid reupload, missing bracketed paste/focus/OSC standards) were resolved in phases 0–4 and verified with the regression test suite. The new findings above reflect issues discovered in subsequent review and commits.
