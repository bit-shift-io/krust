// Client keyboard input mapping.
//
// Maps browser keyboard events to raw bytes written to the PTY. Follows the
// mapping table in `NOTES.md`: Ctrl+letter → control code, arrows → CSI
// sequences, F-keys → `CSI N~`, Alt+char → `ESC char`.

/// Encode the standard xterm modifier parameter (1 + shift 1 + alt 2 + ctrl 4).
///
/// Returns `None` when no application modifiers are active.
pub(crate) fn xterm_modifier_param(ctrl: bool, alt: bool, shift: bool) -> Option<u8> {
    let mut param = 1u8;
    let mut any = false;
    if shift {
        param += 1;
        any = true;
    }
    if alt {
        param += 2;
        any = true;
    }
    if ctrl {
        param += 4;
        any = true;
    }
    any.then_some(param)
}

/// Whether a character is directly typeable into a PTY (graphic or space).
fn is_printable_ascii(c: char) -> bool {
    c.is_ascii_graphic() || c == ' '
}

/// Map a browser keyboard event to the raw bytes to write to the PTY.
///
/// Follows the mapping table in `NOTES.md`: Ctrl+letter → control code,
/// arrows → CSI sequences, F-keys → `CSI N~`, Alt+char → `ESC char`.
/// Local echo is disabled by the server, so nothing is echoed here.
pub(crate) fn map_key(key: &str, ctrl: bool, alt: bool, shift: bool, _meta: bool) -> Vec<u8> {
    let single_char = if key.chars().count() == 1 {
        key.chars().next()
    } else {
        None
    };

    // Ctrl + letter → control character (Ctrl+C = \x03, etc.)
    if let Some(c) = single_char {
        if ctrl && c.is_ascii_alphabetic() {
            return vec![c.to_ascii_lowercase() as u8 - b'a' + 1];
        }
    }

    // Ctrl + punctuation/space control codes.
    if ctrl {
        let code = match key {
            "Space" => Some(0x00),
            "[" => Some(0x1b),
            "\\" => Some(0x1c),
            "]" => Some(0x1d),
            "^" => Some(0x1e),
            "_" => Some(0x1f),
            "Backspace" => Some(0x08),
            _ => None,
        };
        if let Some(c) = code {
            return vec![c];
        }
    }

    match key {
        "Enter" => {
            if shift {
                return vec![0x1b, 0x0d];
            }
            if let Some(m) = xterm_modifier_param(ctrl, alt, shift) {
                return format!("\x1b[13;{}u", m).into_bytes();
            }
            return vec![0x0d];
        }
        "Tab" => {
            if shift {
                return b"\x1b[Z".to_vec();
            }
            return vec![0x09];
        }
        "Escape" => return vec![0x1b],
        "Backspace" => return vec![0x7f],
        "Delete" => return b"\x1b[3~".to_vec(),
        "Insert" => return b"\x1b[2~".to_vec(),
        "Home" => return b"\x1b[H".to_vec(),
        "End" => return b"\x1b[F".to_vec(),
        "PageUp" => return b"\x1b[5~".to_vec(),
        "PageDown" => return b"\x1b[6~".to_vec(),
        "ArrowUp" => return b"\x1b[A".to_vec(),
        "ArrowDown" => return b"\x1b[B".to_vec(),
        "ArrowRight" => return b"\x1b[C".to_vec(),
        "ArrowLeft" => return b"\x1b[D".to_vec(),
        _ => {}
    }

    let f_tail = match key {
        "F1" => Some(11),
        "F2" => Some(12),
        "F3" => Some(13),
        "F4" => Some(14),
        "F5" => Some(15),
        "F6" => Some(17),
        "F7" => Some(18),
        "F8" => Some(19),
        "F9" => Some(20),
        "F10" => Some(21),
        "F11" => Some(23),
        "F12" => Some(24),
        _ => None,
    };
    if let Some(t) = f_tail {
        return format!("\x1b[{}~", t).into_bytes();
    }

    // Alt + printable → ESC prefix.
    if alt {
        if let Some(c) = single_char {
            if is_printable_ascii(c) {
                let mut buf = [0u8; 4];
                let mut out = Vec::with_capacity(5);
                out.push(0x1b);
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                return out;
            }
        }
        return Vec::new();
    }

    // Plain single printable character passes through.
    if let Some(c) = single_char {
        if is_printable_ascii(c) {
            let mut buf = [0u8; 4];
            return c.encode_utf8(&mut buf).as_bytes().to_vec();
        }
    }

    Vec::new()
}
