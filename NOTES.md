# Notes: Krust Terminal

## Focus-loss / tab-switch rendering glitch — root cause & fix

### Symptom (as reported)

- Trigger: switching browser tabs away and back, Firefox/Linux; the longer
  hidden, the more likely. Both Canvas 2D and WebGL2 renderers.
- Appearance: large regions of the canvas painted as bare background color;
  cells later rewritten by new output redraw one at a time. Persists — a full
  browser refresh does not fix it.

### Root cause (not a renderer bug, not an outdated dependency)

Replaying the trimmed ≤512 KB byte log into a stateful VT parser cannot
reconstruct the true screen once the session has produced more than 512 KB:
the window's prefix is gone, so every cell the app drew before it and never
repainted is blank. This lossy replay reached the client through every path
that started a stream over: FullReset/force upgrades, reconnects, and fresh
loads. Hidden tab + output burst → Lagged/ByteBudget trip → resync storm →
FullReset → corrupted screen; that is the focus-loss correlation. Refresh
didn't help because it replayed the same lossy window.

Dependencies checked: vt100 0.16.2 (latest), tokio 1.53.1 (latest), vte 0.15.0
(latest); portable-pty/axum/tower-http are a minor version behind but are
PTY/HTTP plumbing unrelated to parsing or rendering. The parser was not at
fault.

### Fix (server-only; no client or protocol changes)

- `server/src/session.rs`: session keeps a **mirror `vt100::Parser`** in the
  PTY reader thread (single writer), advancing `mirror.upto` under the same
  lock, after the log append and before the broadcast. Resizes are funneled to
  the mirror via an unbounded channel so its wrap points track the PTY.
- `server/src/replay.rs` (new): `replay_image(mirror_screen, retained)` —
  `mirror.screen().state_diff()` against a parser fed the exact bytes the
  client will have re-parsed (`retained`), prefixed with `ESC[?1049h/l` to
  align alt-screen state, and ended with a CUP that pins the cursor. Plain
  ANSI; repaints the true visible screen no matter where the log window began.
- `server/src/handlers.rs`: on attach — `Reset`, then the retained log (for
  best-effort scrollback), then the image, with `sent_upto` advanced only past
  bytes the image already covers (mirror-first lock ordering keeps the live
  tail contiguous). On FullReset — `Reset` + image, `sent_upto` = `mirror.upto`.
- `ResyncStreak` now counts only resyncs that actually sent bytes, so `UpToDate`
  no-op passes can no longer escalate a healthy client into a restart.

### Verification

- 3 new unit tests in `replay.rs` (cell-exact equality: contents, fg/bg, SGR
  attrs, wide chars, cursor, alt-screen, DECCKM, bracketed paste):
  trimmed-log reconstruction, alternate-screen reconstruction (both preludes),
  and blank FullReset.
- Live E2E (raw-socket WS client): attach = Reset + log + image; input→PTY
  echo; reconnect = Reset + replay. Output-storm stall fired a live FullReset
  and the stream continued correctly (image + live frames, ~1.49 MB drained).
- Browser: Canvas 2D and WebGL2 both paint the prompt against the real server.
- `cargo test -p krust` (32) and `-p terminal-client` (80) green; krust clippy
  clean; only untouched files remain rustfmt-check-failing (pre-existing).