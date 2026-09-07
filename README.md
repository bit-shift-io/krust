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
* **WebGL2 Fallback:** Detects WebGL2 availability and falls back to xterm.js when unavailable.

---

## Quick Start

```bash
# Clone & build
git clone https://github.com/your-username/krust.git
cd krust
wasm-pack build --target web
cargo build --release

# Run
./target/release/krust
```

Open http://localhost:3000 in your browser.

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
│                    Rust Backend                        │
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
├── backend/                # PTY server crate
│   ├── Cargo.toml
│   └── src/main.rs
├── client-wasm/            # WASM client crate
│   ├── Cargo.toml
│   ├── src/lib.rs
│   ├── index.html
│   ├── demo/               # Test/demo pages
│   └── pkg/                # wasm-pack output
└── res/                    # xterm.js fallback assets
```

---

## Development

```bash
# Build WASM client
cd client-wasm && wasm-pack build --target web

# Build & run backend
cargo run --release

# Run tests
cargo test
```