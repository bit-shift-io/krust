use axum::{
    extract::ws::{Message, WebSocket, WebSocketUpgrade},
    http::header,
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

// Embed files from the project-root res/ folder
const INDEX_HTML: &str = include_str!("../res/index.html");
const XTERM_CSS: &str = include_str!("../res/xterm.css");
const XTERM_JS: &str = include_str!("../res/xterm.js");
const XTERM_FIT_JS: &str = include_str!("../res/xterm-addon-fit.js");
const XTERM_WEBGL_JS: &str = include_str!("../res/xterm-addon-webgl.js");

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
        .route("/ws", get(ws_handler))
        .route(
            "/res/xterm.css",
            get(|| async { ([(header::CONTENT_TYPE, "text/css")], XTERM_CSS) }),
        )
        .route(
            "/res/xterm.js",
            get(|| async { ([(header::CONTENT_TYPE, "application/javascript")], XTERM_JS) }),
        )
        .route(
            "/res/xterm-addon-fit.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    XTERM_FIT_JS,
                )
            }),
        )
        .route(
            "/res/xterm-addon-webgl.js",
            get(|| async {
                (
                    [(header::CONTENT_TYPE, "application/javascript")],
                    XTERM_WEBGL_JS,
                )
            }),
        );

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Web terminal listening on http://localhost:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
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
