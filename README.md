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
│   ├── src/lib.rs
│   ├── res/                # HTML pages + test harness
│   └── pkg/                # raw WASM output
```

---

## Development

```bash
# Build & run (build.rs compiles the WASM client into target/wasm/ when stale; then
# server.html, krust_runtime.js and the wasm are embedded into the binary)
cargo run --release

# Run tests
cargo test
```