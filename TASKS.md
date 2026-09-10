# Implementation Plan: Code Quality Refactoring

---

## Overview

Refactor the krust client to reduce code duplication, decompose oversized structures,
and clean up repeated patterns. The server side is already clean (3 small modules);
all targets are in `client/src/`.

**Key files:**
- `client/src/state.rs` (918 lines) — main target: duplicated render logic, oversized struct
- `client/src/renderer.rs` (774 lines) — repeated color math in `build_instances`
- `client/src/exports.rs` (479 lines) — repetitive box-pair return pattern

---

## Phase 1: Extract shared Canvas 2D cell-painting helper

### 1.1 Extract `paint_cell` from `render_full_grid` and `render_dirty_cells`

**Status:** Pending

**Problem:** `render_full_grid` (lines 509-627) and `render_dirty_cells` (lines 630-745)
each contain ~80 lines of nearly identical cell-painting logic:
  1. Clear cell to default background
  2. Paint non-default background rect
  3. Paint selection background rect (if selected)
  4. Paint text glyph (with bold font swap)
  5. Dispatch to `draw_graphic_cell` for box-drawing/block chars

The only difference: `render_full_grid` iterates all cells in a nested loop,
while `render_dirty_cells` iterates a pre-computed dirty list.

**Changes to `client/src/state.rs`:**
- Add a private method:
  ```rust
  fn paint_cell(
      &self,
      ctx: JsHandle,
      screen: &vt100::Screen,
      row: u16,
      col: u16,
      cw: f64,
      ch: f64,
      font: &mut String,
  )
  ```
- This method performs steps 1-4 above for a single cell.
- `render_full_grid` calls `paint_cell` in its grid loop.
- `render_dirty_cells` calls `paint_cell` in its dirty-list loop.
- Both methods retain their own setup (full-grid clears, cursor drawing).

**Estimated reduction:** ~80 lines removed, ~40 lines added (net -40 lines).

**Verification:**
- `cargo test -p terminal-client` — all existing tests pass
- `cargo build --release --target wasm32-unknown-unknown` — wasm builds clean
- Manual browser test: terminal renders identically (text, colors, selection, cursor)

---

### 1.2 Extract cursor drawing into a shared helper

**Status:** Pending

**Problem:** `draw_cursor` is called at the end of both `render_full_grid` (line 624)
and `render_dirty_cells` (line 742). The cursor logic is already extracted into its
own method, but the `visible_cursor` computation (lines 748-763) is called separately
in both paths and could be inlined into `draw_cursor`.

**Changes to `client/src/state.rs`:**
- Simplify: `draw_cursor` takes `(screen, rows, cols)` and internally calls
  `visible_cursor` — no need for callers to compute it separately.

**Estimated reduction:** ~10 lines.

---

## Phase 2: Decompose `TerminalState` struct

### 2.1 Extract `ScrollState` sub-struct

**Status:** Pending

**Problem:** `TerminalState` has 21 fields. Several are scroll-related and always
accessed together:
- `saved_normal_offset_for_alt: Option<usize>`
- `normal_scroll_offset: usize`
- `alternate_scroll_offset: usize`

**Changes to `client/src/state.rs`:**
- Create a `ScrollState` struct:
  ```rust
  struct ScrollState {
      saved_normal_offset_for_alt: Option<usize>,
      normal_offset: usize,
      alternate_offset: usize,
  }
  ```
- Move `active_scroll_offset`, `clamp_scroll`, `active_scrollback_len`,
  `set_scroll_offset`, `scroll_by`, `scroll_to_bottom`, `scroll_to_top`,
  `apply_scrollback` methods to `impl ScrollState`.
- `TerminalState` holds `scroll: ScrollState`.

**Estimated reduction:** ~0 lines (restructure, not removal), but reduces
`TerminalState` field count from 21 to 18 and groups related logic.

---

### 2.2 Extract `SelectionState` sub-struct

**Status:** Pending

**Problem:** Selection fields are always accessed together:
- `selection_mode: SelectionMode`
- `selection_start: Option<(u16, u16)>`
- `selection_end: Option<(u16, u16)>`

**Changes to `client/src/state.rs`:**
- Create a `SelectionState` struct:
  ```rust
  struct SelectionState {
      mode: SelectionMode,
      start: Option<(u16, u16)>,
      end: Option<(u16, u16)>,
  }
  ```
- Move `handle_selection_start`, `handle_selection_update`,
  `clear_selection`, `selection_cells`, `selected` methods to `impl SelectionState`.
- `TerminalState` holds `selection: SelectionState`.

**Estimated reduction:** ~0 lines (restructure), reduces field count to 15.

---

## Phase 3: Clean up `renderer.rs` color math

### 3.1 Extract `rgb_to_floats` helper

**Status:** Pending

**Problem:** `build_instances` (lines 633-763) contains ~12 instances of the pattern:
```rust
((rgb >> 16) & 0xff) as f32 / 255.0,
((rgb >> 8) & 0xff) as f32 / 255.0,
(rgb & 0xff) as f32 / 255.0,
```

**Changes to `client/src/renderer.rs`:**
- Add a private helper:
  ```rust
  fn rgb_to_floats(rgb: u32) -> (f32, f32, f32) {
      (
          ((rgb >> 16) & 0xff) as f32 / 255.0,
          ((rgb >> 8) & 0xff) as f32 / 255.0,
          (rgb & 0xff) as f32 / 255.0,
      )
  }
  ```
- Replace all inline color conversions in `build_instances` with calls to this helper.
- Also use in `render()` method's `gl_clear_color` call (lines 530-536).

**Estimated reduction:** ~30 lines of duplicated bit manipulation.

**Verification:**
- `cargo test -p terminal-client` — pass
- `render-check.sh` — headless Chromium passes

---

### 3.2 Extract `default_colors` usage

**Status:** Pending

**Problem:** The `default_colors` helper (lines 368-377) already exists but is only
used in `build_instances`. The same pattern appears in `render()` for `gl_clear_color`.

**Changes to `client/src/renderer.rs`:**
- Use `rgb_to_floats` in `render()` for `gl_clear_color` instead of inline shifts.
- Remove `default_colors` function (superseded by `rgb_to_floats`).

---

## Phase 4: Clean up `exports.rs` repetitive pattern

### 4.1 Extract `return_json` helper macro

**Status:** Pending

**Problem:** Every WASM export that returns JSON follows the same pattern:
```rust
let boxed = Box::new([ptr as u32, len as u32]);
Box::into_raw(boxed) as *mut u8
```
This appears 8 times across `init`, `process_bytes`, `query_replies`, `version`,
`selection_mode`, `selected_text`, `handle_click`, `key_to_bytes`.

**Changes to `client/src/exports.rs`:**
- Add a helper function:
  ```rust
  fn return_pair(ptr: *mut u8, len: usize) -> *mut u8 {
      let boxed = Box::new([ptr as u32, len as u32]);
      Box::into_raw(boxed) as *mut u8
  }
  ```
- Replace all 8 occurrences with `return_pair(ptr, len)`.
- Also replace the 3 error-case `[0u32, 0u32]` returns with a `return_null_pair()` helper.

**Estimated reduction:** ~20 lines.

---

## Verification Checklist

- [ ] `cargo test` — all workspace tests pass
- [ ] `cargo test -p terminal-client` — 38+ host tests pass
- [ ] `cargo build --release --target wasm32-unknown-unknown` — wasm builds clean
- [ ] `cargo build -p krust` — server builds clean
- [ ] `render-check.sh` — headless Chromium passes
- [ ] Manual browser test: text rendering, selection, cursor, scroll all work
- [ ] No new compiler warnings introduced
