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

## Keyboard protocol limitations (deliberate)

Unlike the rendering/parsing issues above, these are scoped-out features, not
bugs.

- **Kitty keyboard protocol (`CSI ? u`):** the query is answered with flags
  `0` ("legacy keys only"). `key_to_bytes` maps the classic xterm sequences and
  does not implement progressive enhancement or key event types, so advertising
  anything else would be a lie. TUIs that probe for kitty keys fall back to
  legacy input (or to their `modifyOtherKeys` path).
- **`modifyOtherKeys` (`CSI > 4 ; m`):** unsupported. XTGETTCAP `km` is still
  advertised (matching xterm) so capability probes do not stall, but the mode
  is not tracked and does not alter `key_to_bytes`. Modified keys that rely on
  it (Ctrl+Shift+letter and friends) arrive as their base sequence.

Making either real means tracking the mode in `TerminalState` (the same pattern
used for bracketed paste / focus reporting) and consulting it when encoding a
key press; there is no framework in the way.

## Audit remediation (2026-10): performance, GL, standards

Derived from `AUDIT.md` (see `TASKS.md` phases 0-4). Three themes:

### 1. Paste / live-output stall (critical)

`compute_dirty_cells` ran on every WebSocket frame and cloned the whole
`vt100::Screen` twice (scrollback included) to build a dirty set the WebGL
renderer never reads. At 1 KB PTY reads a 1 MB paste was ~1000 frames ×
~400k cell copies. Fixes: the diff (`diff_screens`) is computed **at render
time** by `DirtyTracker`, the WebGL path skips it entirely, the remaining clone
is avoided with `prev_screen.take()`, the server PTY read buffer is 16 KiB, and
the client paste throttle is gone (one frame per paste). The server also now
blocks on the PTY write lock (`pty_write_locked`) instead of `try_lock`-and-drop.

### 2. WebGL2 renderer waste (high)

Per-frame full-grid rebuild/re-upload, a background quad per cell, per-frame
glyph hashing, and per-frame attribute lookups. Fixes: default-background cells
emit no bg quad and blank cells no text quad; needed codepoints are collected
into a set and `plan_missing` dedups with a set; the 256-entry ASCII UV LUT
skips the range scan; `graphic_cell_rects` returns a fixed-size array instead of
allocating; attribute locations are cached (`AttrLocations`); instance scratch
buffers are reused; the selection `HashSet` is skipped when empty; and only the
changed instance span is re-uploaded with `bufferSubData`.

### 3. Terminal standards (medium)

Added bracketed paste (`?2004`), focus reporting (`?1004`), bell, `OSC 8`
hyperlinks (ctrl/cmd-click opens), `OSC 52` clipboard (opt-in via
`?clipboard=1`), dynamic colors (`OSC 4`/`10`/`11`, with the `OSC 11` query
reply kept consistent), `DECRQM` + `DA3`, and `OSC 7`/`OSC 133` shell
integration. All of these live in `TerminalCallbacks` / small scanners and are
host-unit-tested; the palette overrides are thread-local in `color.rs` and reset
by `reset_terminal`.
