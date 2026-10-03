// Server-side screen replayer.
//
// A client's VT parser can only rebuild the true screen from a *complete*
// byte stream. The retained log is trimmed to a mid-stream window, so
// replaying it alone leaves every cell the app drew before the window blank
// (and drifts modes/attributes) — which was the focus-loss glitch: a
// FullReset landed on a client, its parser restarted from a trimmed log, and
// whole regions painted as bare background until new output happened to
// rewrite them.
//
// Instead, the session keeps a mirror `vt100::Parser` fed with every PTY
// byte, and `replay_image` turns the mirror's current screen into a plain
// ANSI stream that repaints that exact screen on any client:
//
//   {"type":"Reset"}    (client drops its parser state)
//   retained log        (best-effort scrollback history)
//   replay_image bytes  (absolute repaint of the true visible screen)
//
// The image is built with vt100's own `Screen::state_diff`, applied against
// a parser that has consumed exactly what the client will have consumed when
// the image arrives (the retained log; nothing for a blank FullReset), so
// the diff is precise no matter how far the client's parse of the log
// drifted from the true screen. An `ESC[?1049h/l` prelude aligns alt-screen
// state (DECSET 1049 unconditionally clears the alt grid, so the image
// always paints onto a blank target), and a final CUP pins the cursor. The
// stream is ordinary terminal output — no client or protocol changes.
use vt100::Parser;

/// Build the ANSI stream that repaints the mirror's current screen.
///
/// `mirror_screen` is a clone of the mirror's screen, taken under the mirror
/// lock so it agrees with the mirror's stream offset. `retained` is the byte
/// log the client will have re-parsed right before this image arrives (the
/// log the server sends after the Reset frame). Feeding those same bytes to
/// the `prev` parser reconstructs the client's exact state so the diff
/// accounts for its drift.
pub(crate) fn replay_image(mirror_screen: &vt100::Screen, retained: &[u8]) -> Vec<u8> {
    let (rows, cols) = mirror_screen.size();
    let mut prev = Parser::new(rows, cols, 0);
    prev.process(retained);

    let mut img: Vec<u8> = Vec::new();

    let want_alt = mirror_screen.alternate_screen();
    if prev.screen().alternate_screen() != want_alt {
        // Idempotent by vt100 semantics: DECSET 1049 always clears the alt
        // grid before entering, and DECRST 1049 is a no-op while already on
        // the normal screen. Apply the same prelude to `prev` so both sides
        // are diffing the same screen kind.
        img.extend_from_slice(if want_alt {
            b"\x1b[?1049h"
        } else {
            b"\x1b[?1049l"
        });
        prev.process(&img);
    }

    img.extend_from_slice(&mirror_screen.state_diff(prev.screen()));

    // The diff leaves the cursor wherever the last painted cell did; pin it
    // to the mirror's real cursor position.
    let (r, c) = mirror_screen.cursor_position();
    img.extend_from_slice(format!("\x1b[{};{}H", r + 1, c + 1).as_bytes());

    img
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn assert_screens_equal(a: &Parser, b: &Parser) {
        let (rows, cols) = a.screen().size();
        assert_eq!(b.screen().size(), (rows, cols), "screens must share a size");
        for row in 0..rows {
            for col in 0..cols {
                let (ca, cb) = (a.screen().cell(row, col), b.screen().cell(row, col));
                match (ca, cb) {
                    (Some(x), Some(y)) => {
                        assert_eq!(x.contents(), y.contents(), "cell {row},{col} contents");
                        assert_eq!(x.fgcolor(), y.fgcolor(), "cell {row},{col} fg");
                        assert_eq!(x.bgcolor(), y.bgcolor(), "cell {row},{col} bg");
                        assert_eq!(x.bold(), y.bold(), "cell {row},{col} bold");
                        assert_eq!(x.dim(), y.dim(), "cell {row},{col} dim");
                        assert_eq!(x.italic(), y.italic(), "cell {row},{col} italic");
                        assert_eq!(x.underline(), y.underline(), "cell {row},{col} underline");
                        assert_eq!(x.inverse(), y.inverse(), "cell {row},{col} inverse");
                        assert_eq!(x.is_wide(), y.is_wide(), "cell {row},{col} wide");
                        assert_eq!(
                            x.is_wide_continuation(),
                            y.is_wide_continuation(),
                            "cell {row},{col} wide-continuation"
                        );
                    }
                    (None, None) => {}
                    (x, y) => panic!("cell {row},{col} presence mismatch: {x:?} vs {y:?}"),
                }
            }
        }
        assert_eq!(
            a.screen().cursor_position(),
            b.screen().cursor_position(),
            "cursor"
        );
        assert_eq!(
            a.screen().alternate_screen(),
            b.screen().alternate_screen(),
            "alternate screen"
        );
        assert_eq!(
            a.screen().hide_cursor(),
            b.screen().hide_cursor(),
            "cursor visibility"
        );
        assert_eq!(
            a.screen().application_cursor(),
            b.screen().application_cursor(),
            "application cursor mode"
        );
        assert_eq!(
            a.screen().bracketed_paste(),
            b.screen().bracketed_paste(),
            "bracketed paste"
        );
    }

    /// The core regression: a replay window that has trimmed away the
    /// stream's prefix cannot reconstruct the mirror's screen on its own;
    /// the image built from the mirror state can.
    #[test]
    fn replay_image_reconstructs_after_a_trimmed_log_replay() {
        // Prefix the retained window trims away; content afterwards only
        // rewrites part of the screen, so the lost prefix is observable.
        let part1: &[u8] = b"\x1b[2J\x1b[Hlegacy line one\r\nlegacy line two\r\n";
        let part2: &[u8] =
            b"\x1b[38;2;100;200;50m\xe6\xb1\x89\xe5\xad\x97 line\x1b[0m\r\nmid-stream tail only\r\n";
        let mut mirror = Parser::new(6, 60, 0);
        mirror.process(part1);
        mirror.process(part2);

        // The retained window starts at an ESC boundary inside the stream:
        // exactly what the trim produces. The client re-parses only this.
        let esc = part2.iter().position(|&b| b == 0x1b).unwrap();
        let retained = &part2[esc..];

        let mut client = Parser::new(6, 60, 0);
        client.process(retained);
        assert_ne!(
            screen_text(&client),
            screen_text(&mirror),
            "premise: replaying a trimmed window alone must diverge from the true screen"
        );

        client.process(&replay_image(mirror.screen(), retained));
        assert_screens_equal(&client, &mirror);
    }

    /// Same guarantee while a TUI owns the alternate screen, including the
    /// case where the client's replay never entered it at all.
    #[test]
    fn replay_image_reconstructs_the_alternate_screen() {
        let mut mirror = Parser::new(8, 50, 0);
        mirror.process(b"normal screen junk\r\n");
        let entry: &[u8] = b"\x1b[?1049h\x1b[2J\x1b[?1h\x1b[?2004h";
        let content: &[u8] =
            b"\x1b[36malt content \xe6\xb1\x89\xe5\xad\x97 spinner \xe2\xa0\x8b\x1b[0m\x1b[5;9Hpoint";
        mirror.process(entry);
        mirror.process(content);

        assert!(mirror.screen().alternate_screen());

        // Window A: the retained log includes the 1049 entry, so the
        // client's replay lands on the alt screen before the image arrives.
        let retained_a = [entry, content].concat();
        let mut client = Parser::new(8, 50, 0);
        client.process(&retained_a);
        client.process(&replay_image(mirror.screen(), &retained_a));
        assert_screens_equal(&client, &mirror);

        // Window B: trims *past* the 1049 entry, so the client's replay is
        // still on the normal screen; the prelude must switch it over.
        let mut client2 = Parser::new(8, 50, 0);
        client2.process(content);
        assert!(!client2.screen().alternate_screen());
        client2.process(&replay_image(mirror.screen(), content));
        assert_screens_equal(&client2, &mirror);
    }

    /// The FullReset path hands a stalled client a Reset and an image built
    /// against a blank screen — no retained log at all.
    #[test]
    fn replay_image_from_blank_rebuilds_the_screen() {
        let mut mirror = Parser::new(6, 60, 0);
        mirror.process(
            b"\x1b[2J\x1b[H\x1b[31mred\x1b[1mbold bold\x1b[m plain\r\nrow two\x08\x08.\r\n",
        );
        mirror.process(b"\x1b[?1049h\x1b[2J\x1b[34mtui on alt\x1b[3;2H");

        let mut client = Parser::new(6, 60, 0);
        client.process(&replay_image(mirror.screen(), b""));
        assert_screens_equal(&client, &mirror);
    }
}
