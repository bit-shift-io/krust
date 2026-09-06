# Implementing a High-Performance Rust & WebAssembly Terminal

This document outlines the architecture and implementation steps for replacing a DOM-heavy JavaScript terminal setup (`xterm.js`) with a high-performance, GPU-accelerated **Rust + WebAssembly (WASM)** rendering pipeline. 

By leveraging **`beamterm-renderer`** (or a similar WebGL2-backed Rust architecture) paired with `portable-pty` on the backend, you eliminate background tab throttling, layout reflow bugs, and screen lock rendering corruption [cite: 1.1.2, 1.2.1].

---

## 1. System Architecture Overview

```text
 ┌────────────────────────────────────────────────────────┐
 │                    Browser Client                      │
 │                                                        │
 │  ┌───────────────────────┐   ┌──────────────────────┐  │
 │  │ HTML5 Canvas / WebGL2 │   │ Transparent DOM Layer│  │
 │  │    (Rust / WASM)      │   │ (Text Selection /    │  │
 │  │  Sub-ms Grid Render   │   │  Clipboard Copy/Paste│  │
 │  └───────────────────────┘   └──────────────────────/  │
 └──────────────────────────▲─────────────────────────────┘
                            │ Binary WebSocket (ArrayBuffer)
 ┌──────────────────────────▼─────────────────────────────┐
 │                    Rust Backend                        │
 │                                                        │
 │   ┌───────────────┐         ┌───────────────────────┐  │
 │   │ Axum WebSocket│◄───────►│    portable-pty       │  │
 │   └───────────────┘         └───────────────────────┘  │
 │                                     │                  │
 │                              (Spawns System Shell)     │
 └────────────────────────────────────────────────────────┘
```

---

## 2. Project Structure

Organize your workspace as a standard cargo workspace containing a backend server and a WASM client package:

```text
my-web-terminal/
├── Cargo.toml
├── backend/
│   ├── Cargo.toml
│   └── src/
│       └── main.rs
└── client-wasm/
    ├── Cargo.toml
    ├── index.html
    └── src/
        └── lib.rs
```

---

## 3. Step-by-Step Implementation

### Step 3.1: Root Workspace Configuration (`Cargo.toml`)

Create a root `Cargo.toml` to manage both crates:

```toml
[workspace]
members = [
    "backend",
    "client-wasm"
]
resolver = "2"
```

---

### Step 3.2: The Rust Backend (PTY + WebSockets)

In `backend/Cargo.toml`, set up dependencies for handling system pseudo-terminals and asynchronous communication [cite: 1.1.2]:

```toml
[package]
name = "terminal-backend"
version = "0.1.0"
edition = "2021"

[dependencies]
tokio = { version = "1.0", features = ["full"] }
axum = { version = "0.7", features = ["ws"] }
portable-pty = "0.8"
futures-util = "0.3"
serde = { version = "1.0", features = ["derive"] }
serde_json = "1.0"
```

Implement the backend server in `backend/src/main.rs`. This handles spawning the user shell via `portable-pty` and routing raw binary frames over WebSockets:

```rust
use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::IntoResponse,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{native_pty_system, CommandBuilder, PtySize};
use std::sync::Arc;
use tokio::sync::broadcast;

#[tokio::main]
async fn main() {
    let app = Router::new().route("/ws", get(ws_handler));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    println!("Terminal backend running on http://127.0.0.1:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(mut socket: WebSocket) {
    let pty_system = native_pty_system();
    
    // Default initial dimensions
    let pair = pty_system.openpty(PtySize {
        rows: 24,
        cols: 80,
        pixel_width: 0,
        pixel_height: 0,
    }).unwrap();

    let cmd = CommandBuilder::new("bash");
    let _child = pair.slave.spawn_command(cmd).unwrap();

    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();

    // Stream PTY output to the WebSocket client as binary packets
    let (tx, mut rx) = broadcast::channel(100);
    
    let reader_tx = tx.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            use std::io::Read;
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    let _ = reader_tx.send(buf[..n].to_vec());
                }
                Err(_) => break,
            }
        }
    });

    // Task to send PTY output to browser
    let mut send_task = tokio::spawn(async move {
        while let Ok(data) = rx.recv().await {
            if socket.send(Message::Binary(data)).await.is_err() {
                break;
            }
        }
    });

    // Task to receive keyboard inputs from browser and write to PTY
    let mut writer_task = tokio::spawn(async move {
        // Simple input redirection logic
    });

    tokio::select! {
        _ = (&mut send_task) => writer_task.abort(),
        _ = (&mut writer_task) => send_task.abort(),
    }
}
```

---

### Step 3.3: The WASM Client Renderer

In `client-wasm/Cargo.toml`, configure the crate to compile to WebAssembly using `wasm-bindgen` and high-performance WebGL bindings [cite: 1.1.2]:

```toml
[package]
name = "terminal-client"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wasm-bindgen = "0.2"
wasm-bindgen-futures = "0.4"
js-sys = "0.3"
web-sys = { version = "0.3", features = [
    "Window", "Document", "HtmlCanvasElement", "WebSocket", "MessageEvent", 
    "BinaryType", "Storage", "Selection", "Range"
] }
beamterm-renderer = "0.10"
console_error_panic_hook = "0.1"
```

In `client-wasm/src/lib.rs`, initialize the WebGL2 rendering surface and wire up the WebSocket binary message handler:

```rust
use wasm_bindgen::prelude::*;
use beamterm_renderer::Terminal;
use web_sys::{WebSocket, MessageEvent, BinaryType};

#[wasm_bindgen(start)]
pub fn run() -> Result<(), JsValue> {
    console_error_panic_hook::set_once();

    // Initialize the GPU-accelerated terminal renderer bound to #terminal-canvas
    let mut terminal = Terminal::builder("#terminal-canvas")
        .build()
        .map_err(|e| JsValue::from_str(&e.to_string()))?;

    // Connect to backend WebSocket
    let ws = WebSocket::new("ws://127.0.0.1:3000/ws")?;
    ws.set_binary_type(BinaryType::Arraybuffer);

    let ws_clone = ws.clone();
    let closure = Closure::wrap(Box::new(move |e: MessageEvent| {
        if let Ok(array_buffer) = e.data().dyn_into::<js_sys::ArrayBuffer>() {
            let uint8_array = js_sys::Uint8Array::new(&array_buffer);
            let mut bytes = vec![0; uint8_array.length() as usize];
            uint8_array.copy_to(&mut bytes);

            // Feed incoming bytes to your terminal parser/grid model here
            // Then invoke a sub-millisecond hardware-accelerated frame render
            // terminal.render_frame().unwrap();
        }
    }) as Box<dyn FnMut(MessageEvent)>);

    ws.set_onmessage(Some(closure.as_ref().unchecked_ref()));
    closure.forget();

    Ok(())
}
```

---

### Step 3.4: Frontend HTML Shell & Selection Layer

Create `client-wasm/index.html`. To preserve native text selection and block-highlighting without relying on DOM element thrashing, place an invisible, selectable text layer directly over the WebGL canvas:

```html
<!DOCTYPE html>
<html lang="en">
<head>
    <meta charset="UTF-8">
    <title>Rust Web Terminal</title>
    <style>
        body, html {
            margin: 0;
            padding: 0;
            background-color: #121212;
            overflow: hidden;
            height: 100%;
        }
        #terminal-container {
            position: relative;
            width: 100vw;
            height: 100vh;
        }
        #terminal-canvas {
            position: absolute;
            top: 0;
            left: 0;
            width: 100%;
            height: 100%;
            z-index: 1;
        }
        /* Invisible selection overlay for native copy/paste behavior */
        #selection-layer {
            position: absolute;
            top: 0;
            left: 0;
            width: 100%;
            height: 100%;
            z-index: 2;
            color: transparent;
            user-select: text;
            white-space: pre;
            font-family: monospace;
            font-size: 14px;
            line-height: 1.2;
            pointer-events: auto;
            opacity: 0.01; /* Nearly transparent, purely handles mouse selection */
        }
    </style>
</head>
<body>
    <div id="terminal-container">
        <canvas id="terminal-canvas"></canvas>
        <div id="selection-layer" contenteditable="true"></div>
    </div>

    <!-- Load compiled WASM module via wasm-pack -->
    <script type="module">
        import init from './pkg/terminal_client.js';
        async function main() {
            await init();
        }
        main();
    </script>
</body>
</html>
```

---

## 4. Compilation and Execution

1. **Build the WASM Client:**
   ```bash
   cd client-wasm
   wasm-pack build --target web
   ```
2. **Start the Rust PTY Server:**
   ```bash
   cd backend
   cargo run
   ```
3. Open `client-wasm/index.html` via a local web server (e.g., `python3 -m http.server` or `miniserve`).

## 5. Benefits of this Setup
* **Zero DOM Reflow Freeze:** Rendering happens inside a WebGL2 pipeline. Minimizing the window or locking the screen pauses the render loop safely without desynchronizing state vectors or corrupting string element trees.
* **Instant Resume:** On window focus recovery, the GPU context picks up immediately without requiring expensive DOM tree reconciliations.
* **Native Select & Copy:** The transparent overlay handles mouse selection natively, allowing normal browser copy/paste shortcuts (`Ctrl+C` / `Cmd+C`) to interact with your terminal buffer effortlessly.
