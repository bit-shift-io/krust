// Cursor styles (DECSCUSR) and synchronized output (?2026) gating.
//
// `vt100` implements neither feature:
//
//   * It parses `CSI Ps SP q` (DECSCUSR) as an unknown final and discards it,
//     so the renderer would never learn that the application asked for an
//     underline or bar cursor.
//   * It applies `CSI ? 2026 h/l` (synchronized output) as a no-op private
//     mode, so every write lands on the screen immediately. A TUI frame that
//     spans several `process_bytes` calls — which is the normal case, since
//     the server broadcasts the PTY in 1024-byte reads — would therefore be
//     rendered half-painted, with the cursor sitting wherever the partial
//     diff happened to stop.
//
// This module owns both gaps:
//
//   * [`SyncGate`] holds the bytes of a synchronized frame until its end
//     marker arrives, so the parser (and the render scheduled after it) only
//     ever sees complete frames. The gate is bounded: an unterminated frame
//     is force-fed after [`MAX_SYNC_BUFFER`] bytes, and the page arms a 300 ms
//     stall timer after every chunk that calls `flush_sync`, so a dying app
//     can never freeze the screen.
//   * [`apply_decscusr`] scans for DECSCUSR and records the request in a
//     [`CursorStyle`], carrying partial sequences across chunk boundaries.

use std::borrow::Cow;

/// DECSET/DECREST for synchronized output: everything between these two
/// markers is one atomic frame.
pub(crate) const SYNC_START: &[u8] = b"\x1b[?2026h";
pub(crate) const SYNC_END: &[u8] = b"\x1b[?2026l";

/// Hard cap on bytes held by the sync gate. Real frames are a few KB
/// (opencode repaints ~3 KB); anything past this means the end marker was
/// lost, and stalling the terminal forever is worse than painting an
/// incomplete frame.
pub(crate) const MAX_SYNC_BUFFER: usize = 256 * 1024;

/// How the cursor is drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum CursorShape {
    /// Full-cell cursor with fg/bg swapped — the DECSCUSR 1/2 block and the
    /// DECSCUSR 7 reverse-video cursor (which renders identically here,
    /// since krust's block *is* a fg/bg swap), and the pre-style default.
    Block,
    /// Bottom strip — DECSCUSR 3/4.
    Underline,
    /// Left-edge strip — DECSCUSR 5/6.
    Bar,
}

/// The cursor style requested by the application via DECSCUSR.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) struct CursorStyle {
    pub(crate) shape: CursorShape,
    pub(crate) blink: bool,
}

impl Default for CursorStyle {
    fn default() -> Self {
        Self {
            shape: CursorShape::Block,
            blink: false,
        }
    }
}

impl CursorStyle {
    /// Map a DECSCUSR parameter onto a style. Out-of-range values and `0`
    /// (restore default) reset to krust's default: steady block.
    pub(crate) fn from_decscusr(ps: u32) -> Self {
        match ps {
            1 => Self {
                shape: CursorShape::Block,
                blink: true,
            },
            2 => Self {
                shape: CursorShape::Block,
                blink: false,
            },
            3 => Self {
                shape: CursorShape::Underline,
                blink: true,
            },
            4 => Self {
                shape: CursorShape::Underline,
                blink: false,
            },
            5 => Self {
                shape: CursorShape::Bar,
                blink: true,
            },
            6 => Self {
                shape: CursorShape::Bar,
                blink: false,
            },
            7 => Self {
                shape: CursorShape::Block,
                blink: true,
            },
            _ => Self::default(),
        }
    }

    /// Whether the cursor should be painted in the given blink phase.
    pub(crate) fn blink_visible(&self, phase_on: bool) -> bool {
        !self.blink || phase_on
    }

    /// Whether the style draws as the full-cell (color-swapped) cursor.
    pub(crate) fn is_block(&self) -> bool {
        self.shape == CursorShape::Block
    }
}

/// The strip rect for `shape` inside the cell box `(x, y, w, h)`, in the same
/// units as the inputs, or `None` for the full-cell block cursor. Shared by
/// both renderers so an underline/bar cursor looks the same in GL and 2D.
pub(crate) fn strip_rect(
    shape: CursorShape,
    x: f64,
    y: f64,
    w: f64,
    h: f64,
) -> Option<(f64, f64, f64, f64)> {
    let thickness = |v: f64| (v / 8.0).max(1.0);
    match shape {
        CursorShape::Block => None,
        CursorShape::Underline => {
            let th = thickness(h);
            Some((x, y + h - th, w, th))
        }
        CursorShape::Bar => {
            let tw = thickness(w);
            Some((x, y, tw, h))
        }
    }
}

/// Scan `bytes` for complete `CSI Ps SP q` sequences and update `style`.
///
/// Sequences that may straddle a chunk boundary are carried in `carry`; the
/// returned slice is what may be handed to the parser now (it reborrows
/// `bytes` on the common path, so an unsplit stream costs no allocation).
pub(crate) fn apply_decscusr<'a>(
    bytes: &'a [u8],
    carry: &mut Vec<u8>,
    style: &mut CursorStyle,
) -> Cow<'a, [u8]> {
    if carry.is_empty() {
        let hold = partial_decscusr_suffix(bytes);
        if hold == 0 {
            scan_decscusr(bytes, style);
            return Cow::Borrowed(bytes);
        }
        let cut = bytes.len() - hold;
        scan_decscusr(&bytes[..cut], style);
        carry.extend_from_slice(&bytes[cut..]);
        return Cow::Owned(bytes[..cut].to_vec());
    }

    let mut feed = std::mem::take(carry);
    feed.extend_from_slice(bytes);
    let hold = partial_decscusr_suffix(&feed);
    let cut = feed.len() - hold;
    scan_decscusr(&feed[..cut], style);
    if hold > 0 {
        carry.extend_from_slice(&feed[cut..]);
    }
    Cow::Owned(feed[..cut].to_vec())
}

/// Length of the trailing run that is a proper prefix of `CSI Ps SP q`
/// (i.e. could still become one), else 0.
fn partial_decscusr_suffix(feed: &[u8]) -> usize {
    let Some(last) = feed.iter().rposition(|&b| b == 0x1b) else {
        return 0;
    };
    let tail = &feed[last..];
    if tail.len() >= 6 {
        // Longest possible hold is "ESC [ P P P " — anything longer than a
        // plausible `Ps SP` tail can no longer become a DECSCUSR.
        return 0;
    }
    if tail.len() == 1 {
        return 1; // "ESC"
    }
    if tail[1] != b'[' {
        return 0;
    }
    // "ESC [" alone, or "ESC [" digits, or "ESC [" digits " ".
    let rest = &tail[2..];
    let digits_end = rest
        .iter()
        .position(|&b| !b.is_ascii_digit())
        .unwrap_or(rest.len());
    if digits_end == rest.len() {
        return tail.len();
    }
    if digits_end + 1 == rest.len() && rest[digits_end] == b' ' {
        return tail.len();
    }
    0
}

fn scan_decscusr(bytes: &[u8], style: &mut CursorStyle) {
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' {
            let mut k = i + 2;
            while k < bytes.len() && bytes[k].is_ascii_digit() {
                k += 1;
            }
            if k + 1 < bytes.len() && bytes[k] == b' ' && bytes[k + 1] == b'q' {
                let ps: u32 = std::str::from_utf8(&bytes[i + 2..k])
                    .ok()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                *style = CursorStyle::from_decscusr(ps);
                i = k + 2;
                continue;
            }
        }
        i += 1;
    }
}

/// Holds the body of a synchronized-output frame until its end marker shows
/// up, so the parser only ever receives whole frames.
pub(crate) struct SyncGate {
    /// Bytes withheld: a partial start marker while idle, or everything
    /// from an unmatched `SYNC_START` onward while a frame is open.
    buf: Vec<u8>,
    /// True between a consumed `SYNC_START` and `SYNC_END`.
    in_sync: bool,
}

impl SyncGate {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            in_sync: false,
        }
    }

    /// Whether bytes are currently being withheld.
    pub(crate) fn pending(&self) -> bool {
        !self.buf.is_empty()
    }

    /// Feed `bytes` through the gate, returning the bytes the parser may
    /// consume now. The common case (no frame open, no marker in sight)
    /// borrows the input directly.
    pub(crate) fn push<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if !self.in_sync && self.buf.is_empty() {
            let has_marker = find(bytes, SYNC_START).is_some();
            let hold = partial_marker_suffix(bytes, SYNC_START);
            if !has_marker && hold == 0 {
                return Cow::Borrowed(bytes);
            }
        }

        let mut feed = std::mem::take(&mut self.buf);
        feed.extend_from_slice(bytes);
        let mut out: Vec<u8> = Vec::with_capacity(feed.len());
        loop {
            if !self.in_sync {
                match find(&feed, SYNC_START) {
                    Some(p) => {
                        // Everything before the marker is a finished frame
                        // tail; the marker and beyond belong to the new one.
                        out.extend_from_slice(&feed[..p]);
                        feed.drain(..p);
                        self.in_sync = true;
                        continue;
                    }
                    None => {
                        let hold = partial_marker_suffix(&feed, SYNC_START);
                        let cut = feed.len() - hold;
                        out.extend_from_slice(&feed[..cut]);
                        self.buf = feed[cut..].to_vec();
                        return Cow::Owned(out);
                    }
                }
            }

            // Frame open: emit whole frames, keep incomplete ones buffered.
            if let Some(q) = find(&feed, SYNC_END) {
                let end = q + SYNC_END.len();
                out.extend_from_slice(&feed[..end]);
                feed.drain(..end);
                self.in_sync = false;
                continue;
            }
            // A second start marker without an end for the first: the app
            // dropped a frame. Close the old one at the boundary rather
            // than buffering both forever.
            if let Some(p) = find(&feed[1..], SYNC_START).map(|p| p + 1) {
                out.extend_from_slice(&feed[..p]);
                feed.drain(..p);
                continue;
            }
            if feed.len() > MAX_SYNC_BUFFER {
                out.extend_from_slice(&feed);
                feed.clear();
                self.in_sync = false;
                return Cow::Owned(out);
            }
            self.buf = feed;
            return Cow::Owned(out);
        }
    }

    /// Take every withheld byte (ending the open frame, if any) so the
    /// caller can feed it — used by the page's stall timer and on reset.
    pub(crate) fn flush(&mut self) -> Vec<u8> {
        self.in_sync = false;
        std::mem::take(&mut self.buf)
    }

    pub(crate) fn reset(&mut self) {
        let _ = self.flush();
    }
}

/// Length of the trailing run that is a proper prefix of `marker`, else 0.
fn partial_marker_suffix(s: &[u8], marker: &[u8]) -> usize {
    let Some(last) = s.iter().rposition(|&b| b == 0x1b) else {
        return 0;
    };
    let tail = &s[last..];
    if !tail.is_empty() && tail.len() < marker.len() && marker.starts_with(tail) {
        tail.len()
    } else {
        0
    }
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn style_of(chunks: &[&[u8]]) -> (CursorStyle, Vec<u8>) {
        let mut carry = Vec::new();
        let mut style = CursorStyle::default();
        let mut fed = Vec::new();
        for c in chunks {
            fed.extend_from_slice(&apply_decscusr(c, &mut carry, &mut style));
        }
        (style, fed)
    }

    #[test]
    fn decscusr_updates_style() {
        let (style, fed) = style_of(&[b"\x1b[4 q"]);
        assert_eq!(style, CursorStyle::from_decscusr(4));
        assert_eq!(fed, b"\x1b[4 q"); // sequence passes through to vt100 too
    }

    #[test]
    fn decscusr_split_across_chunks() {
        let (style, fed) = style_of(&[b"\x1b[3", b" qtext"]);
        assert_eq!(style.shape, CursorShape::Underline);
        assert!(style.blink);
        assert_eq!(fed, b"\x1b[3 qtext");
    }

    #[test]
    fn decscusr_ignores_xtversion_query() {
        // `CSI > 0 q` is XTVERSION, not a cursor style.
        let (style, _) = style_of(&[b"\x1b[>0q"]);
        assert_eq!(style, CursorStyle::default());
    }

    #[test]
    fn decscusr_out_of_range_restores_default() {
        let mut style = CursorStyle::from_decscusr(4);
        let mut carry = Vec::new();
        apply_decscusr(b"\x1b[0 q", &mut carry, &mut style);
        assert_eq!(style, CursorStyle::default());
        apply_decscusr(b"\x1b[42 q", &mut carry, &mut style);
        assert_eq!(style, CursorStyle::default());
    }

    #[test]
    fn decscusr_boundary_tail_not_swallowed() {
        // A tail that can no longer become DECSCUSR is fed immediately.
        let mut carry = Vec::new();
        let mut style = CursorStyle::default();
        let out = apply_decscusr(b"\x1b[4;2m", &mut carry, &mut style);
        assert_eq!(&*out, b"\x1b[4;2m");
        assert!(carry.is_empty());
    }

    #[test]
    fn sync_passes_normal_output_through_borrowed() {
        let mut gate = SyncGate::new();
        let out = gate.push(b"\x1b[2Jhello\x1b[38;5;1m");
        assert!(matches!(out, Cow::Borrowed(_)));
        assert!(!gate.pending());
    }

    #[test]
    fn sync_complete_frame_in_one_push_is_not_delayed() {
        let mut gate = SyncGate::new();
        let input = [b"A".as_slice(), SYNC_START, b"BODY", SYNC_END, b"C"].concat();
        let out = gate.push(&input).into_owned();
        assert_eq!(out, input);
        assert!(!gate.pending());
    }

    #[test]
    fn sync_frame_defers_until_end_marker() {
        let mut gate = SyncGate::new();
        let input = [SYNC_START, b"half"].concat();
        let first = gate.push(&input);
        assert!(first.is_empty(), "frame body must be withheld: {:?}", first);
        assert!(gate.pending());
        let input = [b"rest".as_slice(), SYNC_END, b"tail"].concat();
        let second = gate.push(&input);
        assert_eq!(
            second,
            [SYNC_START, b"halfrest", SYNC_END, b"tail"].concat(),
            "frame must arrive whole, followed by the trailing output"
        );
        assert!(!gate.pending());
    }

    #[test]
    fn sync_partial_start_marker_held_across_chunks() {
        let mut gate = SyncGate::new();
        let out = gate.push(b"AB\x1b[?2026");
        assert_eq!(&*out, b"AB");
        assert!(gate.pending());
        let out = gate.push(b"hXY");
        assert!(out.is_empty(), "marker+body belongs to the open frame");
        assert!(gate.pending());
        let out = gate.push(SYNC_END);
        assert_eq!(out, [SYNC_START, b"XY", SYNC_END].concat());
        assert!(!gate.pending());
    }

    #[test]
    fn sync_unclosed_frame_recovers_on_second_start() {
        let mut gate = SyncGate::new();
        let input = [SYNC_START, b"dropped"].concat();
        let _ = gate.push(&input);
        let input = [SYNC_START, b"new", SYNC_END].concat();
        let out = gate.push(&input);
        assert_eq!(
            out,
            [SYNC_START, b"dropped", SYNC_START, b"new", SYNC_END].concat()
        );
        assert!(!gate.pending());
    }

    #[test]
    fn sync_flush_releases_held_bytes() {
        let mut gate = SyncGate::new();
        let _ = gate.push(b"\x1b[?2026"); // partial marker only
        assert!(gate.pending());
        let held = gate.flush();
        assert_eq!(held, b"\x1b[?2026");
        assert!(!gate.pending());
    }

    #[test]
    fn sync_cap_force_feeds_oversized_frame() {
        let mut gate = SyncGate::new();
        let mut input = SYNC_START.to_vec();
        input.resize(SYNC_START.len() + MAX_SYNC_BUFFER + 1, b'X');
        let out = gate.push(&input);
        assert_eq!(out.len(), input.len());
        assert!(!gate.pending());
    }

    #[test]
    fn strip_rect_shapes() {
        assert_eq!(strip_rect(CursorShape::Block, 0.0, 0.0, 8.0, 16.0), None);
        let (x, y, w, h) = strip_rect(CursorShape::Underline, 10.0, 0.0, 8.0, 16.0).unwrap();
        assert_eq!((x, w), (10.0, 8.0));
        assert_eq!(h, 2.0);
        assert_eq!(y, 14.0);
        let (x, y, w, h) = strip_rect(CursorShape::Bar, 10.0, 4.0, 8.0, 16.0).unwrap();
        assert_eq!((x, y, h), (10.0, 4.0, 16.0));
        assert_eq!(w, 1.0);
    }
}
