// Mouse report encoding for the xterm mouse protocols.
//
// Whether — and how — a browser mouse event is reported to the application
// is decided entirely by the private modes the application set on the vt100
// screen: 9 / 1000 / 1002 / 1003 pick the protocol, 1005 / 1006 pick the
// encoding. The page therefore keeps no mouse state of its own: it asks this
// module (through the `mouse_event` export) for the report bytes of each
// event and falls back to its own selection / scrollback behaviour when no
// bytes come back.
//
// Protocol restrictions and byte encodings mirror xterm.js's
// `CoreMouseService` (DEFAULT_PROTOCOLS / DEFAULT_ENCODINGS), which in turn
// mirrors xterm — including the deliberate quirks: the single-byte encoding
// drops any report whose fields exceed 255 instead of clamping, and only
// SGR can name the button on release.

use vt100::{MouseProtocolEncoding, MouseProtocolMode};

// --- FFI wire format ------------------------------------------------------
//
// Event kinds, as passed across the boundary by the page.

pub(crate) const KIND_PRESS: i32 = 0;
pub(crate) const KIND_RELEASE: i32 = 1;
pub(crate) const KIND_MOTION: i32 = 2;
pub(crate) const KIND_WHEEL: i32 = 3;

// Modifier bits, as passed across the boundary by the page.
pub(crate) const MOD_SHIFT: i32 = 1;
pub(crate) const MOD_ALT: i32 = 2;
pub(crate) const MOD_CTRL: i32 = 4;

/// A mouse event, decoded from the page's numeric kind code.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum MouseKind {
    Press,
    Release,
    /// Pointer moved: `button` is the held button, or 3 when none is held.
    Motion,
    /// Wheel notch: `button` is the direction (0 up, 1 down, 2 left, 3 right).
    Wheel,
}

impl MouseKind {
    pub(crate) fn from_raw(v: i32) -> Option<Self> {
        match v {
            KIND_PRESS => Some(MouseKind::Press),
            KIND_RELEASE => Some(MouseKind::Release),
            KIND_MOTION => Some(MouseKind::Motion),
            KIND_WHEEL => Some(MouseKind::Wheel),
            _ => None,
        }
    }
}

/// Encode one mouse event for the active protocol and encoding.
///
/// * `button` — 0/1/2 = left/middle/right, 3 = no button (motion only);
///   for `Wheel` it is the direction instead (0 up … 3 right).
/// * `col`, `row` — 1-based cell coordinates.
/// * `rows`, `cols` — screen size, for the range check.
/// * `mods` — [`MOD_SHIFT`] / [`MOD_ALT`] / [`MOD_CTRL`] bits.
///
/// Returns `None` when the protocol does not want this event (the page then
/// falls back to its own selection/scrolling behaviour) or when the encoding
/// cannot represent the coordinates.
pub(crate) fn mouse_report(
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
    kind: MouseKind,
    button: i32,
    col: i32,
    row: i32,
    rows: i32,
    cols: i32,
    mods: i32,
) -> Option<Vec<u8>> {
    // xterm.js rejects events outside the grid rather than clamping them.
    if col < 1 || col > cols || row < 1 || row > rows {
        return None;
    }
    let button = button.clamp(0, 3);
    // A press/release without a button is nonsense (xterm maps such events
    // to "no button" and drops them); motion is exactly the case where "no
    // button held" is meaningful.
    if kind != MouseKind::Motion && kind != MouseKind::Wheel && button == 3 {
        return None;
    }

    let mut mods = mods & (MOD_SHIFT | MOD_ALT | MOD_CTRL);

    // Protocol restrictions (xterm.js DEFAULT_PROTOCOLS).
    match mode {
        MouseProtocolMode::None => return None,
        MouseProtocolMode::Press => {
            // X10: presses only — no release, no wheel, no modifiers.
            if kind != MouseKind::Press {
                return None;
            }
            mods = 0;
        }
        MouseProtocolMode::PressRelease => {
            // VT200: press / release / wheel, but no motion.
            if kind == MouseKind::Motion {
                return None;
            }
        }
        MouseProtocolMode::ButtonMotion => {
            // 1002: like VT200 plus motion while a button is held.
            if kind == MouseKind::Motion && button == 3 {
                return None;
            }
        }
        // 1003: every motion is reported, including hover.
        MouseProtocolMode::AnyMotion => {}
    }

    // Build the event code (xterm.js eventCode()).
    let button = button as u8;
    let mut code: u8 = 0;
    if mods & MOD_SHIFT != 0 {
        code |= 4;
    }
    if mods & MOD_ALT != 0 {
        code |= 8;
    }
    if mods & MOD_CTRL != 0 {
        code |= 16;
    }
    match kind {
        MouseKind::Wheel => code |= 64 | button,
        MouseKind::Motion => code |= 32 | button,
        MouseKind::Press => code |= button,
        MouseKind::Release => {
            code |= button;
            // Only SGR can say which button was released; every other
            // encoding has to report "button 3" (none).
            if encoding != MouseProtocolEncoding::Sgr {
                code |= 3;
            }
        }
    }

    let is_release = matches!(kind, MouseKind::Release);
    Some(match encoding {
        // `CSI < Pb ; Px ; Py M` — press/motion, `m` on release.
        MouseProtocolEncoding::Sgr => format!(
            "\x1b[<{};{};{}{}",
            code,
            col,
            row,
            if is_release { 'm' } else { 'M' }
        )
        .into_bytes(),
        // `CSI M Pb Px Py` — three single bytes, so every field must fit
        // in one byte (i.e. ≤ 223 after the +32 offset). xterm.js drops the
        // report entirely rather than clamping.
        MouseProtocolEncoding::Default => {
            let fields = [i32::from(code) + 32, col + 32, row + 32];
            if fields.iter().any(|&v| v > 255) {
                return None;
            }
            let mut out = Vec::with_capacity(7);
            out.extend_from_slice(b"\x1b[M");
            out.extend(fields.iter().map(|&v| v as u8));
            out
        }
        // Mode 1005: like the single-byte encoding, but each field is a
        // Unicode code point (value + 32) written as UTF-8, which lifts the
        // one-byte coordinate limit.
        MouseProtocolEncoding::Utf8 => {
            let mut out = Vec::with_capacity(16);
            out.extend_from_slice(b"\x1b[M");
            for value in [i32::from(code) + 32, col + 32, row + 32] {
                // Every field is < 0x110000 by construction (cols/rows are
                // u16, the code is at most 0x9F+32), so the scalar is valid.
                let ch = char::from_u32(value as u32).unwrap_or('\u{FFFD}');
                let mut buf = [0u8; 4];
                out.extend_from_slice(ch.encode_utf8(&mut buf).as_bytes());
            }
            out
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sgr(kind: MouseKind, button: i32, col: i32, row: i32) -> Option<Vec<u8>> {
        mouse_report(
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Sgr,
            kind,
            button,
            col,
            row,
            24,
            80,
            0,
        )
    }

    #[test]
    fn sgr_press_release_and_motion() {
        assert_eq!(sgr(MouseKind::Press, 0, 5, 3).unwrap(), b"\x1b[<0;5;3M");
        assert_eq!(sgr(MouseKind::Press, 2, 1, 1).unwrap(), b"\x1b[<2;1;1M");
        // Release names the released button and ends in `m`.
        assert_eq!(sgr(MouseKind::Release, 0, 5, 3).unwrap(), b"\x1b[<0;5;3m");
        assert_eq!(sgr(MouseKind::Release, 1, 5, 3).unwrap(), b"\x1b[<1;5;3m");
        // Drag carries the held button plus the motion bit.
        assert_eq!(sgr(MouseKind::Motion, 0, 5, 3).unwrap(), b"\x1b[<32;5;3M");
        // A hover-move ("no button held") only exists under mode 1003.
        assert_eq!(
            mouse_report(
                MouseProtocolMode::AnyMotion,
                MouseProtocolEncoding::Sgr,
                MouseKind::Motion,
                3,
                5,
                3,
                24,
                80,
                0
            )
            .unwrap(),
            b"\x1b[<35;5;3M"
        );
    }

    #[test]
    fn sgr_wheel_uses_button_64_and_up_down() {
        assert_eq!(
            sgr(MouseKind::Wheel, 0, 10, 20).unwrap(),
            b"\x1b[<64;10;20M"
        );
        assert_eq!(
            sgr(MouseKind::Wheel, 1, 10, 20).unwrap(),
            b"\x1b[<65;10;20M"
        );
    }

    #[test]
    fn sgr_reports_modifiers() {
        let report = mouse_report(
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
            MouseKind::Press,
            0,
            4,
            2,
            24,
            80,
            MOD_CTRL | MOD_ALT,
        )
        .unwrap();
        // ctrl (16) + alt (8) + left (0)
        assert_eq!(report, b"\x1b[<24;4;2M");
    }

    #[test]
    fn x10_mode_strips_modifiers_and_reports_presses_only() {
        let mode = MouseProtocolMode::Press;
        let enc = MouseProtocolEncoding::Default;
        // Modifiers are cleared, not encoded.
        let report = mouse_report(mode, enc, MouseKind::Press, 0, 4, 2, 24, 80, MOD_SHIFT)
            .unwrap();
        assert_eq!(report, b"\x1b[M\x20$\x22"); // code 0+32, col 4+32, row 2+32
        // No releases, no motion, no wheel.
        assert!(mouse_report(mode, enc, MouseKind::Release, 0, 4, 2, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Motion, 0, 4, 2, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Wheel, 0, 4, 2, 24, 80, 0).is_none());
    }

    #[test]
    fn vt200_rejects_motion_but_allows_press_release_wheel() {
        let mode = MouseProtocolMode::PressRelease;
        let enc = MouseProtocolEncoding::Sgr;
        assert!(mouse_report(mode, enc, MouseKind::Motion, 0, 4, 2, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Motion, 3, 4, 2, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Press, 0, 4, 2, 24, 80, 0).is_some());
        assert!(mouse_report(mode, enc, MouseKind::Release, 0, 4, 2, 24, 80, 0).is_some());
        assert!(mouse_report(mode, enc, MouseKind::Wheel, 1, 4, 2, 24, 80, 0).is_some());
    }

    #[test]
    fn drag_mode_rejects_hover_moves() {
        let mode = MouseProtocolMode::ButtonMotion;
        let enc = MouseProtocolEncoding::Sgr;
        assert!(mouse_report(mode, enc, MouseKind::Motion, 3, 4, 2, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Motion, 0, 4, 2, 24, 80, 0).is_some());
    }

    #[test]
    fn any_motion_mode_reports_hover_moves() {
        let report = mouse_report(
            MouseProtocolMode::AnyMotion,
            MouseProtocolEncoding::Sgr,
            MouseKind::Motion,
            3,
            7,
            9,
            24,
            80,
            0,
        )
        .unwrap();
        assert_eq!(report, b"\x1b[<35;7;9M");
    }

    #[test]
    fn no_protocol_reports_nothing() {
        assert!(
            mouse_report(
                MouseProtocolMode::None,
                MouseProtocolEncoding::Sgr,
                MouseKind::Press,
                0,
                4,
                2,
                24,
                80,
                0
            )
            .is_none()
        );
    }

    #[test]
    fn default_encoding_uses_single_bytes_and_drops_overflow() {
        let enc = MouseProtocolEncoding::Default;
        // Press left at 4,2 → `M` + (0+32) + (4+32) + (2+32).
        assert_eq!(
            mouse_report(
                MouseProtocolMode::PressRelease,
                enc,
                MouseKind::Press,
                0,
                4,
                2,
                24,
                80,
                0
            )
            .unwrap(),
            b"\x1b[M\x20$\x22"
        );
        // Release always reports "no button" (3+32 = 35 = '#').
        assert_eq!(
            mouse_report(
                MouseProtocolMode::PressRelease,
                enc,
                MouseKind::Release,
                1,
                4,
                2,
                24,
                80,
                0
            )
            .unwrap(),
            b"\x1b[M#$\x22"
        );
        // Column 224 → 256 does not fit a byte: no report at all.
        assert!(
            mouse_report(
                MouseProtocolMode::PressRelease,
                enc,
                MouseKind::Press,
                0,
                224,
                2,
                24,
                300,
                0
            )
            .is_none()
        );
        // 223 still fits (255).
        assert!(mouse_report(
            MouseProtocolMode::PressRelease,
            enc,
            MouseKind::Press,
            0,
            223,
            2,
            24,
            300,
            0
        )
        .is_some());
    }

    #[test]
    fn utf8_encoding_writes_multibyte_fields() {
        let report = mouse_report(
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Utf8,
            MouseKind::Press,
            0,
            300,
            2,
            400,
            400,
            0,
        )
        .unwrap();
        // Fields are code+32 = 32, col+32 = 332 (two UTF-8 bytes), row+32
        // = 34 — no single-byte limit anywhere.
        let mut expected = b"\x1b[M".to_vec();
        for value in [32u32, 332, 34] {
            let mut buf = [0u8; 4];
            expected.extend_from_slice(char::from_u32(value).unwrap().encode_utf8(&mut buf).as_bytes());
        }
        assert_eq!(report, expected);
    }

    #[test]
    fn out_of_range_coordinates_are_rejected() {
        let enc = MouseProtocolEncoding::Sgr;
        let mode = MouseProtocolMode::AnyMotion;
        assert!(mouse_report(mode, enc, MouseKind::Press, 0, 0, 5, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Press, 0, 81, 5, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Press, 0, 5, 25, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Press, 0, -3, 5, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Press, 0, 80, 24, 24, 80, 0).is_some());
    }

    #[test]
    fn press_or_release_without_a_button_is_dropped() {
        let mode = MouseProtocolMode::AnyMotion;
        let enc = MouseProtocolEncoding::Sgr;
        assert!(mouse_report(mode, enc, MouseKind::Press, 3, 4, 2, 24, 80, 0).is_none());
        assert!(mouse_report(mode, enc, MouseKind::Release, 3, 4, 2, 24, 80, 0).is_none());
    }

    #[test]
    fn kind_decoding_round_trips() {
        assert_eq!(MouseKind::from_raw(KIND_PRESS), Some(MouseKind::Press));
        assert_eq!(MouseKind::from_raw(KIND_RELEASE), Some(MouseKind::Release));
        assert_eq!(MouseKind::from_raw(KIND_MOTION), Some(MouseKind::Motion));
        assert_eq!(MouseKind::from_raw(KIND_WHEEL), Some(MouseKind::Wheel));
        assert_eq!(MouseKind::from_raw(4), None);
        assert_eq!(MouseKind::from_raw(-1), None);
    }

    // The wiring this whole module exists for: an app (opencode, via
    // @opentui) enables its modes on the vt100 screen, and the screen's
    // state decides the bytes we send back.
    #[test]
    fn opencode_enable_sequence_drives_the_report() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h");
        let screen = parser.screen();
        let report = mouse_report(
            screen.mouse_protocol_mode(),
            screen.mouse_protocol_encoding(),
            MouseKind::Press,
            0,
            5,
            3,
            24,
            80,
            0,
        )
        .unwrap();
        assert_eq!(report, b"\x1b[<0;5;3M");

        // DECRST hands the pointer back to the page: no reports.
        parser.process(b"\x1b[?1002l\x1b[?1000l\x1b[?1006l");
        let screen = parser.screen();
        assert_eq!(screen.mouse_protocol_mode(), MouseProtocolMode::None);
        assert!(
            mouse_report(
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
                MouseKind::Press,
                0,
                5,
                3,
                24,
                80,
                0
            )
            .is_none()
        );
    }

    #[test]
    fn vt200_only_sequence_reports_press_release_in_sgr() {
        let mut parser = vt100::Parser::new(24, 80, 0);
        parser.process(b"\x1b[?1000h\x1b[?1006h");
        let screen = parser.screen();
        assert_eq!(screen.mouse_protocol_mode(), MouseProtocolMode::PressRelease);
        assert_eq!(
            screen.mouse_protocol_encoding(),
            MouseProtocolEncoding::Sgr
        );
        // Motion is not wanted, even with a button held.
        assert!(
            mouse_report(
                screen.mouse_protocol_mode(),
                screen.mouse_protocol_encoding(),
                MouseKind::Motion,
                0,
                5,
                3,
                24,
                80,
                0
            )
            .is_none()
        );
    }
}
