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
            if !c.is_control() {
                let mut buf = [0u8; 4];
                let mut out = Vec::with_capacity(5);
                out.push(0x1b);
                out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
                return out;
            }
        }
        return Vec::new();
    }

    // Plain single non-control character passes through.
    if let Some(c) = single_char {
        if !c.is_control() {
            let mut buf = [0u8; 4];
            return c.encode_utf8(&mut buf).as_bytes().to_vec();
        }
    }

    Vec::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accented_characters_encode_as_utf8() {
        assert_eq!(map_key("é", false, false, false, false), vec![0xc3, 0xa9]);
        assert_eq!(map_key("ü", false, false, false, false), vec![0xc3, 0xbc]);
        assert_eq!(map_key("ñ", false, false, false, false), vec![0xc3, 0xb1]);
        assert_eq!(map_key("à", false, false, false, false), vec![0xc3, 0xa0]);
    }

    #[test]
    fn cyrillic_characters_encode_as_utf8() {
        assert_eq!(map_key("а", false, false, false, false), vec![0xd0, 0xb0]);
        assert_eq!(map_key("б", false, false, false, false), vec![0xd0, 0xb1]);
        assert_eq!(map_key("ж", false, false, false, false), vec![0xd0, 0xb6]);
    }

    #[test]
    fn cjk_ideographs_encode_as_utf8() {
        assert_eq!(map_key("中", false, false, false, false), vec![0xe4, 0xb8, 0xad]);
        assert_eq!(map_key("字", false, false, false, false), vec![0xe5, 0xad, 0x97]);
        assert_eq!(map_key("元", false, false, false, false), vec![0xe5, 0x85, 0x83]);
    }

    #[test]
    fn emoji_encode_as_utf8() {
        // Emoji are single-character codepoints; encode to UTF-8.
        assert_eq!(map_key("😀", false, false, false, false), vec![0xf0, 0x9f, 0x98, 0x80]);
    }

    #[test]
    fn alt_accented_character_prefixed_with_esc() {
        let mut buf = [0u8; 4];
        let mut expected = Vec::with_capacity(5);
        expected.push(0x1b);
        expected.extend_from_slice('\u{e9}'.encode_utf8(&mut buf).as_bytes());
        assert_eq!(map_key("é", false, true, false, false), expected);
    }

    #[test]
    fn control_characters_are_dropped() {
        assert_eq!(map_key("\x00", false, false, false, false), Vec::<u8>::new());
    }

    #[test]
    fn tab_still_returns_control_sequence() {
        assert_eq!(map_key("Tab", false, false, false, false), vec![0x09]);
    }
}
