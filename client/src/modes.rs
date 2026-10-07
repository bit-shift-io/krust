// Stream-scan helpers for terminal private modes that the `vt100` crate does
// not model.
//
// `vt100` silently drops DECSET/DECRST private modes it does not know (e.g.
// focus reporting, 1004), so krust tracks the ones it cares about out of band
// by scanning the byte stream as it is fed to the parser. Sequences that may
// straddle a chunk boundary are carried across calls, exactly like the
// DECSCUSR scanner in `cursor.rs`.

use std::borrow::Cow;

/// DECSET 1004 focus reporting.
#[derive(Default)]
pub(crate) struct FocusReporting {
    enabled: bool,
    /// Trailing bytes of a `CSI ? ... h|l` sequence that may still be completed
    /// by the next chunk.
    carry: Vec<u8>,
}

impl FocusReporting {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Whether the application requested focus in/out reports.
    pub(crate) fn enabled(&self) -> bool {
        self.enabled
    }

    /// Scan `bytes` for `CSI ? ... 1004 h|l`, updating the tracked mode, and
    /// return the slice the parser may consume now (a partial sequence at the
    /// tail is withheld in `carry`).
    pub(crate) fn scan<'a>(&mut self, bytes: &'a [u8]) -> Cow<'a, [u8]> {
        if self.carry.is_empty() {
            let hold = partial_private_mode_suffix(bytes);
            if hold == 0 {
                scan_1004(bytes, &mut self.enabled);
                return Cow::Borrowed(bytes);
            }
            let cut = bytes.len() - hold;
            scan_1004(&bytes[..cut], &mut self.enabled);
            self.carry.extend_from_slice(&bytes[cut..]);
            return Cow::Owned(bytes[..cut].to_vec());
        }

        let mut feed = std::mem::take(&mut self.carry);
        feed.extend_from_slice(bytes);
        let hold = partial_private_mode_suffix(&feed);
        let cut = feed.len() - hold;
        scan_1004(&feed[..cut], &mut self.enabled);
        if hold > 0 {
            self.carry.extend_from_slice(&feed[cut..]);
        }
        Cow::Owned(feed[..cut].to_vec())
    }
}

/// Length of the trailing run that is a proper prefix of `CSI ? <params> h|l`
/// (i.e. could still become one), else 0.
fn partial_private_mode_suffix(feed: &[u8]) -> usize {
    let Some(last) = feed.iter().rposition(|&b| b == 0x1b) else {
        return 0;
    };
    let tail = &feed[last..];
    // "ESC [ ? " plus a handful of digits/semicolons; anything longer than this
    // can no longer be the prefix of a mode we track.
    if tail.len() > 24 {
        return 0;
    }
    if tail.len() == 1 {
        return 1; // "ESC"
    }
    if tail[1] != b'[' {
        return 0;
    }
    if tail.len() == 2 {
        return 2; // "ESC ["
    }
    if tail[2] != b'?' {
        return 0;
    }
    if tail.len() == 3 {
        return 3; // "ESC [ ?"
    }
    // Digits and ';' only means the final `h`/`l` has not arrived yet.
    if tail[3..]
        .iter()
        .all(|b| b.is_ascii_digit() || *b == b';')
    {
        return tail.len();
    }
    0
}

/// Update `enabled` for every complete `CSI ? ... 1004 h|l` in `bytes` that
/// has already been fully received.
fn scan_1004(bytes: &[u8], enabled: &mut bool) {
    let mut i = 0;
    while i + 3 < bytes.len() {
        if bytes[i] == 0x1b && bytes[i + 1] == b'[' && bytes[i + 2] == b'?' {
            let mut k = i + 3;
            let mut num: u32 = 0;
            let mut in_num = false;
            let mut has_1004 = false;
            while k < bytes.len() {
                let b = bytes[k];
                if b.is_ascii_digit() {
                    num = num.saturating_mul(10).saturating_add((b - b'0') as u32);
                    in_num = true;
                    k += 1;
                } else if b == b';' {
                    has_1004 |= in_num && num == 1004;
                    num = 0;
                    in_num = false;
                    k += 1;
                } else {
                    break;
                }
            }
            has_1004 |= in_num && num == 1004;
            if k < bytes.len() && (bytes[k] == b'h' || bytes[k] == b'l') {
                if has_1004 {
                    *enabled = bytes[k] == b'h';
                }
                i = k + 1;
                continue;
            }
        }
        i += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::FocusReporting;

    fn scan_chunks(chunks: &[&[u8]]) -> (bool, Vec<u8>) {
        let mut focus = FocusReporting::new();
        let mut fed = Vec::new();
        for chunk in chunks {
            fed.extend_from_slice(&focus.scan(chunk));
        }
        (focus.enabled(), fed)
    }

    #[test]
    fn decset_enables_and_decreset_disables() {
        let mut focus = FocusReporting::new();
        focus.scan(b"\x1b[?1004h");
        assert!(focus.enabled());
        focus.scan(b"\x1b[?1004l");
        assert!(!focus.enabled());
    }

    #[test]
    fn detects_1004_inside_a_parameter_list() {
        let (enabled, _) = scan_chunks(&[b"\x1b[?1003;1004;1006h"]);
        assert!(enabled);
    }

    #[test]
    fn ignores_other_private_modes() {
        let (enabled, fed) = scan_chunks(&[b"\x1b[?25l\x1b[?2004h"]);
        assert!(!enabled);
        assert_eq!(fed, b"\x1b[?25l\x1b[?2004h");
    }

    #[test]
    fn sequence_split_across_chunks_is_carried() {
        let (enabled, fed) = scan_chunks(&[b"\x1b[?10", b"04h"]);
        assert!(enabled, "split DECSET 1004 was not detected");
        assert_eq!(fed, b"\x1b[?1004h", "carried bytes must be re-emitted");
    }

    #[test]
    fn partial_escape_is_held_then_released() {
        // A lone ESC that never becomes a mode sequence must still reach the
        // parser once the next chunk disambiguates it.
        let mut focus = FocusReporting::new();
        assert!(focus.scan(b"\x1b").is_empty());
        let rest = focus.scan(b"[A");
        assert_eq!(&*rest, b"\x1b[A");
    }
}
