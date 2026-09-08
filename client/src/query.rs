// Client device-query reply detection.

/// Scan a chunk of terminal writing for device-query sequences that demand a
/// response (DA1, DA2, cursor position, OSC-11 background colour). Responding
/// keeps shells like fish from stalling on unanswered queries (`\x1b[c` etc).
///
/// Pure + unit-tested; `row`/`col` are the 1-based cursor position to report
/// for a `\x1b[6n` query.
pub(crate) fn collect_query_replies(bytes: &[u8], row: usize, col: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            i += 1;
            continue;
        }
        // OSC 11 (and any other OSC ) query: ESC ] 11 ; ? ESC \
        if i + 2 < bytes.len() && bytes[i + 1] == b']' {
            let mut k = i + 3;
            while k + 1 < bytes.len() {
                if bytes[k] == 0x07 || (bytes[k] == 0x1b && bytes[k + 1] == b'\\') {
                    break;
                }
                k += 1;
            }
            if k + 1 < bytes.len() && bytes[i..k].starts_with(b"\x1b]11;?") {
                out.extend_from_slice(b"\x1b]11;rgb:2b2b/2b2b/2b2b\x1b\\");
            }
            i = k + 1;
            continue;
        }
        // CSI sequences
        if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut k = i + 2;
            let mut has_greater = false;
            while k < bytes.len()
                && (bytes[k].is_ascii_digit() || matches!(bytes[k], b';' | b'>' | b'?'))
            {
                if bytes[k] == b'>' {
                    has_greater = true;
                }
                k += 1;
            }
            if k >= bytes.len() {
                i += 1;
                continue;
            }
            let params = &bytes[i + 2..k];
            match bytes[k] {
                b'c' if params.is_empty() || params == b"0" => {
                    out.extend_from_slice(b"\x1b[?1;2c");
                }
                b'c' if has_greater => {
                    out.extend_from_slice(b"\x1b[>0;1;0c");
                }
                b'n' if params == b"6" => {
                    out.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                _ => {}
            }
            i = k + 1;
            continue;
        }
        i += 1;
    }
    out
}
