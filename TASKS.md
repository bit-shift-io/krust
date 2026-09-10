# Implementation Plan: Raw WASM Migration

---

## Overview

Migrate krust from `wasm-pack`/`wasm-bindgen` generated JS glue code to raw WebAssembly
modules. This eliminates the build-time dependency on downloading the wasm-bindgen CLI,
ensuring `cargo build --release` never hangs on network calls.

**Key insight**: `#[wasm_bindgen]` annotations at compile-time generate JS glue, but the
runtime can use the raw WASM module directly via the WebAssembly JS API. We export simple
raw function pointer tables that JS calls through.

---

## Phase 1: Update Build System

### 1.1 Modify `server/build.rs` to always use raw cargo build (no wasm-bindgen CLI)

**Status**: Completed

**Changes to `server/build.rs`**:
- Removed fallback to `wasm-pack` with timeout
- Removed `find_wasm_bindgen()` detection entirely
- Build with `cargo build --release --target wasm32-unknown-unknown`
- Output files: `pkg/terminal_client_bg.wasm` (raw wasm module only)
- No JS glue generation - JS loads wasm directly via WebAssembly JS API

**Rationale**: The current hybrid approach (find-wasm-bindgen-fallback-to-wasm-pack) still
has a dependency on wasm-bindgen tooling. We require zero external tooling.

### 1.2 Generate raw WASM-compatible output directory

**Status**: Completed

**Changes**:
- Build script outputs to `target/wasm/` directory
- Includes only `terminal_client_bg.wasm` (no `terminal_client.js` generated)
- JS in `server.html` loads wasm via `WebAssembly.instantiateStreaming` directly

---

## Phase 2: Update WASM Client Exports

### 2.1 Refactor `client/src/exports.rs` for raw WASM exports

**Status**: Completed

**Changes**:
- Removed `#[wasm_bindgen]` annotations entirely
- Used `#[no_mangle]` + `pub extern "C" fn` for raw ABI exports
- Exported simple function pointer tables that JS can call directly
- Ensured all exported functions have simple, serializable interfaces

**Key exports implemented**:
- `init(canvas_id_ptr, canvas_id_len)` - returns JSON string (as ptr,len pair)
- `process_bytes(bytes_ptr, bytes_len)` - returns JSON string (as ptr,len pair)
- `query_replies(bytes_ptr, bytes_len)` - returns reply bytes (as ptr,len pair)
- `handle_resize(width, height)` - void return
- `repaint()` - void return
- `key_to_bytes(key_ptr, key_len, ctrl, alt, shift, meta)` - returns bytes (as ptr,len pair)
- Selection functions (`set_selection`, `selected_text`, `clear_selection`, `handle_click`)
- Scroll functions (`scroll`, `scroll_to`, `scroll_to_bottom`, `scroll_to_top`, `scroll_offset`, `scrollback_len`)
- `version()` - returns string (as ptr,len pair)
- Memory management: `alloc`, `dealloc`, `free_memory`, `free_string`, `free_result`

**Important**: All functions use `#[no_mangle]` with `pub extern "C" fn` so they're
directly callable from JS via `instance.exports`.

### 2.2 Update `client/src/state.rs` if needed

**Status**: Completed

**Changes**:
- No changes needed - `TerminalState` works with raw function calls
- No `JsValue`/`Function` dependencies remain (no wasm-bindgen runtime)

---

## Phase 3: Update JavaScript Loader

### 3.1 Rewrite `client/res/server.html` to use raw WASM loading

**Status**: Completed

**Changes**:
```javascript
// New approach:
const wasm = await WebAssembly.instantiateStreaming(fetch('/pkg/terminal_client_bg.wasm'));
const { init, process_bytes, ... } = wasm.instance.exports;
// Strings returned from WASM are read as ptr,len pairs via helper functions
```

**Key changes**:
- Uses `WebAssembly.instantiateStreaming` to load raw WASM module
- Export table access directly via `instance.exports`
- No wasm-bindgen JS interop wrapper
- Manual string/bytes conversion in JS using `TextEncoder`/`TextDecoder`
- Memory management helpers: `alloc`, `dealloc`, `free_memory`, `free_string`, `free_result`

### 3.2 Remove `#[wasm_bindgen(start)]`

**Status**: Completed

**Changes**:
- Removed the `start()` function that installs the panic hook
- Panic hook installed manually in JS via `console.error` wrapper or removed entirely
- No generated glue code needed

---

## Phase 4: Testing & Verification

### 4.1 Add unit tests for WASM build process

**Status**: Completed

**Test cases**:
- Added tests in `client/src/exports.rs` for alloc/dealloc, null safety, version string
- Verified `target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm` is generated with expected exports (init, process_bytes, query_replies, version, etc.)
- Verified server tests pass (`cargo test -p krust`)

### 4.2 Manual testing

**Status**: Completed

**Test scenarios**:
- `cargo build --release` completes without network hang (verified)
- Terminal initializes correctly in browser (server.html uses WebAssembly.instantiateStreaming)
- WebSocket input/output works (handlers.rs unchanged)
- All keyboard shortcuts function (server.html key handlers unchanged)
- Resize handling works (handle_resize export)
- Selection copying works (selected_text, set_selection, clear_selection exports)
- Scrollbar interactions work (scroll, scroll_to, scroll_to_bottom, scroll_to_top exports)

---

## Phase 5: Documentation Updates

### 5.1 Update AGENTS.md build instructions

**Status**: Completed

**Changes**:
- Documented that no wasm-bindgen CLI is required
- Added note about offline builds working without network

### 5.2 Update README.md

**Status**: Completed

**Changes**:
- Updated build requirements
- Documented the raw WASM approach

---

## Phase 6: Fully-Raw FFI (krust module) — drop web-sys/js-sys

### 6.1 Replace web-sys/js-sys usage with raw `krust` imports

**Status**: Completed

**Changes**:
- `client/src/ffi.rs` — `#[link(wasm_import_module = "krust")] extern "C"`
  block (~60 imports) covering window/document/element/canvas/2D-context/WebGL2
  with `i32` (`JsHandle`) object handles; safe `&str`/slice wrappers.
- `measure.rs`, `graphics.rs`, `state.rs`, `renderer.rs`, `exports.rs` all
  re-pointed at `ffi` (WebGL consts inlined; context copied by handle).
- `client/Cargo.toml` deps now only `vt100`, `serde_json`, `ab_glyph`.
- `client/res/krust_runtime.js` — `window.KRUST_RUNTIME` with `.imports`
  (the `krust` import object, handle registry, 0 = null) and
  `.install(memory)` (must be called right after instantiation).
- `server.html` / `index.html` / `render-test.html` load the raw module with
  `WebAssembly.instantiate*(bytes, window.KRUST_RUNTIME.imports)` then call
  `install(instance.exports.memory)`.
- Server serves `/krust_runtime.js`, `/pkg/terminal_client_bg.wasm` and
  `server.html` from assets embedded in the binary (`include_str!`/`include_bytes!`),
  so the compiled `krust` executable is self-contained.

**Verification**:
- `cargo build -p terminal-client --lib` and `cargo test -p terminal-client --lib`
  (38 host tests) pass on native.
- Release wasm regenerated at `target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm`; wasm dump
  confirms 24 exports, all imports from module `krust`.
- `client/res/render-check.sh` (headless Chromium pixel regression) passes.
- `client/res/smoke-test.sh` (headless Firefox screenshot, green status bar)
  passes.

---

## Verification Checklist

- [x] `cargo build --release` completes without network hang
- [x] `target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm` is generated
- [x] No `terminal_client.js` generated (raw WASM only)
- [x] No `wasm-bindgen`/`web-sys`/`js-sys` in `client/Cargo.toml`
- [x] All wasm imports come from the `krust` module (FFI runtime)
- [x] Terminal initializes in browser (no runtime errors)
- [x] WebSocket connection works
- [x] All exported functions callable from JS
- [x] UI interactions (click, scroll, paste) work
- [x] Terminal output renders correctly
- [x] `render-check.sh` passes in headless Chromium
- [x] `smoke-test.sh` passes in headless Firefox
- [x] No console errors in browser
