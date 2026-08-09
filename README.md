# krust 🦐

A fast, single-binary web terminal emulator built in Rust using Axum and xterm.js.

> **Name origin:** A blend of **kr**ill (small sea crustaceans) and R**ust**.

---

## Features

* **WebGL Acceleration:** Uses `xterm-addon-webgl` for fast rendering without subpixel line gaps in TUIs.
* **Truecolor Support:** Native support for 24-bit color (`COLORTERM=truecolor`).
* **Smart Zoom:** `Ctrl + MouseWheel` scales font size without breaking TUI mouse tracking.
* **Dynamic Tab Titles:** Updates browser tab names dynamically using OSC escape sequences.
* **Centered Canvas:** Auto-centers character grid while matching the terminal background color.
* **Real-time Resizing:** Handles PTY window updates via `ResizeObserver`.

---

## Quick Start

```bash
# Clone & build
git clone [https://github.com/your-username/krust.git](https://github.com/your-username/krust.git)
cd krust
cargo build --release

# Run
./target/release/krust
```

Open http://localhost:3000 in your browser.
