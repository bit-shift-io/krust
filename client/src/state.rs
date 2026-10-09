// Client terminal state.
//
// Holds the vt100 parser, the active renderer (WebGL2 primary, Canvas 2D
// fallback), cell dimensions, and selection state. Exposes the methods the
// WASM exports mutate it through.

use std::borrow::Cow;
use std::cell::RefCell;
use vt100::Parser;

use crate::color::{cell_visual, color_to_rgb, default_bg, default_fg, CellOverride};
use crate::cursor::{apply_decscusr, strip_rect, CursorStyle, SyncGate};
use crate::ffi::{self, JsHandle};
use crate::graphics::draw_graphic_cell;
use crate::measure::{
    css_color, device_pitch, fit_grid, measure_cell_dimensions_scratch, CELL_EPSILON, FONT_STACK,
    FONT_STACK_BOLD,
};
use crate::modes::FocusReporting;
use crate::renderer;
use crate::selection::{cell_is_selected, normalized_bounds, SelectionMode};

pub(crate) const DEFAULT_ROWS: u16 = 24;
pub(crate) const DEFAULT_COLS: u16 = 80;
pub(crate) const SCROLLBACK_LEN: usize = 1024;

/// The `vt100` crate implements the ANSI save/restore cursor sequences
/// (`ESC 7`/`ESC 8`) but silently ignores the CSI equivalents (`CSI s`/`CSI u`)
/// that many TUIs (opencode, zsh) emit. Because DECSET/DECRESET 1049 save and
/// restore the normal-grid cursor via `decsc`/`decrc`, ignoring `CSI s`/`CSI u`
/// makes the cursor land at the home position after exiting the alternate
/// screen. This maps `CSI s`/`CSI u` onto the implemented `ESC 7`/`ESC 8`
/// before the bytes reach the parser.
///
/// Sequence tails that may straddle a `process_bytes` chunk boundary are
/// carried in `carry` across calls.
pub(crate) fn normalize_save_restore(bytes: &[u8], carry: &mut Vec<u8>) -> Vec<u8> {
    let mut feed = std::mem::take(carry);
    feed.extend_from_slice(bytes);

    let carry_from =
        if feed.len() >= 2 && feed[feed.len() - 2] == 0x1b && feed[feed.len() - 1] == b'[' {
            feed.len() - 2
        } else if !feed.is_empty() && feed[feed.len() - 1] == 0x1b {
            feed.len() - 1
        } else {
            feed.len()
        };

    let mut out = Vec::with_capacity(feed.len());
    let mut i = 0;
    while i < carry_from {
        if i + 2 < feed.len()
            && feed[i] == 0x1b
            && feed[i + 1] == b'['
            && (feed[i + 2] == b's' || feed[i + 2] == b'u')
        {
            out.push(0x1b);
            out.push(if feed[i + 2] == b's' { b'7' } else { b'8' });
            i += 3;
        } else {
            out.push(feed[i]);
            i += 1;
        }
    }
    if carry_from < feed.len() {
        *carry = feed[carry_from..].to_vec();
    }
    out
}

/// A hyperlink created by an `OSC 8` sequence, tied to a screen region.
///
/// `vt100` has no per-cell hyperlink attribute, so a link is recorded as the
/// cursor span between the opening and closing `OSC 8`. `end` is exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct LinkSpan {
    pub(crate) uri: String,
    pub(crate) start: (u16, u16),
    pub(crate) end: (u16, u16),
}

/// Keep at most this many links; `ls --hyperlink` on a huge directory emits one
/// per entry and only the visible ones can ever be hovered.
const MAX_LINKS: usize = 256;

/// Whether `(row, col)` falls inside `span`, accounting for wrapped rows.
pub(crate) fn link_contains(span: &LinkSpan, row: u16, col: u16) -> bool {
    let (sr, sc) = span.start;
    let (er, ec) = span.end;
    if (er, ec) <= (sr, sc) {
        return false;
    }
    if row < sr || row > er {
        return false;
    }
    if sr == er {
        return row == sr && col >= sc && col < ec;
    }
    if row == sr {
        col >= sc
    } else if row == er {
        col < ec
    } else {
        true
    }
}

/// The URI of the last link covering `(row, col)`, if any.
pub(crate) fn hyperlink_at(links: &[LinkSpan], row: u16, col: u16) -> Option<&str> {
    links
        .iter()
        .rev()
        .find(|span| link_contains(span, row, col))
        .map(|span| span.uri.as_str())
}

/// vt100 parser callbacks for terminal events that do not affect the screen.
///
/// `vt100`'s `Screen` does not expose bells or hyperlinks, so they are
/// surfaced through the parser's callback state instead.
#[derive(Default)]
pub(crate) struct TerminalCallbacks {
    /// A bell (audible or visual) has rung since the page last took it.
    pub(crate) bell_pending: bool,
    /// Completed `OSC 8` hyperlinks, in the order they were closed.
    pub(crate) links: Vec<LinkSpan>,
    /// The currently open `OSC 8` link: its start cell and URI.
    open_link: Option<((u16, u16), String)>,
    /// Base64 payload from the last `OSC 52` copy request not yet taken.
    pub(crate) clipboard: Option<String>,
    /// Working directory from the last `OSC 7` sequence, not yet taken.
    pub(crate) cwd: Option<String>,
    /// Last `OSC 133` prompt mark (`A`/`B`/`C`/`D`), not yet taken.
    pub(crate) prompt_mark: Option<u8>,
}

impl TerminalCallbacks {
    /// Close the currently open link at `pos`, recording it if it covered any
    /// cells.
    fn close_link(&mut self, pos: (u16, u16)) {
        if let Some((start, uri)) = self.open_link.take() {
            if start != pos {
                self.links.push(LinkSpan {
                    uri,
                    start,
                    end: pos,
                });
                if self.links.len() > MAX_LINKS {
                    let drop = self.links.len() - MAX_LINKS;
                    self.links.drain(..drop);
                }
            }
        }
    }

    /// Clear all recorded link spans and any open link.
    ///
    /// Called when the terminal screen is fully cleared (ED 2 / `\x1b[2J`)
    /// or reset, so old hyperlinks do not attach to newly drawn text.
    pub(crate) fn clear_links(&mut self) {
        self.links.clear();
        self.open_link = None;
    }
}

impl vt100::Callbacks for TerminalCallbacks {
    fn audible_bell(&mut self, _: &mut vt100::Screen) {
        self.bell_pending = true;
    }

    fn visual_bell(&mut self, _: &mut vt100::Screen) {
        self.bell_pending = true;
    }

    fn copy_to_clipboard(&mut self, _: &mut vt100::Screen, _ty: &[u8], data: &[u8]) {
        // `vt100` hands us a base64 payload; keep it encoded so the page can
        // decode it with `atob` (which knows the browser's text conventions).
        self.clipboard = Some(String::from_utf8_lossy(data).into_owned());
    }

    fn unhandled_osc(&mut self, screen: &mut vt100::Screen, params: &[&[u8]]) {
        // OSC 4 / 10 / 11 recolor the palette; they are not screen state.
        if crate::color::apply_dynamic_color(params) {
            return;
        }
        if params.first() == Some(&b"7".as_slice()) {
            // OSC 7 ; file://host/path ST reports the shell's working directory.
            let mut uri = Vec::new();
            for (i, part) in params.iter().skip(1).enumerate() {
                if i > 0 {
                    uri.push(b';');
                }
                uri.extend_from_slice(part);
            }
            self.cwd = Some(String::from_utf8_lossy(&uri).into_owned());
            return;
        }
        if params.first() == Some(&b"133".as_slice()) {
            // OSC 133 ; A|B|C|D marks prompt/command boundaries.
            if let Some(mark) = params.get(1).and_then(|p| p.first()) {
                self.prompt_mark = Some(*mark);
            }
            return;
        }
        if params.first() != Some(&b"8".as_slice()) {
            return;
        }
        let pos = screen.cursor_position();
        self.close_link(pos);
        let mut uri = Vec::new();
        for (i, part) in params.iter().skip(2).enumerate() {
            if i > 0 {
                uri.push(b';');
            }
            uri.extend_from_slice(part);
        }
        if !uri.is_empty() {
            self.open_link = Some((pos, String::from_utf8_lossy(&uri).into_owned()));
        }
    }
}

/// The bytes to send to the PTY for a paste of `text`.
///
/// When the application enabled bracketed paste (DECSET 2004), the text is
/// wrapped in `ESC[200~` / `ESC[201~` so the shell inserts it literally rather
/// than interpreting embedded newlines or control characters. Otherwise the
/// bytes go through verbatim.
pub(crate) fn bracketed_paste_bytes(text: &str, enabled: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(text.len() + if enabled { 12 } else { 0 });
    if enabled {
        out.extend_from_slice(b"\x1b[200~");
    }
    out.extend_from_slice(text.as_bytes());
    if enabled {
        out.extend_from_slice(b"\x1b[201~");
    }
    out
}

/// Scroll-related state, grouped for clarity.
struct ScrollState {
    /// Normal screen scrollback offset saved when entering alternate screen
    saved_normal_offset_for_alt: Option<usize>,
    /// Scrollback view offset for the normal screen (0 = at bottom, >0 = scrolled up)
    normal_offset: usize,
    /// Scrollback view offset for the alternate screen (typically always 0)
    alternate_offset: usize,
}

impl ScrollState {
    fn new() -> Self {
        Self {
            saved_normal_offset_for_alt: None,
            normal_offset: 0,
            alternate_offset: 0,
        }
    }

    fn offset(&self, alternate: bool) -> usize {
        if alternate {
            self.alternate_offset
        } else {
            self.normal_offset
        }
    }

    fn set_offset(&mut self, alternate: bool, offset: usize) {
        if alternate {
            self.alternate_offset = offset;
        } else {
            self.normal_offset = offset;
        }
    }
}

/// Selection-related state, grouped for clarity.
struct SelectionState {
    /// Selection mode
    mode: SelectionMode,
    /// Text selection start cell
    start: Option<(u16, u16)>,
    /// Text selection end cell
    end: Option<(u16, u16)>,
}

impl SelectionState {
    fn new() -> Self {
        Self {
            mode: SelectionMode::None,
            start: None,
            end: None,
        }
    }

    fn clear(&mut self) {
        self.start = None;
        self.end = None;
        self.mode = SelectionMode::None;
    }

    fn is_selected(&self, row: u16, col: u16) -> bool {
        match (self.start, self.end) {
            (Some(a), Some(b)) => cell_is_selected(a, b, row, col),
            _ => false,
        }
    }

    fn cells(&self, cols: u16) -> Vec<(u16, u16)> {
        let (Some(a), Some(b)) = (self.start, self.end) else {
            return Vec::new();
        };
        let ((sr, sc), (er, ec)) = normalized_bounds(a, b);
        let cols = cols.max(1);
        let mut cells = Vec::new();
        for row in sr..=er {
            let c0 = if row == sr { sc } else { 0 };
            let c1 = if row == er {
                ec.min(cols - 1)
            } else {
                cols - 1
            };
            for col in c0..=c1 {
                cells.push((row, col));
            }
        }
        cells
    }
}

/// Tracks which cells changed since the last render.
///
/// `prev_screen` is a snapshot of the screen taken when it was last diffed;
/// `dirty` is the set of cells that differ from it. The diff is computed at
/// render time, not on every inbound byte: `process_bytes` mutates the parser
/// but touches none of this, and `refresh` runs once per painted frame. That
/// coalescing is what keeps a large paste from cloning the screen (and its
/// scrollback) once per 1 KB WebSocket frame.
struct DirtyTracker {
    /// Screen snapshot the next diff compares against.
    prev_screen: Option<vt100::Screen>,
    /// Cells changed since the last render, consumed by the Canvas 2D path.
    dirty: Vec<(u16, u16)>,
    /// When set, the next render repaints the whole grid and the diff is
    /// skipped (first frame, resize, reset, scroll, selection change).
    full_redraw: bool,
}

impl DirtyTracker {
    fn new() -> Self {
        Self {
            prev_screen: None,
            dirty: Vec::new(),
            full_redraw: true,
        }
    }

    /// Force a full-grid repaint on the next render.
    fn mark_all_dirty(&mut self) {
        self.full_redraw = true;
        self.dirty.clear();
    }

    /// Drop the baseline too, so the next diff treats every cell as changed.
    fn reset_baseline(&mut self) {
        self.prev_screen = None;
        self.mark_all_dirty();
    }

    /// Recompute the dirty set against `screen`, then snapshot it as the next
    /// baseline. A pending full redraw skips the scan but still refreshes the
    /// baseline, so the render after it is incremental.
    fn refresh(&mut self, screen: &vt100::Screen, rows: u16, cols: u16) {
        if self.full_redraw {
            self.dirty.clear();
        } else if let Some(prev) = self.prev_screen.take() {
            let (prows, pcols) = screen.size();
            let rows = if rows > 0 { rows } else { prows };
            let cols = if cols > 0 { cols } else { pcols };
            self.dirty = diff_screens(&prev, screen, rows, cols);
        } else {
            // No baseline: treat this as a full redraw rather than painting
            // nothing.
            self.full_redraw = true;
        }
        self.prev_screen = Some(screen.clone());
    }
}

/// Terminal state fields.
///
/// Holds the vt100 parser, the active renderer (WebGL2 primary, Canvas 2D
/// fallback), cell dimensions, and selection state. Exposes the methods the
/// WASM exports mutate it through.
pub(crate) struct TerminalState {
    /// vt100 parser
    parser: Parser<TerminalCallbacks>,
    /// 2D rendering context (Canvas 2D fallback path)
    ctx: Option<JsHandle>,
    /// WebGL2 renderer (primary path)
    webgl: Option<renderer::WebGL2Renderer>,
    /// Canvas element (source of pixel dimensions)
    canvas: JsHandle,
    /// Canvas element ID
    canvas_id: String,
    /// Terminal dimensions in cells
    pub(crate) rows: u16,
    pub(crate) cols: u16,
    /// Measured cell width in CSS pixels
    pub(crate) cell_width: f64,
    /// Measured cell height in CSS pixels
    pub(crate) cell_height: f64,
    /// Pixel offset of the grid inside the canvas, in device pixels. The grid
    /// holds as many whole cells as fit and is centered in the leftover, so
    /// this is what keeps the terminal inside the window instead of running
    /// off the right/bottom edge.
    origin_x: i64,
    origin_y: i64,
    /// Scroll-related state
    scroll: ScrollState,
    /// Selection-related state
    selection: SelectionState,
    /// Cells changed since the last render, plus the screen snapshot the diff
    /// compares against. Recomputed at render time, not per inbound frame.
    dirty: DirtyTracker,
    /// Last rendered cursor cell, so the old highlight can be cleared when
    /// the cursor moves or becomes hidden.
    prev_cursor: Option<(u16, u16)>,
    /// True when a render pass has been scheduled via `schedule_render`
    /// and needs to be flushed on the next JS animation frame.
    needs_render: bool,
    /// Incomplete tail of an ESC sequence from the previous `process_bytes`
    /// call that may yet form a `CSI s`/`CSI u` cursor save/restore.
    csi_su_carry: Vec<u8>,
    /// Incomplete tail of a DECSCUSR (`CSI Ps SP q`) sequence from the
    /// previous `process_bytes` call.
    dscsr_carry: Vec<u8>,
    /// Cursor shape/blink requested by the application via DECSCUSR.
    cursor_style: CursorStyle,
    /// Current blink phase for a blinking [`Self::cursor_style`] (toggled by
    /// the page's `blink_tick` interval).
    blink_on: bool,
    /// Withholds the body of a `?2026`-synchronized frame until it is
    /// complete, so a repaint never lands mid-frame.
    sync: SyncGate,
    /// Focus-reporting mode (DECSET 1004), tracked out of band because `vt100`
    /// does not model it.
    focus: FocusReporting,
}

/// Renderer selection override, set from JS before `init()` via
/// `set_renderer_mode`. 0 = auto (WebGL2 first, Canvas 2D fallback),
/// 1 = force WebGL2 (error if unavailable), 2 = force Canvas 2D.
pub static RENDERER_MODE: std::sync::atomic::AtomicI32 = std::sync::atomic::AtomicI32::new(0);

impl TerminalState {
    /// Create a new terminal state with a Canvas 2D rendering context
    ///
    /// # Parameters
    /// * `canvas_id` - HTML canvas element ID
    pub(crate) fn new(canvas_id: &str, cached: Option<(f64, f64)>) -> Result<Self, String> {
        let parser = Parser::new_with_callbacks(
            DEFAULT_ROWS,
            DEFAULT_COLS,
            SCROLLBACK_LEN,
            TerminalCallbacks::default(),
        );

        let win = ffi::window();
        let doc = if win != 0 {
            ffi::window_document(win)
        } else {
            0
        };
        let canvas = if doc != 0 {
            ffi::document_get_element_by_id(doc, canvas_id)
        } else {
            0
        };
        if canvas == 0 {
            return Err(format!("canvas '#{}' not found", canvas_id));
        }

        // Try Canvas 2D first; fall back to WebGL2
        let dpr = ffi::window_dpr(win);

        // Use cached cell dims when available (avoids scratch canvas measurement
        // on repeat visits). Fall back to a scratch canvas measurement, then to
        // sensible defaults. Snap to a whole device-pixel pitch so the CSS size,
        // the GL cell size and the column/row fit all agree.
        let dpr_eff = dpr.max(1.0);
        let (cell_width, cell_height) = cached
            .filter(|(w, h)| *w > 0.0 && *h > 0.0)
            .or_else(measure_cell_dimensions_scratch)
            .unwrap_or((14.0, 20.0));
        let cell_width = device_pitch(cell_width, dpr_eff) as f64 / dpr_eff;
        let cell_height = device_pitch(cell_height, dpr_eff) as f64 / dpr_eff;

        // Try WebGL2 first for GPU-accelerated rendering; fall back to Canvas 2D.
        // `?r=gl`/`?r=2d` (via set_renderer_mode) force one renderer for A/B
        // comparison: mode 1 fails hard when WebGL2 is unavailable; mode 2
        // skips WebGL2 entirely.
        let mode = RENDERER_MODE.load(std::sync::atomic::Ordering::Relaxed);
        let mut webgl = None;
        if mode != 2 {
            match renderer::WebGL2Renderer::new(
                canvas_id,
                cell_width,
                cell_height,
                DEFAULT_ROWS,
                DEFAULT_COLS,
                dpr,
            ) {
                Ok(w) => {
                    ffi::console_log(if mode == 1 {
                        "KRUST: WebGL2 renderer initialized (forced r=gl)"
                    } else {
                        "KRUST: WebGL2 renderer initialized"
                    });
                    webgl = Some(w);
                }
                Err(e) => {
                    if mode == 1 {
                        return Err(format!("r=gl: WebGL2 unavailable: {}", e));
                    }
                    ffi::console_log("KRUST: WebGL2 unavailable, falling back to Canvas 2D");
                }
            }
        } else {
            ffi::console_log("KRUST: forced Canvas 2D renderer (r=2d)");
        }
        // Canvas 2D fallback: only when WebGL2 could not be obtained (or was
        // skipped by the forced-2D mode).
        let ctx = if webgl.is_none() {
            let c = ffi::canvas_get_2d(canvas);
            (c != 0).then_some(c)
        } else {
            None
        };
        if ctx.is_none() && webgl.is_none() {
            return Err(
                "no rendering context available (WebGL2 and Canvas 2D both failed)".to_string(),
            );
        }

        let canvas_w = ffi::element_offset_width(canvas);
        let canvas_h = ffi::element_offset_height(canvas);

        let mut state = TerminalState {
            parser,
            ctx,
            webgl,
            canvas,
            canvas_id: canvas_id.to_string(),
            rows: DEFAULT_ROWS,
            cols: DEFAULT_COLS,
            cell_width,
            cell_height,
            origin_x: 0,
            origin_y: 0,
            scroll: ScrollState::new(),
            selection: SelectionState::new(),
            dirty: DirtyTracker::new(),
            prev_cursor: None,
            needs_render: false,
            csi_su_carry: Vec::new(),
            dscsr_carry: Vec::new(),
            cursor_style: CursorStyle::default(),
            blink_on: true,
            sync: SyncGate::new(),
            focus: FocusReporting::new(),
        };
        if canvas_w > 0.0 && canvas_h > 0.0 {
            state.refit(
                (canvas_w * dpr_eff).round() as i64,
                (canvas_h * dpr_eff).round() as i64,
            );
        }
        state.parser.screen_mut().set_size(state.rows, state.cols);
        Ok(state)
    }

    /// Process incoming ANSI bytes through the VT100 parser
    pub(crate) fn process_bytes(&mut self, bytes: &[u8]) {
        // Clamp the current scroll offset to the (possibly shrunken) history
        // before new bytes arrive, so we always stay within valid range.
        self.clamp_scroll();
        let normalized = normalize_save_restore(bytes, &mut self.csi_su_carry);
        let gated = self.sync.push(&normalized);
        self.feed_parser(gated);
    }

    /// Feed sync-gated bytes through the DECSCUSR scan into the parser,
    /// keeping the alternate-screen bookkeeping and dirty-cell diff around
    /// the transition. Split out so `flush_sync` goes through exactly the
    /// same path.
    fn feed_parser(&mut self, gated: Cow<[u8]>) {
        let screen_before = self.parser.screen().alternate_screen();

        let feed = apply_decscusr(&gated, &mut self.dscsr_carry, &mut self.cursor_style);
        let feed = self.focus.scan(&feed);
        if !feed.is_empty() {
            self.parser.process(&feed);
        }

        let screen_after = self.parser.screen().alternate_screen();

        if !screen_before && screen_after {
            // Entering alternate screen: clear links from normal screen
            self.parser.callbacks_mut().clear_links();
            self.scroll.saved_normal_offset_for_alt = Some(self.scroll.normal_offset);
            self.parser
                .screen_mut()
                .set_scrollback(self.scroll.normal_offset);
        } else if screen_before && !screen_after {
            // Exiting alternate screen: clear links from alt screen
            self.parser.callbacks_mut().clear_links();
            if let Some(saved) = self.scroll.saved_normal_offset_for_alt.take() {
                self.scroll.normal_offset = saved;
                self.parser.screen_mut().set_scrollback(saved);
            }
        }
        // Dirty cells are intentionally *not* computed here: the diff runs at
        // render time (see `render_canvas2d`), so a burst of WebSocket frames
        // between two animation frames costs one diff, not one per frame.
    }

    /// Whether the sync gate is withholding bytes (an open `?2026` frame or
    /// a partial start marker). The page arms a stall timer while this is 1.
    pub(crate) fn sync_pending(&self) -> bool {
        self.sync.pending()
    }

    /// Force-feed whatever the sync gate holds. Called by the page's stall
    /// timer when a frame's end marker never arrived; feeding the whole
    /// buffer in one go is still atomic, so the worst case is an
    /// incomplete frame appearing at once rather than a frozen screen.
    pub(crate) fn flush_sync(&mut self) {
        if !self.sync.pending() {
            return;
        }
        let gated = self.sync.flush();
        if !gated.is_empty() {
            self.feed_parser(Cow::Owned(gated));
        }
    }

    /// Advance the blink phase. Returns true when the active style blinks
    /// (so the caller knows a repaint was actually needed).
    pub(crate) fn toggle_blink_phase(&mut self) -> bool {
        if self.cursor_style.blink {
            self.blink_on = !self.blink_on;
            true
        } else {
            false
        }
    }

    /// Mark every cell dirty so the next render is a full redraw.
    pub(crate) fn mark_all_dirty(&mut self) {
        self.dirty.mark_all_dirty();
    }

    /// Returns the scroll offset for the currently active screen.
    fn active_scroll_offset(&self) -> usize {
        let screen = self.parser.screen();
        self.scroll.offset(screen.alternate_screen())
    }

    /// Clamp the current scroll offset to the actual scrollback bounds.
    fn clamp_scroll(&mut self) {
        let max = self.active_scrollback_len();
        let cur = self.active_scroll_offset();
        if cur > max {
            let screen = self.parser.screen();
            self.scroll.set_offset(screen.alternate_screen(), max);
        }
    }

    /// Returns the scrollback length for the currently active screen.
    fn active_scrollback_len(&mut self) -> usize {
        let offset = self.active_scroll_offset();
        let screen = self.parser.screen_mut();
        screen.set_scrollback(usize::MAX);
        let max = screen.scrollback();
        screen.set_scrollback(offset);
        max
    }

    /// Returns the scrollback length for the currently active screen.
    pub(crate) fn scrollback_len(&mut self) -> usize {
        self.active_scrollback_len()
    }

    /// Whether the alternate screen is active.
    ///
    /// Full-screen TUIs (opencode, vim, less, htop) own the screen and manage
    /// their own scrolling, so the alt screen's grid — which vt100 keeps
    /// separate from the normal screen, and which still accumulates rows when
    /// the app scrolls the full region up — is not offered as scrollback.
    pub(crate) fn is_alt_screen(&self) -> bool {
        self.parser.screen().alternate_screen()
    }

    /// Current scrollback view offset (0 = active screen at bottom, >0 = scrolled up).
    pub(crate) fn scroll_offset(&self) -> usize {
        self.active_scroll_offset()
    }

    /// Set the scrollback view offset for the currently active screen.
    pub(crate) fn set_scroll_offset(&mut self, offset: usize) {
        let current = self.active_scroll_offset();
        if current != offset {
            self.clear_selection();
            let screen = self.parser.screen();
            self.scroll.set_offset(screen.alternate_screen(), offset);
            self.clamp_scroll();
        }
        self.apply_scrollback();
        self.mark_all_dirty();
    }

    /// Adjust the scrollback view by `delta` lines (positive = up/back,
    /// negative = down/forward). Returns the resulting offset.
    pub(crate) fn scroll_by(&mut self, delta: isize) -> usize {
        let cur = self.active_scroll_offset() as isize;
        let next = (cur + delta).max(0);
        self.set_scroll_offset(next as usize);
        self.active_scroll_offset()
    }

    /// Snap the view to the bottom (active screen).
    pub(crate) fn scroll_to_bottom(&mut self) {
        self.set_scroll_offset(0);
    }

    /// Snap the view to the oldest available history row on the active screen.
    pub(crate) fn scroll_to_top(&mut self) {
        let max = self.active_scrollback_len();
        self.set_scroll_offset(max);
    }

    /// Apply the current scroll offset to the parser screen view for the
    /// currently active screen.
    fn apply_scrollback(&mut self) {
        let screen_alt = self.parser.screen().alternate_screen();
        let offset = self.scroll.offset(screen_alt);
        if self.parser.screen().scrollback() != offset {
            self.parser.screen_mut().set_scrollback(offset);
        }
    }

    /// Dispatches to the active renderer: WebGL2 by default, Canvas 2D
    /// when WebGL2 was unavailable. Honors the `needs_render` flag set by
    /// `schedule_render` to enable frame coalescing.
    pub(crate) fn render(&mut self) -> Result<(), String> {
        // Browsers may drop the WebGL context while a tab or iframe is hidden
        // (notably on Firefox under memory pressure, but also Chrome when the
        // browser is under pressure). When the context is lost, all GL calls
        // become silent no-ops and the terminal freezes as a blank canvas
        // until a page reload. Detect this here and rebuild the renderer so
        // the next frame paints correctly as soon as the context is restored.
        if let Some(w) = self.webgl.as_mut() {
            // Check if the underlying WebGL context has been lost. If so,
            // rebuild the renderer to restore drawing.
            if ffi::gl_is_context_lost(w.ctx) {
                let rebuilt = self.rebuild_webgl();
                if rebuilt.is_err() {
                    // If we cannot rebuild (no GL context available), fall back
                    // to Canvas 2D renderer which is more resilient.
                    return self.render_canvas2d();
                }
            }
        }
        // Always render (caller decides when to invoke). The `needs_render`
        // flag only controls full vs selective strategy inside render_canvas2d.
        self.needs_render = false;
        self.apply_scrollback();
        if self.webgl.is_some() {
            // Compute everything that borrows `self` before taking the mutable
            // WebGL borrow, which may bake new glyphs into the atlas.
            let scroll_offset = self.active_scroll_offset();
            let selection = self.selection_cells();
            let style = self.cursor_style;
            let screen = self.parser.screen();
            // Sentinel when the cursor must not be drawn: scrolled into
            // history, DECTCEM-hidden (TUIs hide it around every repaint),
            // or in the off phase of a blinking style.
            let cursor = match cursor_draw_pos(screen, scroll_offset, self.rows, self.cols) {
                Some(pos) if style.blink_visible(self.blink_on) => pos,
                _ => (u16::MAX, u16::MAX),
            };
            if let Some(w) = self.webgl.as_mut() {
                // The GL renderer rebuilds its instance buffer from the screen
                // every frame, so the Canvas 2D dirty set is neither needed nor
                // computed on this path. Nothing should have built one.
                debug_assert!(
                    self.dirty.dirty.is_empty(),
                    "GL render path must not carry a Canvas 2D dirty set"
                );
                return w.render(screen, default_fg(), default_bg(), &selection, cursor, style);
            }
        }
        self.render_canvas2d()
    }

    /// Build the list of cells in the active line-based (text-flow)
    /// selection. The anchor row runs from the anchor column to the end of
    /// the row, intermediate rows are selected in full, and the last row runs
    /// from column 0 to the end column.
    fn selection_cells(&self) -> Vec<(u16, u16)> {
        self.selection.cells(self.cols)
    }

    /// Draw the current parser screen with native Canvas 2D text.
    ///
    /// Renders either the whole grid (first frame, resize, scroll, selection
    /// change) or only the cells that changed since the last frame, keeping
    /// redraw cost proportional to the amount of new output.
    fn render_canvas2d(&mut self) -> Result<(), String> {
        // Clone the (cheap) context handle so no borrow of `self` lingers
        // while the render helpers mutate other fields.
        let ctx = self.ctx.ok_or("no renderer")?;
        let (prows, pcols) = self.parser.screen().size();
        let rows = if self.rows > 0 { self.rows } else { prows };
        let cols = if self.cols > 0 { self.cols } else { pcols };
        let cw = self.cell_width;
        let ch = self.cell_height;

        let dpr = ffi::window_dpr(ffi::window());

        // Compute the dirty set here, at render time: many WebSocket frames
        // may have arrived since the last painted frame, and they coalesce
        // into a single diff instead of one diff each. A pending full redraw
        // (resize, scroll, selection, reset, first frame) skips the scan.
        self.dirty.refresh(self.parser.screen(), rows, cols);

        // Full redraw when forced, or when enough of the grid changed that a
        // selective pass would cost more than just repainting everything.
        let total = (rows as usize).saturating_mul(cols as usize);
        let dirty_count = self.dirty.dirty.len();
        let full = self.dirty.full_redraw || dirty_count >= total / 2;
        self.dirty.full_redraw = false;

        let dirty = std::mem::take(&mut self.dirty.dirty);

        ffi::ctx_set_transform(ctx, dpr.max(1.0), 0.0, 0.0, dpr.max(1.0), 0.0, 0.0);

        let (ox, oy) = self.origin_css(dpr);
        if full {
            self.render_full_grid(ctx, rows, cols, cw, ch, dpr, ox, oy)
        } else {
            self.render_dirty_cells(ctx, rows, cols, cw, ch, dpr, ox, oy, dirty)
        }?;
        Ok(())
    }

    /// Repaint the entire grid (first frame, resize, scroll, selection change).
    #[allow(clippy::too_many_arguments)]
    fn render_full_grid(
        &mut self,
        ctx: JsHandle,
        rows: u16,
        cols: u16,
        cw: f64,
        ch: f64,
        dpr: f64,
        ox: f64,
        oy: f64,
    ) -> Result<(), String> {
        let screen = self.parser.screen();
        let (prows, pcols) = screen.size();
        let css_w = ffi::canvas_width(self.canvas) as f64 / dpr;
        let css_h = ffi::canvas_height(self.canvas) as f64 / dpr;

        // Clear to default background
        ffi::ctx_set_fill_style(ctx, &css_color(default_bg()));
        ffi::ctx_fill_rect(ctx, 0.0, 0.0, css_w.max(1.0), css_h.max(1.0));
        ffi::ctx_set_text_baseline(ctx, "middle");

        let mut font = FONT_STACK.to_string();
        ffi::ctx_set_font(ctx, &font);

        // Paint each cell
        for row in 0..rows {
            for col in 0..cols {
                self.paint_cell(
                    ctx, screen, row, col, cw, ch, prows, pcols, &mut font, ox, oy,
                );
            }
        }

        // Cursor: background then text (hidden when scrolled into history)
        let cur = self.visible_cursor(screen, rows, cols);
        let cursor_pos = self.draw_cursor(ctx, cur, cw, ch, ox, oy)?;
        self.prev_cursor = cursor_pos;
        Ok(())
    }

    /// Redraw only cells that changed since the last render, plus the cursor.
    #[allow(clippy::too_many_arguments)]
    fn render_dirty_cells(
        &mut self,
        ctx: JsHandle,
        rows: u16,
        cols: u16,
        cw: f64,
        ch: f64,
        dpr: f64,
        ox: f64,
        oy: f64,
        mut dirty: Vec<(u16, u16)>,
    ) -> Result<(), String> {
        let screen = self.parser.screen();
        let (prows, pcols) = screen.size();
        let _css_w = ffi::canvas_width(self.canvas) as f64 / dpr;
        let _css_h = ffi::canvas_height(self.canvas) as f64 / dpr;

        // Cells to repaint: everything marked dirty, plus current and previous cursor cells
        let cur = self.visible_cursor(screen, rows, cols);
        if let Some(c) = cur {
            dirty.push(c);
        }
        if let Some(pc) = self.prev_cursor {
            if pc.0 < rows && pc.1 < cols {
                dirty.push(pc);
            }
        }
        dirty.sort_unstable();
        dirty.dedup();

        ffi::ctx_set_text_baseline(ctx, "middle");
        let mut font = FONT_STACK.to_string();
        ffi::ctx_set_font(ctx, &font);

        for &(row, col) in dirty.iter() {
            self.paint_cell(
                ctx, screen, row, col, cw, ch, prows, pcols, &mut font, ox, oy,
            );
        }

        // Cursor drawn last, on top of the regular cell content.
        let cur = self.visible_cursor(screen, rows, cols);
        let cursor_pos = self.draw_cursor(ctx, cur, cw, ch, ox, oy)?;
        self.prev_cursor = cursor_pos;
        Ok(())
    }

    /// The cursor cell to render, or `None` when it must not be drawn.
    fn visible_cursor(&self, screen: &vt100::Screen, rows: u16, cols: u16) -> Option<(u16, u16)> {
        cursor_draw_pos(
            screen,
            self.scroll.offset(screen.alternate_screen()),
            rows,
            cols,
        )
    }

    /// Schedule a render call for the next `requestAnimationFrame` callback.
    /// This enables frame coalescing: multiple `process_bytes` calls within a
    /// single animation frame will only result in one render.
    pub(crate) fn schedule_render(&mut self) {
        self.needs_render = true;
    }

    /// Draw the cursor on top of a cell in the active [`CursorStyle`].
    /// `cursor` is the cell to draw, or `None` to skip cursor drawing.
    /// `ox`/`oy` are the grid origin in CSS pixels.
    #[allow(clippy::too_many_arguments)]
    fn draw_cursor(
        &mut self,
        ctx: JsHandle,
        cursor: Option<(u16, u16)>,
        cw: f64,
        ch: f64,
        ox: f64,
        oy: f64,
    ) -> Result<Option<(u16, u16)>, String> {
        let Some((cr, cc)) = cursor else {
            return Ok(None);
        };
        let style = self.cursor_style;
        // Blink phase off: the cell still tracks (so it is repainted when
        // the phase flips) but nothing goes on top of it.
        if !style.blink_visible(self.blink_on) {
            return Ok(Some((cr, cc)));
        }
        let screen = self.parser.screen();
        let cell = screen.cell(cr, cc);
        let (x, y) = (ox + cc as f64 * cw, oy + cr as f64 * ch);
        if let Some((sx, sy, sw, sh)) = strip_rect(style.shape, x, y, cw, ch) {
            // Underline/bar: the cell itself was already painted normally
            // (it is always in the dirty set), so only the strip goes on
            // top, in the cell's own foreground color.
            let (fg, _bg) = cell_visual(cell, default_fg(), default_bg(), CellOverride::Normal);
            ffi::ctx_set_fill_style(ctx, &css_color(fg));
            ffi::ctx_fill_rect(ctx, sx, sy, sw, sh);
            return Ok(Some((cr, cc)));
        }
        // Block: swap original fg/bg + glyph.
        let (fg, bg) = cell_visual(cell, default_fg(), default_bg(), CellOverride::Cursor);
        ffi::ctx_set_fill_style(ctx, &css_color(bg));
        ffi::ctx_fill_rect(ctx, x, y, cw, ch);
        ffi::ctx_set_fill_style(ctx, &css_color(fg));
        if let Some(c) = cell {
            let s = c.contents();
            if !s.is_empty() && !draw_graphic_cell(ctx, x, y, cw, ch, s, fg) {
                ffi::ctx_fill_text(ctx, s, x, y + ch * 0.5);
            }
        }
        Ok(Some((cr, cc)))
    }

    /// Whether the given cell lies inside the active line-based (text-flow)
    /// selection (normalized so the anchor can be above/below the end).
    fn selected(&self, row: u16, col: u16) -> bool {
        self.selection.is_selected(row, col)
    }

    /// Paint a single cell: clear to default bg, paint non-default bg,
    /// paint selection bg, and draw text glyph.
    ///
    /// `ox`/`oy` are the grid origin in CSS pixels; every rect is placed
    /// relative to it so the grid can be centered inside the canvas.
    #[allow(clippy::too_many_arguments)]
    fn paint_cell(
        &self,
        ctx: JsHandle,
        screen: &vt100::Screen,
        row: u16,
        col: u16,
        cw: f64,
        ch: f64,
        prows: u16,
        pcols: u16,
        font: &mut String,
        ox: f64,
        oy: f64,
    ) {
        let (x, y) = (ox + col as f64 * cw, oy + row as f64 * ch);

        // 1. Clear cell to default background
        ffi::ctx_set_fill_style(ctx, &css_color(default_bg()));
        ffi::ctx_fill_rect(ctx, x, y, cw, ch);

        let cell = if row < prows && col < pcols {
            screen.cell(row, col)
        } else {
            None
        };
        let selected = self.selected(row, col);
        let override_ = if selected {
            CellOverride::Selected
        } else {
            CellOverride::Normal
        };

        // 2. Non-default background rect
        let base_bg = match cell {
            Some(c) => color_to_rgb(c.bgcolor(), default_bg()),
            _ => default_bg(),
        };
        if base_bg != default_bg() && !selected {
            ffi::ctx_set_fill_style(ctx, &css_color(base_bg));
            ffi::ctx_fill_rect(ctx, x, y, cw, ch);
        }

        // 3. Selection background rect (before text)
        if selected {
            let (_fg, sel_bg) = cell_visual(cell, default_fg(), default_bg(), CellOverride::Selected);
            ffi::ctx_set_fill_style(ctx, &css_color(sel_bg));
            ffi::ctx_fill_rect(
                ctx,
                x - CELL_EPSILON,
                y - CELL_EPSILON,
                cw + CELL_EPSILON * 2.0,
                ch + CELL_EPSILON * 2.0,
            );
        }

        // 4. Text glyph
        if let Some(c) = cell {
            let s = c.contents();
            if !s.is_empty() {
                let (draw_fg, _bg) = cell_visual(Some(c), default_fg(), default_bg(), override_);
                if draw_fg != default_bg() {
                    if draw_graphic_cell(ctx, x, y, cw, ch, s, draw_fg) {
                        return;
                    }
                    let want = if c.bold() {
                        FONT_STACK_BOLD
                    } else {
                        FONT_STACK
                    };
                    if font.as_str() != want {
                        *font = want.to_string();
                        ffi::ctx_set_font(ctx, font);
                    }
                    ffi::ctx_set_fill_style(ctx, &css_color(draw_fg));
                    ffi::ctx_fill_text(ctx, s, x, y + ch * 0.5);
                }
            }
        }
    }

    /// Trigger resize callback
    pub(crate) fn trigger_resize(&mut self, _new_rows: u16, _new_cols: u16) {}

    /// Handle selection start
    pub(crate) fn handle_selection_start(&mut self, row: u16, col: u16) {
        self.selection.mode = SelectionMode::Line;
        self.selection.start = Some((row, col));
        self.selection.end = None;
        self.mark_all_dirty();
    }

    /// Handle selection update
    pub(crate) fn handle_selection_update(&mut self, row: u16, col: u16) {
        if let Some(ref mut end) = self.selection.end {
            if *end != (row, col) {
                *end = (row, col);
                self.mark_all_dirty();
            }
        } else if self.selection.start.is_some() {
            self.selection.end = Some((row, col));
            self.mark_all_dirty();
        }
    }

    /// Clear the active selection (reset both anchor and end to None)
    pub(crate) fn clear_selection(&mut self) {
        if self.selection.start.is_some() || self.selection.end.is_some() {
            self.mark_all_dirty();
        }
        self.selection.clear();
    }

    /// Get the canvas ID
    pub(crate) fn canvas_id(&self) -> &str {
        &self.canvas_id
    }

    /// Get the current terminal dimensions
    pub(crate) fn size(&self) -> (u16, u16) {
        (self.rows, self.cols)
    }

    /// Access the parser's screen.
    pub(crate) fn parser_screen(&self) -> &vt100::Screen {
        self.parser.screen()
    }

    /// Whether the application enabled bracketed paste (DECSET 2004).
    pub(crate) fn bracketed_paste(&self) -> bool {
        self.parser.screen().bracketed_paste()
    }

    /// Whether the application enabled focus reporting (DECSET 1004).
    pub(crate) fn focus_reporting(&self) -> bool {
        self.focus.enabled()
    }

    /// Take the pending bell flag, clearing it. A bell rings for `BEL` (0x07)
    /// and the visual-bell escape; the page flashes when this returns true.
    pub(crate) fn take_bell(&mut self) -> bool {
        std::mem::take(&mut self.parser.callbacks_mut().bell_pending)
    }

    /// The `OSC 8` hyperlink URI covering `(row, col)`, if any.
    pub(crate) fn hyperlink_at(&self, row: u16, col: u16) -> Option<&str> {
        hyperlink_at(&self.parser.callbacks().links, row, col)
    }

    /// Take the pending `OSC 52` clipboard payload (base64), clearing it.
    pub(crate) fn take_clipboard(&mut self) -> Option<String> {
        self.parser.callbacks_mut().clipboard.take()
    }

    /// Take the last `OSC 7` working directory URI, clearing it.
    pub(crate) fn take_cwd(&mut self) -> Option<String> {
        self.parser.callbacks_mut().cwd.take()
    }

    /// Take the last `OSC 133` prompt mark, clearing it.
    pub(crate) fn take_prompt_mark(&mut self) -> Option<u8> {
        self.parser.callbacks_mut().prompt_mark.take()
    }

    /// Snapshot the modes krust can report through `DECRQM`.
    pub(crate) fn mode_report(&self) -> crate::query::ModeReport {
        let sc = self.parser.screen();
        crate::query::ModeReport {
            application_cursor: sc.application_cursor(),
            hide_cursor: sc.hide_cursor(),
            bracketed_paste: sc.bracketed_paste(),
            alternate_screen: sc.alternate_screen(),
            focus_reporting: self.focus.enabled(),
            mouse_tracking: !matches!(
                sc.mouse_protocol_mode(),
                vt100::MouseProtocolMode::None
            ),
        }
    }

    /// Access the selection start.
    pub(crate) fn selection_start(&self) -> Option<(u16, u16)> {
        self.selection.start
    }

    /// Access the selection end.
    pub(crate) fn selection_end(&self) -> Option<(u16, u16)> {
        self.selection.end
    }

    /// Access the selection mode.
    pub(crate) fn selection_mode(&self) -> SelectionMode {
        self.selection.mode
    }

    /// Mutable access to the WebGL renderer for resize handling.
    pub(crate) fn webgl_mut(&mut self) -> Option<&mut renderer::WebGL2Renderer> {
        self.webgl.as_mut()
    }

    /// Mutable access to the canvas.
    pub(crate) fn canvas_handle(&self) -> JsHandle {
        self.canvas
    }

    /// Whether the active renderer is the Canvas 2D path.
    pub(crate) fn ctx(&self) -> Option<JsHandle> {
        self.ctx
    }

    /// Short name of the renderer that actually ended up active: `"gl"` for
    /// the WebGL2 path, `"2d"` for the Canvas 2D fallback. Reported in the
    /// `init` config JSON so the page can show which mode won the fallback
    /// race (and which `?r=` override is really in effect).
    pub(crate) fn renderer_name(&self) -> &'static str {
        if self.webgl.is_some() {
            "gl"
        } else {
            "2d"
        }
    }

    /// Set measured cell dimensions (used on resize).
    pub(crate) fn set_cell_dims(&mut self, cw: f64, ch: f64) {
        self.cell_width = cw;
        self.cell_height = ch;
        self.snap_cell_dims();
    }

    /// Snap the cell dimensions to a whole device-pixel pitch.
    ///
    /// The page hit-tests against the CSS-pixel size while both renderers draw
    /// at the integer device-pixel pitch, so the CSS size has to be exactly
    /// `device_pitch / dpr` — otherwise the two drift apart and a click near
    /// the right edge of a wide terminal lands in the wrong column.
    fn snap_cell_dims(&mut self) {
        let dpr = ffi::window_dpr(ffi::window()).max(1.0);
        self.cell_width = device_pitch(self.cell_width, dpr) as f64 / dpr;
        self.cell_height = device_pitch(self.cell_height, dpr) as f64 / dpr;
    }

    /// Resize the grid to fit a canvas of `w` x `h` device pixels, dropping
    /// whole columns/rows until it fits and centering what is left.
    ///
    /// The count comes from the *device-pixel* cell pitch — the same number the
    /// renderers draw with — so the grid can never be laid out wider than the
    /// canvas. The leftover pixels are split evenly, giving a small equal margin
    /// instead of a ragged strip down one side.
    ///
    /// Also syncs the WebGL2 renderer's cell pitch and origin, which is the
    /// other half of the same fit.
    pub(crate) fn refit(&mut self, w_dev: i64, h_dev: i64) {
        let dpr = ffi::window_dpr(ffi::window()).max(1.0);
        let cell_w = device_pitch(self.cell_width, dpr);
        let cell_h = device_pitch(self.cell_height, dpr);
        let (cols, origin_x) = fit_grid(w_dev, cell_w);
        let (rows, origin_y) = fit_grid(h_dev, cell_h);
        self.cols = cols.clamp(2, u16::MAX as i64) as u16;
        self.rows = rows.clamp(1, u16::MAX as i64) as u16;
        self.origin_x = origin_x;
        self.origin_y = origin_y;
        let (rows, cols) = (self.rows, self.cols);
        if let Some(w) = self.webgl.as_mut() {
            w.cell_w = cell_w as u32;
            w.cell_h = cell_h as u32;
            w.origin_x = origin_x as u32;
            w.origin_y = origin_y as u32;
            w.rows = rows;
            w.cols = cols;
        }
    }

    /// Grid origin in CSS pixels, for the Canvas 2D render path.
    pub(crate) fn origin_css(&self, dpr: f64) -> (f64, f64) {
        let dpr = dpr.max(1.0);
        (self.origin_x as f64 / dpr, self.origin_y as f64 / dpr)
    }

    /// Map a canvas-relative CSS-pixel point onto a cell, clamped to the grid.
    ///
    /// The grid is centered in the canvas, so points in the surrounding margin
    /// have to be clamped to the nearest cell rather than running off the edge.
    pub(crate) fn cell_at(&self, x: f64, y: f64) -> (u16, u16) {
        let dpr = ffi::window_dpr(ffi::window()).max(1.0);
        let (ox, oy) = self.origin_css(dpr);
        let col = (((x - ox) * dpr).max(0.0) / device_pitch(self.cell_width, dpr).max(1) as f64)
            .floor()
            .min(self.cols.saturating_sub(1) as f64) as u16;
        let row = (((y - oy) * dpr).max(0.0) / device_pitch(self.cell_height, dpr).max(1) as f64)
            .floor()
            .min(self.rows.saturating_sub(1) as f64) as u16;
        (row, col)
    }

    /// Recreate the entire WebGL2 renderer (shader program, buffers and glyph
    /// atlas) from scratch on the currently bound context.
    ///
    /// Browsers may drop the WebGL context while a tab is hidden (Firefox does
    /// this under memory pressure). Every GL object handle becomes invalid, so
    /// `render()` on the old objects silently no-ops and the terminal freezes
    /// as a blank canvas — which is what krust showed whenever the user
    /// switched tabs away and back. After the browser fires
    /// `webglcontextrestored`, calling this rebuilds all objects and the next
    /// `render()` paints a full frame.
    pub(crate) fn rebuild_webgl(&mut self) -> Result<(), String> {
        let Some(_) = self.webgl.as_ref() else {
            return Ok(());
        };
        let dpr = ffi::window_dpr(ffi::window());
        let mut fresh = renderer::WebGL2Renderer::new(
            &self.canvas_id,
            self.cell_width,
            self.cell_height,
            self.rows,
            self.cols,
            dpr,
        )?;
        fresh.origin_x = self.origin_x as u32;
        fresh.origin_y = self.origin_y as u32;
        self.webgl = Some(fresh);
        self.mark_all_dirty();
        Ok(())
    }

    /// Resize the underlying parser screen to `rows` x `cols`.
    pub(crate) fn resize_screen(&mut self, rows: u16, cols: u16) {
        self.parser.screen_mut().set_size(rows, cols);
    }

    /// Throw away all parser state and start from a blank screen.
    ///
    /// Used when the server reports this client has fallen out of the
    /// retained output window: a contiguous byte tail can no longer bring the
    /// grid up to date, so the only correct move is to forget the old state
    /// and rebuild it from the log that follows. The renderer is kept —
    /// canvas, cell pitch and GL objects are all still valid — and the next
    /// `render` is a full redraw because `mark_all_dirty` forces one.
    pub(crate) fn reset(&mut self) {
        self.parser = Parser::new_with_callbacks(
            self.rows,
            self.cols,
            SCROLLBACK_LEN,
            TerminalCallbacks::default(),
        );
        self.scroll = ScrollState::new();
        self.selection = SelectionState::new();
        self.dirty.reset_baseline();
        self.prev_cursor = None;
        self.csi_su_carry.clear();
        self.dscsr_carry.clear();
        self.cursor_style = CursorStyle::default();
        self.blink_on = true;
        self.sync.reset();
        crate::color::reset_palette();
        self.mark_all_dirty();
    }

    /// Reset the diff baseline and force a full redraw on the next render.
    /// This is useful after the terminal has been hidden for a period, to
    /// ensure the dirty cell mechanism starts with a fresh state rather than
    /// comparing against stale state from before the hide.
    pub(crate) fn reset_prev_screen(&mut self) {
        self.dirty.reset_baseline();
    }

    /// Whether the WebGL context has been lost, which leaves every GL object
    /// krust holds invalid until the renderer is rebuilt.
    pub(crate) fn webgl_is_lost(&self) -> bool {
        self.webgl.as_ref().is_some_and(renderer::WebGL2Renderer::is_lost)
    }
}

/// The cells that differ between two screens, bounded to `rows` x `cols`.
///
/// A changed cell also flags its wide-character partner as dirty (in both the
/// current and previous screen), so a wide glyph is always redrawn as a unit
/// and a half-drawn partner can never be left behind. The result is sorted and
/// deduplicated. Pure so it can be driven by host `vt100::Parser` instances.
pub(crate) fn diff_screens(
    prev: &vt100::Screen,
    cur: &vt100::Screen,
    rows: u16,
    cols: u16,
) -> Vec<(u16, u16)> {
    let mut dirty = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            if !cell_changed(prev, cur, row, col) {
                continue;
            }
            dirty.push((row, col));
            // Wide character partner: redraw the other half too.
            if let Some(c) = cur.cell(row, col) {
                if c.is_wide() && col + 1 < cols {
                    dirty.push((row, col + 1));
                } else if c.is_wide_continuation() && col > 0 {
                    dirty.push((row, col - 1));
                }
            }
            // The previous screen's wide partner may also need a redraw if the
            // glyph changed or disappeared.
            if let Some(pc) = prev.cell(row, col) {
                if pc.is_wide() && col + 1 < cols {
                    dirty.push((row, col + 1));
                } else if pc.is_wide_continuation() && col > 0 {
                    dirty.push((row, col - 1));
                }
            }
        }
    }
    dirty.sort_unstable();
    dirty.dedup();
    dirty
}

/// Whether the cell at (`row`, `col`) differs between two screens.
fn cell_changed(a: &vt100::Screen, b: &vt100::Screen, row: u16, col: u16) -> bool {
    match (a.cell(row, col), b.cell(row, col)) {
        (Some(ca), Some(cb)) => {
            ca.contents() != cb.contents()
                || ca.fgcolor() != cb.fgcolor()
                || ca.bgcolor() != cb.bgcolor()
                || ca.bold() != cb.bold()
                || ca.dim() != cb.dim()
                || ca.italic() != cb.italic()
                || ca.underline() != cb.underline()
                || ca.inverse() != cb.inverse()
        }
        (Some(_), None) | (None, Some(_)) => true,
        (None, None) => false,
    }
}

/// The cursor cell to render, or `None` when the cursor must not be drawn:
/// scrolled into history, positioned outside the visible grid, or hidden by
/// the application's DECTCEM (`CSI ? 25 l`).
///
/// TUIs (opencode included) emit `?25l` …repaint… `?25h` around *every*
/// frame, and a frame often spans several `process_bytes` calls — so a
/// renderer that ignores the mode paints the block cursor at the end of each
/// half-painted frame, marching down the screen as chunks arrive.
pub(crate) fn cursor_draw_pos(
    screen: &vt100::Screen,
    scroll_offset: usize,
    rows: u16,
    cols: u16,
) -> Option<(u16, u16)> {
    if scroll_offset != 0 || screen.hide_cursor() {
        return None;
    }
    let (cr, cc) = screen.cursor_position();
    if cr < rows && cc < cols {
        Some((cr, cc))
    } else {
        None
    }
}

thread_local! {
    /// Global terminal state, initialized once by [`crate::init`]
    pub(crate) static TERM_STATE: RefCell<Option<TerminalState>> =
        const { RefCell::new(None) };
}

#[cfg(test)]
mod cursor_pos_tests {
    use super::cursor_draw_pos;
    use vt100::Parser;

    #[test]
    fn cursor_visible_by_default() {
        let p = Parser::new(6, 80, 0);
        assert_eq!(cursor_draw_pos(p.screen(), 0, 6, 80), Some((0, 0)));
    }

    #[test]
    fn dectcem_hide_wins_over_screen_position() {
        let mut p = Parser::new(6, 80, 0);
        p.process(b"\x1b[?25l");
        assert_eq!(cursor_draw_pos(p.screen(), 0, 6, 80), None);
        p.process(b"\x1b[?25h");
        assert_eq!(cursor_draw_pos(p.screen(), 0, 6, 80), Some((0, 0)));
    }

    /// The flicker regression: a TUI frame arrives chunk by chunk, and every
    /// chunk up to the closing `?25h` renders with the cursor hidden — not
    /// parked at the end of the half-painted diff.
    #[test]
    fn mid_frame_render_stays_hidden_until_show() {
        let mut p = Parser::new(6, 80, 0);
        p.process(b"\x1b[?25l\x1b[3;5Hpartial");
        assert_eq!(cursor_draw_pos(p.screen(), 0, 6, 80), None);
        p.process(b"\x1b[6;1Hdone\x1b[?25h");
        assert_eq!(cursor_draw_pos(p.screen(), 0, 6, 80), Some((5, 4)));
    }

    #[test]
    fn scrolled_into_history_hides_cursor() {
        let p = Parser::new(6, 80, 0);
        assert_eq!(cursor_draw_pos(p.screen(), 1, 6, 80), None);
    }
}

#[cfg(test)]
mod resync_tests {
    use vt100::Parser;

    /// The whole visible content of the screen, as text.
    ///
    /// A blank cell reports empty contents (vt100 stores `len: 0` rather than
    /// a space), so render those as spaces to keep columns meaningful.
    fn screen_text(p: &Parser) -> String {
        let (rows, cols) = p.screen().size();
        let mut out = String::new();
        for row in 0..rows {
            let mut line = String::new();
            for col in 0..cols {
                match p.screen().cell(row, col) {
                    Some(c) if !c.contents().is_empty() => line.push_str(c.contents()),
                    _ => line.push(' '),
                }
            }
            out.push_str(line.trim_end());
            out.push('\n');
        }
        out
    }

    /// Regression: a resumed stream that begins mid-escape-sequence makes the
    /// parser print the orphaned tail as literal text. This is what put
    /// `;255m`, `25h` and `6l156;245m` in the input field — the server
    /// replayed a history buffer trimmed at an arbitrary byte offset, so the
    /// tail of an SGR/DECSET sequence was parsed as ground text in cells the
    /// application never wrote.
    #[test]
    fn mid_sequence_resume_prints_orphaned_escape_text() {
        let mut p = Parser::new(6, 80, 0);
        p.process(b"\x1b[1;1Hfield");
        // Resumed in the middle of "\x1b[38;5;255m".
        p.process(b";255m");
        let text = screen_text(&p);
        assert!(
            text.contains(";255m"),
            "expected the orphaned tail to be painted:\n{}",
            text
        );
    }

    /// The fix: the server trims the retained log on an ESC boundary, so a
    /// fresh client replaying it never starts mid-sequence and never prints
    /// escape-sequence text into the grid.
    #[test]
    fn esc_boundary_resume_prints_no_escape_text() {
        let mut p = Parser::new(6, 80, 0);
        // A replay that begins on an ESC, as the boundary-aligned trim
        // guarantees, including the sequences the bug report showed up in.
        p.process(b"\x1b[?1006l\x1b[38;5;255m\x1b[?25htext");
        let text = screen_text(&p);
        assert!(
            !text.contains(";255m") && !text.contains("25h") && !text.contains("6l"),
            "no escape-sequence fragments may reach the grid:\n{}",
            text
        );
        assert!(text.contains("text"), "content should still render:\n{}", text);
    }

    /// A delta resync continues the client's own byte stream, so the parser
    /// resumes mid-sequence exactly where it left off and nothing is lost.
    #[test]
    fn delta_resync_is_invisible_to_the_parser() {
        let stream: &[u8] = b"\x1b[2J\x1b[1;1Halpha\x1b[38;2;156;245mbeta";

        // Whole stream in one go.
        let mut whole = Parser::new(6, 80, 0);
        whole.process(stream);
        let expected = screen_text(&whole);

        // Same stream, delivered as a prefix plus a delta tail, with the
        // split landing in the middle of the SGR parameter list.
        let split = stream.len() - 6;
        let mut split_parser = Parser::new(6, 80, 0);
        split_parser.process(&stream[..split]);
        split_parser.process(&stream[split..]);
        assert_eq!(
            screen_text(&split_parser),
            expected,
            "a delta resync must not change the resulting screen"
        );
    }

    /// A duplicated resync (the old drop-then-replay-everything behaviour)
    /// does change the result, which is why the tail is sent instead.
    ///
    /// The stream uses relative cursor motion, which is what a TUI emits
    /// between full repaints. Replaying it runs those relative moves twice, so
    /// the second copy of the glyph lands further along the row — and lands in
    /// cells the application's own model has no knowledge of, which is exactly
    /// the reported symptom (arrow keys skip them, typing overwrites them, and
    /// a copy still yields them).
    #[test]
    fn duplicated_replay_is_not_equivalent() {
        let stream: &[u8] = b"\x1b[3CX";
        let mut whole = Parser::new(6, 80, 0);
        whole.process(stream);
        let expected = screen_text(&whole);
        assert_eq!(expected, "   X\n\n\n\n\n\n");

        let mut replayed = Parser::new(6, 80, 0);
        replayed.process(stream);
        replayed.process(stream);
        assert_eq!(screen_text(&replayed), "   X   X\n\n\n\n\n\n");
        assert_ne!(screen_text(&replayed), expected);
    }
}

#[cfg(test)]
mod diff_screens_tests {
    use super::diff_screens;
    use vt100::Parser;

    #[test]
    fn unchanged_screen_yields_no_dirty_cells() {
        let mut a = Parser::new(4, 8, 0);
        let mut b = Parser::new(4, 8, 0);
        a.process(b"hello");
        b.process(b"hello");
        assert!(diff_screens(a.screen(), b.screen(), 4, 8).is_empty());
    }

    #[test]
    fn changed_attributes_flag_the_cell() {
        let mut a = Parser::new(4, 8, 0);
        let mut b = Parser::new(4, 8, 0);
        a.process(b"\x1b[1;1HA");
        b.process(b"\x1b[31m\x1b[1;1HA");
        let dirty = diff_screens(a.screen(), b.screen(), 4, 8);
        assert_eq!(dirty, vec![(0, 0)]);
    }

    #[test]
    fn changed_glyph_and_position_are_listed() {
        let mut a = Parser::new(4, 8, 0);
        let mut b = Parser::new(4, 8, 0);
        a.process(b"\x1b[1;1HAB");
        b.process(b"\x1b[1;1HAX");
        let dirty = diff_screens(a.screen(), b.screen(), 4, 8);
        assert_eq!(dirty, vec![(0, 1)]);
    }

    #[test]
    fn wide_char_change_flags_both_halves() {
        // A wide glyph newly written from a blank screen must list both the
        // wide cell and its continuation cell, so neither half is left blank.
        let a = Parser::new(4, 8, 0);
        let mut b = Parser::new(4, 8, 0);
        b.process("\u{4e2d}".as_bytes());
        let dirty = diff_screens(a.screen(), b.screen(), 4, 8);
        assert!(dirty.contains(&(0, 0)), "wide cell missing: {dirty:?}");
        assert!(
            dirty.contains(&(0, 1)),
            "wide continuation missing: {dirty:?}"
        );
    }

    #[test]
    fn scanning_is_bounded_to_the_requested_grid() {
        // The screen is 8 cols but we only ask about 2: a change in column 5
        // must not appear, so a render never paints outside the fitted grid.
        let a = Parser::new(4, 8, 0);
        let mut b = Parser::new(4, 8, 0);
        b.process(b"\x1b[1;6HX");
        let dirty = diff_screens(a.screen(), b.screen(), 4, 2);
        assert!(dirty.is_empty(), "out-of-grid change leaked: {dirty:?}");
    }

    #[test]
    fn identical_parsers_produce_empty_diff_for_all_dirty() {
        // Sanity: a full-screen change is just "every cell", which the caller
        // can bypass entirely via `full_redraw`.
        let mut a = Parser::new(2, 2, 0);
        let mut b = Parser::new(2, 2, 0);
        a.process(b"ab");
        b.process(b"cd");
        let dirty = diff_screens(a.screen(), b.screen(), 2, 2);
        assert_eq!(dirty, vec![(0, 0), (0, 1)]);
    }

    /// Mirrors `DirtyTracker::refresh`'s take/clone bookkeeping: the stored
    /// previous screen is diffed against the live one, and the retained
    /// snapshot is the live screen, so the next diff is empty.
    #[test]
    fn prev_screen_take_bookkeeping_diffs_against_the_live_screen() {
        let mut parser = Parser::new(4, 8, 0);
        parser.process(b"seed");
        let mut prev: Option<vt100::Screen> = Some(parser.screen().clone());

        parser.process(b"\x1b[2;1Hnext");
        let stored = prev.take().expect("previous screen present");
        let dirty = diff_screens(&stored, parser.screen(), 4, 8);
        prev = Some(parser.screen().clone());
        assert_eq!(dirty, vec![(1, 0), (1, 1), (1, 2), (1, 3)]);

        // No new output: diffing the retained snapshot against the live screen
        // is empty, i.e. the diff is idempotent between renders.
        let stored = prev.take().expect("previous screen present");
        assert!(diff_screens(&stored, parser.screen(), 4, 8).is_empty());
    }
}

#[cfg(test)]
mod dirty_tracker_tests {
    use super::DirtyTracker;
    use vt100::Parser;

    /// Model one render: refresh the diff, then clear the pending-full flag
    /// exactly as `render_canvas2d` does.
    fn settle(tracker: &mut DirtyTracker, screen: &vt100::Screen, rows: u16, cols: u16) {
        tracker.refresh(screen, rows, cols);
        tracker.full_redraw = false;
    }

    #[test]
    fn first_frame_forces_full_redraw() {
        let mut tracker = DirtyTracker::new();
        let parser = Parser::new(4, 8, 0);
        tracker.refresh(parser.screen(), 4, 8);
        assert!(tracker.full_redraw);
    }

    /// The WebGL branch of `TerminalState::render` returns before
    /// `render_canvas2d`, so it never calls `DirtyTracker::refresh`. Model that
    /// path: a parser advancing with no refresh leaves the dirty set empty, so
    /// GL rendering never pays for the Canvas 2D diff or its screen clone.
    #[test]
    fn gl_render_path_never_builds_a_canvas_dirty_set() {
        let mut tracker = DirtyTracker::new();
        let mut parser = Parser::new(4, 8, 0);
        settle(&mut tracker, parser.screen(), 4, 8);

        parser.process(b"gl output");
        assert!(
            tracker.dirty.is_empty(),
            "GL path built a Canvas dirty set: {:?}",
            tracker.dirty
        );
    }

    #[test]
    fn feeding_without_rendering_defers_the_diff() {
        let mut tracker = DirtyTracker::new();
        let mut parser = Parser::new(4, 8, 0);
        settle(&mut tracker, parser.screen(), 4, 8);

        // Several WebSocket frames arrive between two painted frames: the
        // parser advances but the tracker is untouched.
        parser.process(b"one ");
        parser.process(b"two ");
        assert!(
            tracker.dirty.is_empty(),
            "diff ran before render: {:?}",
            tracker.dirty
        );

        // One render computes one diff covering every change so far.
        tracker.refresh(parser.screen(), 4, 8);
        assert_eq!(
            tracker.dirty,
            (0..8).map(|c| (0, c)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn refresh_is_idempotent_between_renders() {
        let mut tracker = DirtyTracker::new();
        let mut parser = Parser::new(4, 8, 0);
        settle(&mut tracker, parser.screen(), 4, 8);

        parser.process(b"x");
        tracker.refresh(parser.screen(), 4, 8);
        tracker.full_redraw = false;
        assert!(!tracker.dirty.is_empty());

        // No new output: the next render diffs to nothing.
        tracker.refresh(parser.screen(), 4, 8);
        assert!(tracker.dirty.is_empty());
    }

    #[test]
    fn mark_all_dirty_forces_a_full_redraw_and_drops_the_diff() {
        let mut tracker = DirtyTracker::new();
        let mut parser = Parser::new(4, 8, 0);
        settle(&mut tracker, parser.screen(), 4, 8);

        parser.process(b"x");
        tracker.refresh(parser.screen(), 4, 8);
        tracker.full_redraw = false;

        tracker.mark_all_dirty();
        assert!(tracker.full_redraw);
        assert!(tracker.dirty.is_empty());
    }

    #[test]
    fn reset_baseline_forces_full_redraw_and_drops_the_baseline() {
        let mut tracker = DirtyTracker::new();
        let mut parser = Parser::new(4, 8, 0);
        settle(&mut tracker, parser.screen(), 4, 8);

        parser.process(b"x");
        tracker.refresh(parser.screen(), 4, 8);
        tracker.reset_baseline();
        assert!(tracker.full_redraw);
        assert!(tracker.prev_screen.is_none());
    }
}

#[cfg(test)]
mod bracketed_paste_tests {
    use super::bracketed_paste_bytes;

    #[test]
    fn wraps_paste_when_bracketed_paste_is_enabled() {
        let bytes = bracketed_paste_bytes("a\nb", true);
        assert_eq!(bytes, b"\x1b[200~a\nb\x1b[201~");
    }

    #[test]
    fn passes_paste_through_when_disabled() {
        assert_eq!(bracketed_paste_bytes("a\nb", false), b"a\nb");
    }

    #[test]
    fn empty_paste_is_wrapped_when_enabled() {
        assert_eq!(bracketed_paste_bytes("", true), b"\x1b[200~\x1b[201~");
        assert!(bracketed_paste_bytes("", false).is_empty());
    }
}

#[cfg(test)]
mod bell_tests {
    use super::{Parser, TerminalCallbacks};

    fn bell_parser() -> Parser<TerminalCallbacks> {
        Parser::new_with_callbacks(4, 8, 0, TerminalCallbacks::default())
    }

    #[test]
    fn bel_rings_and_taking_clears_it() {
        let mut p = bell_parser();
        assert!(!p.callbacks().bell_pending);
        p.process(b"\x07");
        assert!(p.callbacks().bell_pending);
        assert!(std::mem::take(&mut p.callbacks_mut().bell_pending));
        assert!(!p.callbacks().bell_pending);
    }

    #[test]
    fn visual_bell_escape_rings_too() {
        let mut p = bell_parser();
        p.process(b"hello\x1bg");
        assert!(p.callbacks().bell_pending);
    }

    #[test]
    fn bel_inside_osc_terminated_string_does_not_ring() {
        // OSC 2 ... BEL sets the title; the BEL terminates the string rather
        // than ringing, and vt100 does not invoke the bell callback for it.
        let mut p = bell_parser();
        p.process(b"\x1b]2;title\x07");
        assert!(!p.callbacks().bell_pending);
    }
}

#[cfg(test)]
mod link_tests {
    use super::{hyperlink_at, link_contains, LinkSpan, Parser, TerminalCallbacks};

    fn link_parser() -> Parser<TerminalCallbacks> {
        Parser::new_with_callbacks(6, 20, 0, TerminalCallbacks::default())
    }

    #[test]
    fn osc8_open_records_link_over_following_cells() {
        let mut p = link_parser();
        p.process(b"\x1b]8;;http://example.com\x1b\\AB\x1b]8;;\x1b\\");
        let links = &p.callbacks().links;
        assert_eq!(links.len(), 1);
        assert_eq!(links[0].uri, "http://example.com");
        assert_eq!(links[0].start, (0, 0));
        assert_eq!(links[0].end, (0, 2));
        assert_eq!(hyperlink_at(links, 0, 0), Some("http://example.com"));
        assert_eq!(hyperlink_at(links, 0, 1), Some("http://example.com"));
        assert_eq!(hyperlink_at(links, 0, 2), None);
    }

    #[test]
    fn osc8_close_without_open_records_nothing() {
        let mut p = link_parser();
        p.process(b"\x1b]8;;\x1b\\");
        assert!(p.callbacks().links.is_empty());
    }

    #[test]
    fn osc8_double_open_closes_previous() {
        let mut p = link_parser();
        p.process(b"\x1b]8;;http://a\x1b\\X\x1b]8;;http://b\x1b\\Y\x1b]8;;\x1b\\");
        let links = &p.callbacks().links;
        assert_eq!(links.len(), 2);
        assert_eq!(links[0].uri, "http://a");
        assert_eq!(links[1].uri, "http://b");
    }

    #[test]
    fn link_contains_handles_wrapped_rows() {
        let span = LinkSpan {
            uri: "u".into(),
            start: (1, 5),
            end: (3, 2),
        };
        assert!(!link_contains(&span, 0, 5));
        assert!(link_contains(&span, 1, 5));
        assert!(!link_contains(&span, 1, 4));
        assert!(link_contains(&span, 2, 0));
        assert!(link_contains(&span, 2, 19));
        assert!(link_contains(&span, 3, 1));
        assert!(!link_contains(&span, 3, 2));
        assert!(!link_contains(&span, 4, 0));
    }
}

#[cfg(test)]
mod clipboard_tests {
    use super::{Parser, TerminalCallbacks};

    fn clip_parser() -> Parser<TerminalCallbacks> {
        Parser::new_with_callbacks(4, 20, 0, TerminalCallbacks::default())
    }

    #[test]
    fn osc52_copy_is_captured_as_base64() {
        let mut p = clip_parser();
        p.process(b"\x1b]52;c;aGVsbG8=\x1b\\");
        assert_eq!(p.callbacks().clipboard.as_deref(), Some("aGVsbG8="));
    }

    #[test]
    fn osc52_paste_request_sets_no_clipboard() {
        let mut p = clip_parser();
        p.process(b"\x1b]52;c;?\x1b\\");
        assert_eq!(p.callbacks().clipboard, None);
    }
}

#[cfg(test)]
mod dynamic_color_tests {
    use super::{Parser, TerminalCallbacks};

    #[test]
    fn osc4_through_the_parser_updates_the_palette() {
        crate::color::reset_palette();
        let mut p = Parser::new_with_callbacks(2, 8, 0, TerminalCallbacks::default());
        p.process(b"\x1b]4;2;#abcdef\x1b\\");
        assert_eq!(crate::color::resolved_indexed(2), 0xabcdef);
        crate::color::reset_palette();
        assert_eq!(crate::color::resolved_indexed(2), 0x00CD00);
    }
}

#[cfg(test)]
mod shell_integration_tests {
    use super::{Parser, TerminalCallbacks};

    fn parser() -> Parser<TerminalCallbacks> {
        Parser::new_with_callbacks(4, 20, 0, TerminalCallbacks::default())
    }

    #[test]
    fn osc7_records_the_working_directory_uri() {
        let mut p = parser();
        p.process(b"\x1b]7;file://host/home/user\x1b\\");
        assert_eq!(
            p.callbacks().cwd.as_deref(),
            Some("file://host/home/user")
        );
    }

    #[test]
    fn osc133_records_prompt_marks() {
        let mut p = parser();
        p.process(b"\x1b]133;A\x1b\\");
        assert_eq!(p.callbacks().prompt_mark, Some(b'A'));
        p.process(b"\x1b]133;D;0\x1b\\");
        assert_eq!(p.callbacks().prompt_mark, Some(b'D'));
    }
}
