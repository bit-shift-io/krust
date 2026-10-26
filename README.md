# krust 🦐

A fast, single-binary web terminal emulator built in Rust. Spawns a system shell
in a PTY and streams raw bytes over WebSocket to a Rust/WASM client that renders
the terminal on a Canvas 2D surface.

> **Name origin:** A blend of **kr**ill (small sea crustaceans) and R**ust**.

---

## Features

* **Canvas 2D Rendering:** Native Canvas 2D glyph rendering with DPR scaling for crisp text.
* **Truecolor Support:** Native support for 24-bit color (`COLORTERM=truecolor`).
* **VT100 Parser:** ANSI escape sequences parsed client-side via the `vt100` crate compiled to WASM.
* **Smart Zoom:** `Ctrl + MouseWheel` scales font size without breaking TUI mouse tracking.
* **Dynamic Tab Titles:** Updates browser tab names dynamically using OSC escape sequences.
* **WebSocket Binary Pipeline:** Raw PTY bytes streamed as `ArrayBuffer` frames (no JSON framing for output).
* **Backpressure:** Bounded per-client byte budget with coalescing on lag.
* **No wasm-bindgen CLI:** Raw WASM module loaded via `WebAssembly.instantiateStreaming`; offline builds work without network access.

---

## Quick Start

```bash
# Clone & build (build.rs compiles the WASM client into target/wasm/; all assets are then embedded)
git clone https://github.com/your-username/krust.git
cd krust
cargo build --release

# Run (single self-contained binary; no other files needed)
./target/release/krust
```

Open http://localhost:3000 in your browser.

---

## Configuration

krust reads `$XDG_CONFIG_HOME/bitshift/krust/config.json` (falling back to
`~/.config/bitshift/krust/config.json`):

```json
{
  "host": "127.0.0.1",
  "port": 3000,
  "allowed_origins": ["http://localhost:3000", "http://127.0.0.1:3000", "http://localhost:5000"]
}
```

All three fields are optional and default to the values above. **This file is
the only source of `host` and `port`** — the `HOST` and `PORT` environment
variables are deliberately not consulted, because `HOST` is set by default in
some shells and would silently override a file the settings UI had just
written. Grit's settings page maintains `host` for you.

`host` is the security boundary: `127.0.0.1` restricts krust to this machine,
while `0.0.0.0` makes the terminal — a shell — reachable by anything on the
network. With a non-loopback `host`, `allowed_origins` is ignored and every
WebSocket origin is accepted, since there is no meaningful same-origin check
left to make; on loopback the list above is enforced.

---

## Architecture

```
┌────────────────────────────────────────────────────────┐
│                    Browser Client                      │
│  ┌───────────────────────┐   ┌──────────────────────┐  │
│  │ HTML5 Canvas (Rust/WASM) │   │ Transparent DOM Layer│  │
│  │  Canvas 2D Glyph Render  │   │ (Text Selection /    │  │
│  │  (vt100 parser)          │   │  Clipboard Copy/Paste│  │
│  └───────────────────────┘   └──────────────────────/  │
│                                                      │
│  Binary WebSocket (ArrayBuffer)                      │
└───────────────────────▲──────────────────────────────┘
                        │
                        │ Binary frames (VT100 ANSI bytes)
┌──────────────────────────▼─────────────────────────────┐
│                    Rust Server    │
│                                                      │
│   ┌───────────────┐         ┌───────────────────────┐  │
│   │ Axum WebSocket │◄───────►│    portable-pty       │  │
│   └───────────────┘         └───────────────────────┘  │
│                                     │                  │
│                              (Spawns System Shell)     │
└────────────────────────────────────────────────────────┘
```

### Workspace

```
krust/
├── Cargo.toml              # Workspace manifest
├── server/                 # PTY server crate
│   ├── Cargo.toml
│   └── src/main.rs
├── client/                 # WASM client crate
│   ├── Cargo.toml
│   ├── src/                # lib.rs, exports.rs, state.rs, renderer.rs, ffi.rs, …
│   └── res/                # HTML pages, krust_runtime.js, test harness
```

The WebGL2 renderer bakes its glyph atlas with the browser's own Canvas 2D text
engine (the same font stack the 2D fallback paints with), so no font files are
bundled and missing glyphs resolve through the browser's normal font fallback.
Codepoints outside the pre-baked ranges (CJK, emoji, …) are rasterized on demand
into a dynamic atlas region with LRU eviction, so the WebGL2 path covers the
full Unicode range.

---

## Development

```bash
# Build & run (build.rs compiles the WASM client into target/wasm/ when stale; then
# server.html, krust_runtime.js and the wasm are embedded into the binary)
cargo run --release

# Run tests
cargo test
```