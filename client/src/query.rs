// Client device-query reply detection.

/// The subset of terminal modes krust can report through `DECRQM`.
#[derive(Clone, Copy, Default)]
pub(crate) struct ModeReport {
    pub(crate) application_cursor: bool,
    pub(crate) hide_cursor: bool,
    pub(crate) bracketed_paste: bool,
    pub(crate) alternate_screen: bool,
    pub(crate) focus_reporting: bool,
    pub(crate) mouse_tracking: bool,
}

/// Terminal name reported for XTVERSION and the `TN` capability.
const TERM_NAME: &str = "krust";
const TERM_VERSION: &str = "0.1.0";

/// Hex-encoded XTGETTCAP names we can answer with a real value. Anything else
/// still gets an (empty) reply so a blocking requester moves on.
fn xtgettcaps_value(name: &str) -> &'static str {
    match name {
        "544e" => TERM_NAME,   // TN  termname
        "436f" => "256",       // Co  number of colours
        "6d78" => "1000",      // mx  maximum right keypad
        "6d79" => "1000",      // my  maximum top keypad
        "4d4d" => "\x1b",      // KM  key modifier escape
        "524742" => "8/8/8",   // RGB direct colour model
        "5463" => "rgb",       // Tc  true colour
        "6b6d" => "\x1b[>4;m", // km  modifyOtherKeys
        _ => "",
    }
}

/// Decode a hex XTGETTCAP name into its ASCII form. Returns `None` for
/// anything that is not an even-length run of hex digits.
fn hex_name_to_str(hex: &str) -> Option<String> {
    if hex.is_empty() || hex.len() % 2 != 0 {
        return None;
    }
    let bytes = hex.as_bytes();
    let mut out = String::with_capacity(hex.len() / 2);
    for pair in bytes.chunks(2) {
        let hi = (pair[0] as char).to_digit(16)?;
        let lo = (pair[1] as char).to_digit(16)?;
        out.push(((hi << 4) | lo) as u8 as char);
    }
    Some(out)
}

/// Build the `DCS 0 + r name=value;... ST` reply for an XTGETTCAP request.
fn xtgettcaps_reply(payload: &[u8]) -> Option<Vec<u8>> {
    // The request is `DCS + q <hex names> ST`; a leading `>` would make it a
    // different (unsupported) request.
    let text = std::str::from_utf8(payload).ok()?;
    let hex_names: Vec<&str> = text.split_whitespace().collect();
    if hex_names.is_empty() {
        return None;
    }

    let mut out = b"\x1bP0+r".to_vec();
    for (idx, hex) in hex_names.iter().enumerate() {
        // Validate the name is really hex before echoing it back; a malformed
        // request gets no reply rather than a garbled one.
        hex_name_to_str(hex)?;
        if idx > 0 {
            out.push(b';');
        }
        out.extend_from_slice(hex.as_bytes());
        out.push(b'=');
        out.extend_from_slice(xtgettcaps_value(hex).as_bytes());
    }
    out.extend_from_slice(b"\x1b\\");
    Some(out)
}

/// Build a `DECRQM` reply for mode `Ps`: `CSI [private] Ps ; Pm $ y`.
///
/// `Pm` is 0 = not recognized, 1 = set, 2 = reset, matching what xterm-style
/// clients expect. Modes krust does not model answer 0 rather than guessing.
fn decrqm_reply(params: &[u8], private: bool, modes: &ModeReport) -> Option<Vec<u8>> {
    let text = std::str::from_utf8(params).ok()?;
    let ps: u16 = text.strip_prefix('?').unwrap_or(text).parse().ok()?;
    let pm = mode_pm(ps, private, modes);
    let q = if private { "?" } else { "" };
    Some(format!("\x1b[{q}{ps};{pm}$y").into_bytes())
}

/// The `Pm` (set/reset/unsupported) value for a `DECRQM` mode query.
fn mode_pm(ps: u16, private: bool, m: &ModeReport) -> u8 {
    let (known, set) = if private {
        match ps {
            1 => (true, m.application_cursor),
            25 => (true, !m.hide_cursor),
            1004 => (true, m.focus_reporting),
            2004 => (true, m.bracketed_paste),
            1049 => (true, m.alternate_screen),
            // Any of the mouse tracking modes; we do not distinguish them.
            1000 | 1002 | 1003 | 1005 | 1006 => (true, m.mouse_tracking),
            // Auto-wrap is always enabled and cannot be turned off here.
            7 => (true, true),
            _ => (false, false),
        }
    } else {
        (false, false)
    };
    if !known {
        0
    } else if set {
        1
    } else {
        2
    }
}

/// Scan a chunk of terminal writing for device-query sequences that demand a
/// response (DA1, DA2, cursor position, OSC-11 background colour, the kitty
/// keyboard query, XTVERSION and XTGETTCAP). Responding keeps shells from
/// stalling on unanswered queries.
///
/// The last three matter most in practice: bash 5.2+ and fish each block at
/// startup until the terminal answers `CSI ? u` and the `DCS + q` capability
/// requests, so leaving them unanswered makes the terminal look dead — no
/// prompt ever appears.
///
/// Pure + unit-tested; `row`/`col` are the 1-based cursor position to report
/// for a `\x1b[6n` query.
#[cfg(test)]
pub(crate) fn collect_query_replies(bytes: &[u8], row: usize, col: usize) -> Vec<u8> {
    collect_query_replies_with_modes(bytes, row, col, &ModeReport::default())
}
/// Like [`collect_query_replies`], but with the live terminal modes so `DECRQM`
/// can answer set/reset for the modes krust actually models.
pub(crate) fn collect_query_replies_with_modes(
    bytes: &[u8],
    row: usize,
    col: usize,
    modes: &ModeReport,
) -> Vec<u8> {
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
                let bg = crate::color::default_bg();
                let (r, g, b) = ((bg >> 16) & 0xff, (bg >> 8) & 0xff, bg & 0xff);
                out.extend_from_slice(
                    format!(
                        "\x1b]11;rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}\x1b\\"
                    )
                    .as_bytes(),
                );
            }
            i = k + 1;
            continue;
        }
        // DCS: ESC P <params> <data> ESC \  (XTGETTCAP is `DCS + q ... ST`)
        if i + 1 < bytes.len() && bytes[i + 1] == b'P' {
            let mut k = i + 2;
            while k + 1 < bytes.len() {
                if bytes[k] == 0x1b && bytes[k + 1] == b'\\' {
                    break;
                }
                k += 1;
            }
            if k + 1 >= bytes.len() {
                // Unterminated: wait for the rest rather than mis-parsing it.
                break;
            }
            // XTGETTCAP is `DCS + q <hex names> ST`; the only DCS we answer.
            let body_start = i + 2;
            if bytes.len() > body_start + 1
                && bytes[body_start] == b'+'
                && bytes[body_start + 1] == b'q'
            {
                if let Some(reply) = xtgettcaps_reply(&bytes[body_start + 2..k]) {
                    out.extend_from_slice(&reply);
                }
            }
            i = k + 2;
            continue;
        }
        // CSI sequences
        if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
            let mut k = i + 2;
            let mut has_greater = false;
            let mut has_question = false;
            let mut has_equal = false;
            while k < bytes.len()
                && (bytes[k].is_ascii_digit()
                    || matches!(bytes[k], b';' | b'>' | b'?' | b'='))
            {
                match bytes[k] {
                    b'>' => has_greater = true,
                    b'?' => has_question = true,
                    b'=' => has_equal = true,
                    _ => {}
                }
                k += 1;
            }
            if k >= bytes.len() {
                i += 1;
                continue;
            }
            let params = &bytes[i + 2..k];
            // CSI ? Ps $ p  (DECRQM): report set/reset/unsupported.
            if bytes[k] == b'$' && k + 1 < bytes.len() && bytes[k + 1] == b'p' {
                if let Some(reply) = decrqm_reply(params, has_question, modes) {
                    out.extend_from_slice(&reply);
                }
                i = k + 2;
                continue;
            }
            match bytes[k] {
                b'c' if has_equal => {
                    // CSI = c  (DA3): tertiary device attributes.
                    out.extend_from_slice(b"\x1bP!|00000000\x1b\\");
                }
                b'c' if params.is_empty() || params == b"0" => {
                    out.extend_from_slice(b"\x1b[?1;2c");
                }
                b'c' if has_greater => {
                    out.extend_from_slice(b"\x1b[>0;1;0c");
                }
                b'n' if params == b"6" => {
                    out.extend_from_slice(format!("\x1b[{};{}R", row + 1, col + 1).as_bytes());
                }
                // CSI > Ps q  (XTVERSION): identify the terminal.
                b'q' if has_greater => {
                    out.extend_from_slice(
                        format!("\x1bP>|{}({})\x1b\\", TERM_NAME, TERM_VERSION).as_bytes(),
                    );
                }
                // CSI ? u  (kitty keyboard protocol query). Reply with flags 0
                // to say "legacy keys only", which is what we actually render.
                b'u' if has_question => {
                    out.extend_from_slice(b"\x1b[?0u");
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

#[cfg(test)]
mod tests {
    use super::{collect_query_replies, collect_query_replies_with_modes, ModeReport};

    #[test]
    fn osc11_reply_reflects_the_palette_override() {
        crate::color::reset_palette();
        // Default: the built-in dark background.
        assert_eq!(
            collect_query_replies(b"\x1b]11;?\x1b\\", 0, 0),
            b"\x1b]11;rgb:2b2b/2b2b/2b2b\x1b\\"
        );
        crate::color::apply_dynamic_color(&[b"11", b"#112233"]);
        assert_eq!(
            collect_query_replies(b"\x1b]11;?\x1b\\", 0, 0),
            b"\x1b]11;rgb:1111/2222/3333\x1b\\"
        );
        crate::color::reset_palette();
    }

    #[test]
    fn da3_replies_with_tertiary_attributes() {
        assert_eq!(collect_query_replies(b"\x1b[=c", 0, 0), b"\x1bP!|00000000\x1b\\");
    }

    #[test]
    fn decrqm_reports_set_reset_and_unsupported() {
        let m = ModeReport {
            bracketed_paste: true,
            ..ModeReport::default()
        };
        assert_eq!(
            collect_query_replies_with_modes(b"\x1b[?2004$p", 0, 0, &m),
            b"\x1b[?2004;1$y"
        );
        // Cursor visibility defaults on (hide_cursor false -> set).
        assert_eq!(
            collect_query_replies_with_modes(b"\x1b[?25$p", 0, 0, &m),
            b"\x1b[?25;1$y"
        );
        assert_eq!(
            collect_query_replies_with_modes(b"\x1b[?1004$p", 0, 0, &m),
            b"\x1b[?1004;2$y"
        );
        assert_eq!(
            collect_query_replies_with_modes(b"\x1b[?9999$p", 0, 0, &m),
            b"\x1b[?9999;0$y"
        );
        // Non-private DECRQM: nothing modeled -> unsupported.
        assert_eq!(
            collect_query_replies_with_modes(b"\x1b[4$p", 0, 0, &m),
            b"\x1b[4;0$y"
        );
    }
}