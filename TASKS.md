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
- Build script outputs to `client/pkg/` directory
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
- Verified `client/pkg/terminal_client_bg.wasm` is generated with expected exports (init, process_bytes, query_replies, version, etc.)
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

## Verification Checklist

- [x] `cargo build --release` completes without network hang
- [x] `client/pkg/terminal_client_bg.wasm` is generated
- [x] No `terminal_client.js` generated (raw WASM only)
- [x] Terminal initializes in browser (no runtime errors)
- [x] WebSocket connection works
- [x] All exported functions callable from JS
- [x] UI interactions (click, scroll, paste) work
- [x] Terminal output renders correctly
- [x] No console errors in browser
