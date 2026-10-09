# Plan: Remedy Codebase Audit Findings (Security, Memory, Protocol, Usability)

## Status: NOT STARTED

Supersedes the 2026-10-07 performance plan. Derived from `AUDIT.md` (2026-10-09). Work top-to-bottom; each `- [x]` is one TDD-sized task. Do not batch.

## Context

Five focus areas identified during the audit:

1. **Security & Access Control:** Default listener binding allows LAN exposure; `ws_handler` lacks WebSocket `Origin` validation (CSWSH risk); unauthenticated session spawning is uncapped; OSC 8 URLs lack scheme validation. Per user requirements, default listener binding should be configurable via `HOST` (defaulting to `127.0.0.1` for restricted access, overrideable to `0.0.0.0` for trusted LAN environments).
2. **Memory Leaks:** `process_bytes` allocates an unused JSON string on every WebSocket frame that JS callers never free; JS FFI `heap` leaks `window`/`document` handles.
3. **Protocol & Usability:** Single-char keyboard input drops all non-ASCII unicode characters; keyboard paste shortcuts bypass bracketed paste wrapping (`sendPaste`); sessions freeze permanently after a shell process terminates.
4. **ABI Safety & Correctness:** `init()` error branch returns an incompatible pointer type; `alloc`/`dealloc` lacks layout symmetry.
5. **Code Quality:** Clippy warnings in `terminal-client`; accidental log dumps and debug files committed to git.

### Verification commands

```bash
cargo test --workspace                         # all tests
cargo test -p terminal-client                  # client (host) only
cargo test -p krust                            # server only
cargo clippy --workspace --all-targets         # clippy verification
cargo build                                    # rebuilds wasm via build.rs
client/res/render-check.sh                     # headless Chromium GL + 2D
client/res/smoke-test.sh                       # headless smoke
```

---

## Phase 1 — Critical: Memory Leaks & ABI Safety

- [x] **T1.1. Eliminate frame-level CString allocation in `process_bytes`.**
  **Files:** `client/src/exports.rs:252-278`.
  **Steps:** Change `pub extern "C" fn process_bytes(bytes_ptr: *const u8, bytes_len: usize)` to return `i32` (1 for success, 0 on failure) instead of a boxed `(ptr, len)` JSON string. Remove `json` formatting and `write_string_to_wasm(json)` on the hot path.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T1.2. Update JavaScript call sites for `process_bytes`.**
  **Files:** `client/res/server.html:589-591, 662-664`, `client/res/render-test.html:80-83, 334, 371`, `client/res/index.html:84-86`.
  **Steps:** Remove `free_result(summaryPtr)` calls after `process_bytes`; simply call `process_bytes(...)`. Ensure no dangling pointers or leaked memory remain per frame.
  **Verify:** `client/res/render-check.sh` and `client/res/smoke-test.sh`.

- [x] **T1.3. Fix type mismatch in `init()` error exit.**
  **File:** `client/src/exports.rs:206-208`.
  **Steps:** Replace `return write_string_to_wasm("".to_string()).0 as *mut u8;` with `return return_null_pair();` so that any failure to initialize returns a valid null boxed pair (`[0, 0]`) matching the ABI contract expected by `exportBoxedPair`.
  **Verify:** `cargo test -p terminal-client`.

- [x] **T1.4. Standardize `alloc()` and `dealloc()` with explicit `Layout`.**
  **File:** `client/src/exports.rs:948-964`.
  **Steps:** Use `std::alloc::alloc` and `std::alloc::dealloc` with `std::alloc::Layout::from_size_align(size.max(1), 1).unwrap()` rather than relying on `Vec::from_raw_parts` with assumed capacity equivalence.
  **Verify:** `test_alloc_dealloc_roundtrip` in `exports.rs`.

---

## Phase 2 — Usability & Input: Unicode & Bracketed Paste

- [x] **T2.1. Permit non-ASCII unicode scalar characters in `map_key`.**
  **File:** `client/src/input.rs:29-31, 120-141`.
  **Steps:** Replace `is_printable_ascii(c)` with `!c.is_control()` in the single-character pass-through branch (and Alt+char branch), so accented characters (e.g. `é`, `ñ`), Cyrillic, CJK, and emojis are encoded to UTF-8 and forwarded to the PTY instead of being dropped.
  **Verify:** Add unit tests in `client/src/input.rs` covering accented characters (`é`, `ü`), CJK ideographs, and emoji.

- [x] **T2.2. Route keyboard paste shortcuts through `sendPaste()`.**
  **File:** `client/res/server.html:805-818`.
  **Steps:** In the `keydown` listener for `isShiftInsert || isCtrlShiftV || isMacCmdV`, replace `const data = new TextEncoder().encode(text); ws.send(data);` with `sendPaste(text);`.
  **Verify:** When bracketed paste (`?2004`) is active, pasting with Ctrl+Shift+V or Cmd+V applies the `ESC[200~` and `ESC[201~` markers.

- [x] **T2.3. Sanitize OSC 8 hyperlink URI schemes before opening.**
  **File:** `client/res/server.html:940-945`.
  **Steps:** In the `mousedown` handler for modifier-clicks, parse `uri` with `new URL(uri)` and permit only `http:`, `https:`, and `mailto:` schemes before calling `window.open(parsed.href, '_blank', 'noopener,noreferrer')`.
  **Verify:** Clicking an `http://...` link opens; clicking a `javascript:...` link is safely ignored.

---

## Phase 3 — Server Reliability & Session Lifecycle

- [x] **T3.1. Track session vitality upon PTY reader EOF.**
  **File:** `server/src/session.rs:179-198, 278-321`.
  **Steps:** Add `pub(crate) is_alive: Arc<std::sync::atomic::AtomicBool>` to `Session` (initialized to `true`). In the background reader task (`spawn_blocking`), when `reader.read` returns `0` or an error, store `false` in `is_alive`.
  **Verify:** Server unit test asserting `is_alive` flips to `false` when the reader ends.

- [x] **T3.2. Replace dead sessions in `get_or_create_session`.**
  **File:** `server/src/session.rs:204-222`.
  **Steps:** In `get_or_create_session`, if a session exists in `state.sessions` but `!session.is_alive.load(Ordering::SeqCst)`, remove it from `state.sessions` and spawn a fresh replacement session.
  **Verify:** Test simulating shell exit and subsequent reconnect; assert a new active session is created.

- [x] **T3.3. Enforce a maximum concurrent session cap.**
  **File:** `server/src/session.rs:204-232`.
  **Steps:** Define `const MAX_CONCURRENT_SESSIONS: usize = 32`. If `state.sessions.len() >= MAX_CONCURRENT_SESSIONS`, purge any dead sessions first. If still at capacity, return an error or evict the oldest session with 0 connections.
  **Verify:** Unit test proving session map does not grow unboundedly with random session IDs.

---

## Phase 4 — Security & Configuration Controls

- [x] **T4.1. Implement application config file (`.config/bitshift/krust/config.json`) for listener host & port.**
  **Files:** `server/src/config.rs` (new), `server/src/main.rs:35-40`.
  **Steps:** Create a config module matching the Grit pattern (`$XDG_CONFIG_HOME/bitshift/krust/config.json`, fallback `$HOME/.config/bitshift/krust/config.json`). Define `KrustConfig` struct with `host` (default `"127.0.0.1"`, with option to configure `"0.0.0.0"` to allow LAN access), `port` (default `3000`), and `allowed_origins` (default `["http://localhost:3000", "http://127.0.0.1:3000", "http://localhost:5000"]`). Allow `HOST` and `PORT` env vars to override config file settings when set. Bind the server to the configured host and port.
  **Verify:** Unit tests in `server/src/config.rs` testing defaults, loading from custom path, and fallback when missing. Server logs configured listening URL.

- [x] **T4.2. Validate WebSocket `Origin` header in `ws_handler`.**
  **File:** `server/src/handlers.rs:308-320`.
  **Steps:** Extract `axum::http::HeaderMap` in `ws_handler`. If an `Origin` header is present, verify that the host matches `localhost`, `127.0.0.1`, the server's own configured host, or `http://localhost:5000` (Grit UI). Reject unauthorized origins with `StatusCode::FORBIDDEN`.
  **Verify:** Integration test sending `Origin: http://evil.com` to `/ws` receives HTTP 403; requests with `Origin: http://localhost:3000` or `http://localhost:5000` succeed.

---

## Phase 5 — FFI & State Cleanup

- [x] **T5.1. Use static global handles in `krust_runtime.js`.**
  **File:** `client/res/krust_runtime.js:18-20, 64-66`.
  **Steps:** Reserve handle `1` for `window` and handle `2` for `document` in `heap`. Update `krust_window` and `krust_window_document` to return these static handles directly instead of calling `addObject` on every frame/measurement.
  **Verify:** Repeated calls to `krust_window` / `krust_window_document` leave `freeSlots` and `heap.length` constant.

- [x] **T5.2. Reset OSC 8 link spans on screen clear / reset.**
  **File:** `client/src/state.rs:1350-1368`.
  **Steps:** Clear `TerminalCallbacks::links` when the parser receives a full screen clear (`ED 2` / `\x1b[2J`) or terminal reset, preventing old hyperlinks from attaching to newly drawn text.
  **Verify:** Unit test asserting links are cleared after screen reset.

---

## Phase 6 — Code Hygiene & Lints

- [x] **T6.1. Resolve Clippy warnings.**
  **Files:** `client/src/state.rs:904-905`, `client/src/query.rs:37`, `client/src/ffi.rs:348`, `client/src/mouse.rs:67`.
  **Steps:** Remove or underscore unused `css_w` and `css_h` variables in `render_dirty_cells`; replace `% 2 != 0` with `!is_multiple_of(2)`; add `#[allow(clippy::too_many_arguments)]` to `gl_tex_image_2d_alpha` and `mouse_report`.
  **Verify:** `cargo clippy --workspace --all-targets` runs with 0 warnings.

- [x] **T6.2. Remove stray debug logs and update `.gitignore`.**
  **Files:** Repository root, `.gitignore`.
  **Steps:** Remove `console-export-2026-10-8_17-40-56.log`, `console-export-2026-10-8_21-4-17.log`, and `dump.txt` from git tracking. Add `*.log`, `dump.txt`, `client/pkg/`, and `client/target/` to `.gitignore`.
  **Verify:** `git status` shows no untracked or unwanted files.

- [x] **T6.3. Comprehensive regression check.**
  **Steps:** Run `cargo test --workspace`, `client/res/render-check.sh`, and `client/res/smoke-test.sh`. Ensure both WebGL2 and Canvas 2D render paths pass and no regressions occur.
  **Verify:** All checks exit 0.
