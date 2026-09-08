# Implementation Plan: Krust Rust/WASM Terminal

---

## Phase 7: Dirty Cell Tracking Optimization

> Goal: Reduce rendering cost from O(rows × cols) to O(changed_cells) per frame.
> Target: 60fps rendering for typical terminal usage.

### 7.1: Add dirty cell tracking infrastructure [High]

**Objective**: Track which cells have changed since the last render to enable selective redrawing.

**Implementation**:
1. Add `prev_screen: Option<vt100::Screen>` field to `TerminalState` to store the previous screen state
2. Add `dirty_cells: Vec<(u16, u16)>` field to store coordinates of changed cells
3. Add `full_redraw: bool` flag to force complete redraw when needed (resize, scroll, etc.)

**Files to modify**:
- `client/src/state.rs`: Add new fields to `TerminalState` struct
- `client/src/state.rs`: Update `new()` to initialize new fields
- `client/src/state.rs`: Add method `mark_all_dirty()` for full redraw scenarios

**Verification**:
- `cargo test -p terminal-client` passes
- `cargo check -p terminal-client` passes
- `wasm-pack build --target web` succeeds

### 7.2: Implement screen diffing using vt100 crate [High]

**Objective**: Use vt100's `contents_diff()` to detect which cells changed between frames.

**Implementation**:
1. Before processing new bytes, clone the current screen state
2. After processing bytes, compute diff between new and previous screen
3. Parse the diff output to extract changed cell coordinates
4. Store changed cells in `dirty_cells` vector

**Key insight**: The vt100 crate's `contents_diff()` returns ANSI escape sequences describing the changes. We can parse these to determine which cells were modified, or more simply, we can compare cells directly since we have access to both screen states.

**Alternative approach**: Instead of parsing the diff output, compare cells directly:
- For each cell in the visible screen, compare with previous screen
- If text, foreground color, background color, or attributes differ, mark as dirty
- This is simpler and more reliable than parsing ANSI diff output

**Files to modify**:
- `client/src/state.rs`: Add `compute_dirty_cells()` method
- `client/src/state.rs`: Update `process_bytes()` to call diff computation

**Verification**:
- `cargo test -p terminal-client` passes
- Add unit tests for dirty cell detection
- Manual testing with various terminal outputs (ls, cat, vim, etc.)

### 7.3: Modify Canvas 2D renderer for selective redrawing [High]

**Objective**: Only redraw cells that are in the `dirty_cells` list.

**Implementation**:
1. In `render_canvas2d()`, check if `full_redraw` is set
2. If `full_redraw` is true, clear entire canvas and redraw all cells (current behavior)
3. If `full_redraw` is false, only clear and redraw cells in `dirty_cells`
4. For each dirty cell:
   - Clear the cell area with background color
   - Draw background rect if non-default
   - Draw selection background if selected
   - Draw text content
5. Always redraw cursor cell (it moves even when content doesn't change)

**Optimization details**:
- Use `ctx.clear_rect()` for individual cells instead of clearing entire canvas
- Batch similar operations (all background rects, then all text)
- Maintain font state across dirty cells to avoid redundant `set_font()` calls

**Files to modify**:
- `client/src/state.rs`: Modify `render_canvas2d()` method
- `client/src/state.rs`: Add `render_dirty_cells()` helper method

**Verification**:
- `cargo test -p terminal-client` passes
- Visual testing: terminal output looks identical to before
- Performance testing: measure frame times with `performance.now()`

### 7.4: Add frame coalescing for WebSocket messages [Medium]

**Objective**: Batch multiple WebSocket messages into a single render call.

**Current behavior**: Each WebSocket message triggers immediate `process_bytes()` → `render()`.

**New behavior**: 
1. When WebSocket message arrives, call `process_bytes()` but defer rendering
2. Use `requestAnimationFrame()` to schedule a single render per frame
3. Multiple messages within one frame are batched into one render

**Implementation**:
1. Add `needs_render: bool` flag to track if rendering is needed
2. Modify `process_bytes()` to set `needs_render = true` instead of calling `render()`
3. Add `schedule_render()` method that uses `requestAnimationFrame()` 
4. JavaScript side calls `schedule_render()` after `process_bytes()` returns

**Files to modify**:
- `client/src/state.rs`: Add `needs_render` field and `schedule_render()` method
- `client/src/exports.rs`: Modify `process_bytes()` to not render immediately
- `client/res/server.html`: Add `requestAnimationFrame()` loop

**Verification**:
- `cargo test -p terminal-client` passes
- Test with rapid output (e.g., `yes` command) - should see smooth rendering
- Measure CPU usage - should be lower with batched renders

### 7.5: Handle edge cases and special scenarios [Medium]

**Objective**: Ensure correctness for all terminal operations.

**Edge cases to handle**:
1. **Terminal resize**: Mark all cells as dirty, recalculate dimensions
2. **Scrollback changes**: Mark visible area as dirty when scrolling
3. **Selection changes**: Mark selected/unselected cells as dirty
4. **Cursor movement**: Always redraw cursor cell
5. **Clear screen (ANSI escape)**: Mark all cells as dirty
6. **Alternate screen buffer**: Mark all cells as dirty on switch

**Implementation**:
- Add `mark_all_dirty()` method that sets `full_redraw = true`
- Call `mark_all_dirty()` in appropriate scenarios
- Ensure selection rendering works correctly with dirty tracking

**Files to modify**:
- `client/src/state.rs`: Add `mark_all_dirty()` method
- `client/src/state.rs`: Call `mark_all_dirty()` in resize, scroll, selection changes

**Verification**:
- `cargo test -p terminal-client` passes
- Test resize scenarios
- Test scrolling with selection
- Test vim/nano/htop (use alternate screen buffer)

### 7.6: Performance measurement and optimization [Low]

**Objective**: Measure performance improvement and optimize further if needed.

**Implementation**:
1. Add frame timing measurement in JavaScript
2. Log average FPS over last 60 frames
3. Compare with baseline (full redraw every frame)
4. Profile hot paths if needed

**Metrics to measure**:
- Frame time (ms per render)
- CPU usage
- Memory usage
- Dirty cells per frame (average, max)

**Files to modify**:
- `client/res/server.html`: Add FPS measurement and logging
- `client/src/state.rs`: Add optional timing instrumentation

**Verification**:
- Achieve 60fps for typical terminal usage
- CPU usage reduced compared to baseline
- No visual regressions

---

## Summary

This plan implements dirty cell tracking to optimize rendering performance from O(rows × cols) to O(changed_cells) per frame. The key insights are:

1. **vt100 crate provides diff capabilities** - We can leverage `contents_diff()` or compare cells directly
2. **Canvas 2D supports selective clearing** - `clear_rect()` for individual cells is efficient
3. **Frame coalescing reduces overhead** - Multiple WebSocket messages can be batched
4. **Edge cases require full redraws** - Resize, scroll, and selection changes need special handling

Expected outcome: 60fps rendering for typical terminal usage with reduced CPU and memory overhead.