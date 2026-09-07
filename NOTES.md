# Notes: Krust Terminal Refactor — Replace xterm.js with Rust/WASM WebGL2 Pipeline

## Decision: Replace xterm.js with Home-Grown Rust/WASM Terminal

**Why:** xterm.js has structural DOM bugs that are hard to work around (mouse tracking in TUIs, IME composition failures, scrollback corruption, tab-throttling freezes, selection overlay race conditions). The Rust/WASM Canvas 2D pipeline eliminates DOM reflow, gives sub-millisecond frame rendering, and provides native copy/paste via a transparent overlay.

**When this is worth it:** Your team has WebGL2 + VT100 parsing + PTY experience, you're hitting xterm.js bugs without clean workarounds, and you need long-term maintainability without JS dependency upgrades.

**When this is NOT worth it:** Curiosity project, tight deadlines, no WebGL/PTY expertise, or xterm.js bugs have valid plugin/workaround paths.

---

## 1. Architecture Overview

```text
┌────────────────────────────────────────────────────────┐
│                    Browser Client                      │
│                                                      │
│  ┌───────────────────────┐   ┌──────────────────────┐  │
│  │ HTML5 Canvas (Rust/WASM) │   │ Transparent DOM Layer│  │
│  │  Canvas 2D Glyph Render│   │ (Text Selection /    │  │
│  │  (vt100 parser)        │   │  Clipboard Copy/Paste│  │
│  └───────────────────────┘   └──────────────────────/  │
│                                                      │
│  Binary WebSocket (ArrayBuffer)                      │
└───────────────────────▲──────────────────────────────┘
                        │
                        │ Binary frames (VT100 ANSI bytes)
┌──────────────────────────▼─────────────────────────────┐
│                    Rust Backend                        │
│                                                      │
│   ┌───────────────┐         ┌───────────────────────┐  │
│   │ Axum WebSocket │◄───────►│    portable-pty       │  │
│   └───────────────┘         └───────────────────────┘  │
│                                     │                  │
│                              (Spawns System Shell)     │
└────────────────────────────────────────────────────────┘
```

---

## 2. Key Design Decisions

### 2.1 Transparent Overlay + Selection Mechanics

- **Default: `pointer-events: none`** on the invisible selection overlay — all mouse events pass through to the canvas, preserving TUI mouse tracking (htop, vim).
- **Shift-drag toggles `pointer-events: auto`** — when user holds Shift and drags, the overlay captures for native text selection. Without Shift, canvas handles mouse events and translates to VT100 escape sequences.
- **IME/candidate windows** anchor correctly to the hidden caret because the overlay is `contenteditable="true"` with `opacity: 0.01`.
- **Keypress events** intercepted in capture phase by WASM listener — prevents ghost text in DOM while allowing native composition sequences to feed characters into the terminal event handler.

### 2.2 WebSocket Binary Framing

- **Strategy: Streaming VT100 parser + requestAnimationFrame flush**
  - Backend streams raw PTY bytes over WS as un-framed `Message::Binary(ArrayBuffer)` chunks
  - Frontend feeds every incoming chunk byte-by-byte into a **stateful VT100 parser** (e.g., `vt100` crate compiled to WASM)
  - Parser updates internal cell matrix continuously
  - `requestAnimationFrame` loop flushes the grid to the WebGL canvas at a locked 60 FPS, independent of network chunk boundaries
  - **No explicit "frames" needed** — the parser is the frame delimiter; it consumes ANSI sequences byte-by-byte

- **Alternative: Length-prefixed headers** (if you need explicit message boundaries)
  - 4-byte big-endian length prefix + payload
  - Simpler for the backend but adds 4 bytes per chunk — not necessary if using a streaming parser

### 2.3 VT100 Parser Location: WASM Client (Not Backend)

- **Backend remains a thin router**: `portable-pty` → raw ANSI bytes → WebSocket
- **No grid state serialization** over WS — keeps network payloads tiny
- **WASM client** compiles a VT100 parser (e.g., `vt100` crate or custom state machine) that consumes raw bytes and maintains the cell matrix
- `beamterm-renderer` receives the populated cell matrix and renders a single instanced WebGL draw call

### 2.4 Terminal Resizing

- **Browser → WS → Rust**: On window resize, JS calculates new cols/rows from pixel dimensions, sends a JSON message `{"type": "resize", "cols": N, "rows": M}` over WebSocket
- **Rust**: Intercepts the message and calls `pair.master.resize(PtySize { cols, rows, ... })`, triggering `SIGWINCH` to the shell
- **Fixed initial dimensions**: Open with 24×80, dynamic on resize

### 2.5 Input Pipeline & Modifier Mapping

| Browser Key | PTY Output |
|---|---|
| Standard keys (a-z, 0-9) | The character itself |
| `Ctrl + C` | `\x03` (ETX) |
| Arrow keys | `\x1b[A` (Up), `\x1b[B` (Down), `\x1b[C` (Right), `\x1b[D` (Left) |
| `Ctrl + Arrow` | `\x1b` + directional char (some implementations) |
| `Alt/Option + char` | `\x1b` + character (ESC prefix) |
| `Fn` keys / F1-F12 | Varies; typically `\x1b[11~` through `\x1b[21~` |
| `Home` | `\x1b[H` |
| `End` | `\x1b[F` |
| `PageUp` | `\x1b[5~` |
| `PageDown` | `\x1b[6~` |

- **Local echo: DISABLED** — backend PTY configures raw terminal (ICANON/ECHO off). Shell is solely responsible for echoing. Characters typed by the user go directly to the shell; the shell echoes them back through the PTY read loop.

### 2.6 Large Pastes & Backpressure

- **Paste handling**: User pastes N lines into the `contenteditable` overlay. The WASM client extracts the full text string, chunks it if needed, and writes it as a single batch payload over WebSocket (not individual keystrokes).
- **PTY buffer overflow prevention**: Large pastes are throttled with small pacing intervals, or flow-controlled via XON/XOFF if the target app supports it. The server's broadcast channel has a bounded buffer — if the client lags, `RecvError::Lagged` is returned and intermediate frames are dropped/coalesced.
- **contenteditable cleanup**: After paste is read and sent to PTY, immediately clear `element.innerHTML = ""` to prevent memory bloat from accumulating paste history in invisible DOM nodes.

### 2.7 Backpressure & Bounded Channels

- **Tokio `broadcast::channel(100)`**: Fixed ring of 100 message allocations (not unbounded). If a receiver lags, `RecvError::Lagged` is returned.
- **Production approach**: Use a coalescing byte buffer with a strict memory threshold (e.g., 1MB per client). When the PTY outputs faster than the WebSocket can flush:
  1. Server accumulates unread bytes into a bounded ring buffer
  2. If socket send queue backs up past the threshold, drop/stale intermediate frames
  3. When the socket clears, immediately flush the latest absolute screen state rather than replaying thousands of stale frames

### 2.8 Fallbacks & Graceful Degradation

- **WebGL2 unavailable** (legacy browsers, constrained mobile, disabled hardware acceleration): The system **fails hard** — the WASM renderer has no 2D Canvas fallback built in.
- **Fallback strategy**: For environments lacking WebGL2, fall back to a standard DOM/Canvas 2D renderer (or an optimized `xterm.js` configuration with its DOM renderer enabled). True headless/non-WebGL environments cannot sustain sub-millisecond cell rendering via the DOM without severe lag during heavy TUI workloads.
- **Feature-flag the new renderer**: Keep xterm.js as a fallback while the new path is proven. Don't remove xterm.js until the Rust/WASM approach is validated in production.

---

## 3. Migration Path from Current krust

**The existing `krust` codebase has been migrated:**

- xterm.js resources removed from `src/main.rs`
- WASM client (`client-wasm/`) is the primary terminal rendering path
- Backend uses PTY + binary WebSocket pipeline (not JSON-based xterm.js messages)
- xterm.js fallback assets kept in `res/` for environments without WebGL2

**Migration steps (already completed):**

1. ✅ **Prototype the WASM client first** — get `vt100` parser + Canvas 2D renderer running
2. ✅ **Feature-flag the new renderer**: Add config/env flag to switch between xterm.js and Rust/WASM paths; xterm.js kept as fallback only
3. ✅ **Strip xterm.js resources** from `src/main.rs` — removed `include_str!` lines for xterm.js/addons
4. ✅ **Reorganize Cargo workspace** — current workspace with `backend` + `client-wasm` crates
5. ✅ **Migrate the selection overlay** from the xterm.js DOM approach to the transparent overlay design
6. ✅ **Implement the VT100 parser** on the WASM client — using `vt100` crate compiled to WASM
7. ✅ **Wire up the WebSocket binary pipeline** — replace JSON messages with raw binary frames
8. ✅ **Implement resize handling** through the new JSON resize messages over WS
9. ✅ **Implement input pipeline** — keyboard event → PTY raw bytes mapping via `key_to_bytes` API
10. ✅ **Test and benchmark** — FPS, memory usage, bug reduction

---

## 4. Dependencies Overview

### Backend (`backend/Cargo.toml`)
- `tokio` (full) — async runtime
- `axum` (ws) — WebSocket server
- `portable-pty` — PTY management
- `futures-util` — stream/sink utilities
- `serde` + `serde_json` — JSON for resize/control messages
- `tower-http` + `CorsLayer` — CORS enabled on all routes to allow cross-origin fetch from Grit web UI (`localhost:5000` → `localhost:3000`)

### WASM Client (`client-wasm/Cargo.toml`)
- `wasm-bindgen` — JS interop
- `wasm-bindgen-futures` — async WASM utilities
- `web-sys` — Web APIs (WebSocket, Canvas, etc.)
- `js-sys` — JS system types
- `vt100` — ANSI state machine (parsing layer)
- `console-error-panic-hook` — panic handling in WASM

### Selection Overlay (HTML/CSS)
- Invisible `div` with `position: absolute`, `user-select: text`, `pointer-events: auto/none` (toggled via Shift modifier)
- `opacity: 0.01` — nearly transparent, handles mouse selection natively
- `contenteditable="true"` — enables native browser copy/paste with IME support

---

## 5. Open Questions & Decisions

| Question | Decision/Status |
|---|---|
| VT100 parser crate | Using `vt100` crate compiled to WASM |
| beamterm-renderer vs custom WebGL2 | Using Canvas 2D with `vt100` parser (beamterm-renderer kept as optional future upgrade) |
| Backpressure mechanism | Bounded ring buffer + coalescing (not unlimited broadcast) |
| Fallback strategy | Feature-flag xterm.js fallback; degrade to DOM renderer if WebGL2 absent |
| Shift-modifier selection | Implementing — default `pointer-events: none`, toggle on Shift-drag |
| Initial buffer on connect | Fresh shell on new connection; server replay for persistent/tmux sessions |
| Large paste throttling | Pacing intervals or XON/XOFF flow control |
| Resize handling | JSON resize messages over WS → `pair.master.resize()` + SIGWINCH |

---

## 6. Success Metrics

- **FPS**: Locked 60 FPS (~16.6ms/frame), render loop < 1ms even for 40k+ cell grids
- **Memory**: No DOM reflow freezes; tab switch pause < 100ms (GPU context preserves state)
- **Copy/paste**: Native browser copy/paste works without modifier hacks; IME composition anchors correctly
- **Bug reduction**: Eliminate xterm.js DOM-related bugs (mouse tracking, scrollback corruption, tab throttling)
- **Startup time**: WASM module compiles and renders within 200ms of page load

---

## 7. Helpful Links

- `backend/src/main.rs` — PTY server main file
- `client-wasm/src/lib.rs` — WASM client library
- `client-wasm/index.html` — minimal WASM client HTML page
- `AUDIT.md` — latest codebase audit report