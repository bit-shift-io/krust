use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    response::Html,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, NativePtySystem, PtySize, PtySystem};
use serde::Deserialize;
use std::{
    io::{Read, Write},
    sync::Arc,
};
use tokio::sync::{mpsc, Mutex};

#[derive(Deserialize)]
#[serde(tag = "type")]
enum ClientMessage {
    Input {
        data: String,
    },
    Resize {
        cols: u16,
        rows: u16,
        pixel_width: u16,
        pixel_height: u16,
    },
}

#[tokio::main]
async fn main() {
    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler));

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Web terminal listening on http://localhost:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Html<&'static str> {
    Html(
        r#"<!DOCTYPE html>
<html>
<head>
    <meta charset="utf-8" />
    <!-- Terminal Tab Icon -->
    <link rel="icon" type="image/svg+xml" href="data:image/svg+xml,%3Csvg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 32 32'%3E%3Crect width='32' height='32' rx='6' fill='%232b2b2b'/%3E%3Cpath d='M8 10L14 16L8 22' stroke='%23f0f0f0' stroke-width='2.5' stroke-linecap='round' stroke-linejoin='round' fill='none'/%3E%3Cline x1='16' y1='22' x2='24' y2='22' stroke='%23f0f0f0' stroke-width='2.5' stroke-linecap='round'/%3E%3C/svg%3E">
    <link rel="stylesheet" href="https://cdn.jsdelivr.net/npm/xterm@5.3.0/css/xterm.css" />
    <script src="https://cdn.jsdelivr.net/npm/xterm@5.3.0/lib/xterm.js"></script>
    <script src="https://cdn.jsdelivr.net/npm/xterm-addon-fit@0.8.0/lib/xterm-addon-fit.js"></script>
    <script src="https://cdn.jsdelivr.net/npm/xterm-addon-webgl@0.15.0/lib/xterm-addon-webgl.js"></script>
    <style>
        * { box-sizing: border-box; }
        body, html {
            margin: 0;
            padding: 0;
            height: 100vh;
            width: 100vw;
            background: #2b2b2b;
            overflow: hidden;
        }
        #terminal {
            width: 100vw;
            height: 100vh;
            background: #2b2b2b;
            display: flex;
            justify-content: center;
            align-items: center;
        }
        /* Ensure xterm containers inherit the theme background for uniform padding */
        .xterm, .xterm-viewport {
            background-color: inherit !important;
        }
        /* Center the character screen canvas within the viewport */
        .xterm-screen {
            margin: auto !important;
        }
    </style>
</head>
<body>
    <div id="terminal"></div>
    <script>
        const theme = {
            background: '#2b2b2b',
            foreground: '#f0f0f0',
            cursor: '#f0f0f0',
            selectionBackground: 'rgba(255, 255, 255, 0.3)'
        };

        const term = new Terminal({
            cursorBlink: true,
            fontSize: 14,
            lineHeight: 1,
            letterSpacing: 0,
            fontFamily: 'JetBrains Mono, Fira Code, monospace',
            theme: theme
        });

        const fitAddon = new FitAddon.FitAddon();
        term.loadAddon(fitAddon);

        const container = document.getElementById('terminal');
        term.open(container);

        try {
            const webglAddon = new WebglAddon.WebglAddon();
            webglAddon.onContextLoss(() => {
                webglAddon.dispose();
            });
            term.loadAddon(webglAddon);
        } catch (e) {
            console.warn("WebGL initialization failed, falling back to standard canvas", e);
        }

        fitAddon.fit();

        term.onTitleChange(title => {
            document.title = title;
        });

        const socket = new WebSocket(`ws://${location.host}/ws`);

        socket.onopen = () => {
            sendSize();
            term.onData(data => socket.send(JSON.stringify({ type: "Input", data })));
        };

        socket.onmessage = (e) => term.write(e.data);

        function sendSize() {
            if (socket.readyState === WebSocket.OPEN) {
                const core = term._core;
                const cellWidth = core._renderService?.dimensions?.actualCellWidth || 9;
                const cellHeight = core._renderService?.dimensions?.actualCellHeight || 17;

                socket.send(JSON.stringify({
                    type: "Resize",
                    cols: term.cols,
                    rows: term.rows,
                    pixel_width: Math.floor(term.cols * cellWidth),
                    pixel_height: Math.floor(term.rows * cellHeight)
                }));
            }
        }

        const resizeObserver = new ResizeObserver(() => {
            fitAddon.fit();
            sendSize();
        });
        resizeObserver.observe(container);

        window.addEventListener('wheel', (e) => {
            if (e.ctrlKey) {
                e.preventDefault();
                e.stopImmediatePropagation();

                let currentSize = term.options.fontSize || 14;
                let newSize = e.deltaY < 0 ? currentSize + 1 : currentSize - 1;

                newSize = Math.max(8, Math.min(32, newSize));

                if (newSize !== currentSize) {
                    term.options.fontSize = newSize;
                    fitAddon.fit();
                    sendSize();
                }
            }
        }, { capture: true, passive: false });
    </script>
</body>
</html>"#,
    )
}

async fn ws_handler(ws: WebSocketUpgrade) -> impl axum::response::IntoResponse {
    ws.on_upgrade(handle_socket)
}

async fn handle_socket(socket: WebSocket) {
    let pty_system = NativePtySystem::default();
    let pair = pty_system
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .expect("Failed to create PTY");

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
    let mut cmd = CommandBuilder::new(shell);
    cmd.env("TERM", "xterm-256color");
    cmd.env("COLORTERM", "truecolor");

    let mut child = pair
        .slave
        .spawn_command(cmd)
        .expect("Failed to spawn shell");

    let writer = pair
        .master
        .take_writer()
        .expect("Failed to take PTY writer");
    let mut reader = pair
        .master
        .try_clone_reader()
        .expect("Failed to clone PTY reader");

    let writer = Arc::new(Mutex::new(writer));
    let master = Arc::new(Mutex::new(pair.master));

    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);

    tokio::task::spawn_blocking(move || {
        let mut buffer = [0u8; 1024];
        while let Ok(n) = reader.read(&mut buffer) {
            if n == 0 || tx.blocking_send(buffer[..n].to_vec()).is_err() {
                break;
            }
        }
    });

    let pty_read_task = tokio::spawn(async move {
        while let Some(bytes) = rx.recv().await {
            if ws_sender
                .send(Message::Text(String::from_utf8_lossy(&bytes).to_string()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    let ws_recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            if let Message::Text(text) = msg {
                if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                    match client_msg {
                        ClientMessage::Input { data } => {
                            let writer = writer.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Ok(mut w) = writer.try_lock() {
                                    let _ = w.write_all(data.as_bytes());
                                    let _ = w.flush();
                                }
                            })
                            .await;
                        }
                        ClientMessage::Resize {
                            cols,
                            rows,
                            pixel_width,
                            pixel_height,
                        } => {
                            let master = master.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Ok(m) = master.try_lock() {
                                    let _ = m.resize(PtySize {
                                        rows,
                                        cols,
                                        pixel_width,
                                        pixel_height,
                                    });
                                }
                            })
                            .await;
                        }
                    }
                }
            }
        }
    });

    tokio::select! {
        _ = pty_read_task => {},
        _ = ws_recv_task => {},
    }

    let _ = child.kill();
}
