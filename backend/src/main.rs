use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::header,
    routing::get,
    Router,
};
use futures_util::{SinkExt, StreamExt};
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use serde::Deserialize;
use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::Arc,
};
use tokio::sync::{broadcast, Mutex, RwLock};
use tower_http::cors::CorsLayer;

const INDEX_HTML: &str = include_str!("../../client-wasm/demo/backend.html");

const MAX_HISTORY_BYTES: usize = 1024 * 512; // Keep 512 KB scrollback buffer per session
const BINARY_FRAME_MAX: usize = 16 * 1024; // Max bytes per WS binary frame

/// Wrap raw PTY bytes as a single binary WebSocket frame.
fn binary_frame(bytes: Vec<u8>) -> Message {
    Message::Binary(bytes)
}

/// Strict per-client output budget before backpressure kicks in.
const MAX_PENDING_BYTES: usize = 1024 * 1024;

/// Tracks how many bytes a single WebSocket client has queued since its last
/// drain. Returns `false` when the client has exceeded the budget, at which
/// point the caller should drop stale frames and flush the latest screen
/// state (a full history replay) instead of forwarding the overflow.
#[derive(Default)]
struct ByteBudget {
    pending: usize,
}

impl ByteBudget {
    fn accept(&mut self, len: usize) -> bool {
        if self.pending + len > MAX_PENDING_BYTES {
            self.pending = 0;
            return false;
        }
        self.pending += len;
        true
    }
}

/// Split `bytes` into binary frames no larger than [`BINARY_FRAME_MAX`].
///
/// History replays can approach 512 KB; chunking keeps individual WS
/// frames small and avoids hitting the tungstenite max-frame limit.
fn binary_chunks(bytes: Vec<u8>) -> Vec<Message> {
    bytes
        .chunks(BINARY_FRAME_MAX)
        .map(|c| Message::Binary(c.to_vec()))
        .collect()
}

/// Write raw bytes to a PTY master writer and flush.
///
/// Input travels from the WASM client to the shell as raw bytes (the client
/// encodes keystrokes; the PTY is the echo source, so there is no local echo).
fn pty_write(w: &mut dyn Write, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}

/// Build a [`PtySize`] from client-reported pixel dimensions.
fn pty_size(cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width,
        pixel_height,
    }
}

/// Decrement a session's connection count; returns `true` when the number of
/// remaining connections hit zero and the session should be cleaned up.
fn drop_connection(connections: &std::sync::atomic::AtomicUsize) -> bool {
    connections.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1
}

struct Session {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    tx: broadcast::Sender<Vec<u8>>,
    history: Arc<Mutex<Vec<u8>>>,
    connections: std::sync::atomic::AtomicUsize,
}

#[derive(Clone)]
struct AppState {
    sessions: Arc<RwLock<HashMap<String, Arc<Session>>>>,
}

#[derive(Deserialize)]
struct WsQuery {
    s: Option<String>,
}

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
    let state = AppState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .route(
            "/pkg/terminal_client.js",
            get(|| serve_pkg_file("terminal_client.js", "application/javascript")),
        )
        .route(
            "/pkg/terminal_client_bg.wasm",
            get(|| serve_pkg_file("terminal_client_bg.wasm", "application/wasm")),
        )
        .layer(CorsLayer::permissive());
    let app = app.with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("Web terminal listening on http://localhost:{}", port);
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> ([(header::HeaderName, &'static str); 2], &'static str) {
    index_response(INDEX_HTML)
}

fn index_response(body: &'static str) -> ([(header::HeaderName, &'static str); 2], &'static str) {
    (
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        body,
    )
}

/// Serve a file from the built WASM package directory at runtime.
///
/// The package dir defaults to `<crate>/../client-wasm/pkg` (i.e. the
/// workspace layout); override with `KRUST_PKG_DIR`.
fn pkg_dir() -> String {
    if let Ok(d) = std::env::var("KRUST_PKG_DIR") {
        return d;
    }
    if let Ok(manifest) = std::env::var("CARGO_MANIFEST_DIR") {
        return std::path::Path::new(&manifest)
            .join("../client-wasm/pkg")
            .display()
            .to_string();
    }
    "client-wasm/pkg".to_string()
}

async fn serve_pkg_file(
    file: &'static str,
    content_type: &'static str,
) -> Result<([(header::HeaderName, &'static str); 2], Vec<u8>), axum::http::StatusCode> {
    let path = std::path::Path::new(&pkg_dir()).join(file);
    let bytes = tokio::fs::read(&path)
        .await
        .map_err(|_| axum::http::StatusCode::NOT_FOUND)?;
    Ok((
        [
            (header::CONTENT_TYPE, content_type),
            (header::CACHE_CONTROL, "no-store"),
        ],
        bytes,
    ))
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<AppState>,
) -> impl axum::response::IntoResponse {
    let session_id = query
        .s
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());

    ws.on_upgrade(move |socket| handle_socket(socket, state, session_id))
}

async fn get_or_create_session(state: &AppState, session_id: &str) -> Arc<Session> {
    // Check if session already exists
    {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(session_id) {
            return session.clone();
        }
    }

    // Session doesn't exist, spawn a new PTY process
    let mut sessions = state.sessions.write().await;
    if let Some(session) = sessions.get(session_id) {
        return session.clone();
    }

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

    let _child = pair
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

    let (tx, _rx) = broadcast::channel::<Vec<u8>>(512);

    // Background thread reading from PTY output -> broadcasting & storing history
    let tx_clone = tx.clone();
    let history = Arc::new(Mutex::new(Vec::new()));
    let history_clone = history.clone();

    tokio::task::spawn_blocking(move || {
        let mut buffer = [0u8; 1024];
        while let Ok(n) = reader.read(&mut buffer) {
            if n == 0 {
                break;
            }
            let data = buffer[..n].to_vec();

            // 1. Maintain scrollback buffer in memory
            if let Ok(mut hist) = history_clone.try_lock() {
                hist.extend_from_slice(&data);
                if hist.len() > MAX_HISTORY_BYTES {
                    let drain_len = hist.len() - MAX_HISTORY_BYTES;
                    hist.drain(0..drain_len);
                }
            }

            // 2. Broadcast output to active WebSocket listeners.
            // Fire-and-forget send: if receivers lag, the broadcast channel
            // drops the frame (bounded at 512) and recv() reports Lagged,
            // which clients coalesce against the last history state.
            let _ = tx_clone.send(data);
        }
    });

    let session = Arc::new(Session {
        writer: Arc::new(Mutex::new(writer)),
        master: Arc::new(Mutex::new(pair.master)),
        tx,
        history,
        connections: std::sync::atomic::AtomicUsize::new(0),
    });

    sessions.insert(session_id.to_string(), session.clone());
    session
}

async fn handle_socket(socket: WebSocket, state: AppState, session_id: String) {
    let session = get_or_create_session(&state, &session_id).await;
    session.connections.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // 1. Re-sync scrollback history to the newly connected WebSocket
    {
        let hist = session.history.lock().await;
        if !hist.is_empty() {
            for frame in binary_chunks(hist.clone()) {
                if ws_sender.send(frame).await.is_err() {
                    return;
                }
            }
        }
    }

    // Subscribe to ongoing PTY output stream
    let mut pty_rx = session.tx.subscribe();

    // 2. Task: PTY output -> WebSocket
    let history = session.history.clone();
    let _sid_out = session_id.clone();
    let pty_read_task = tokio::spawn(async move {
        let mut budget = ByteBudget::default();
        loop {
            let frame = match pty_rx.recv().await {
                Ok(bytes) => bytes,
                // Consumer fell behind the bounded channel: reset the budget
                // and keep going (recv() re-syncs to the latest item).
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    budget.pending = 0;
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };
            // eprintln!("[out:{}] {} bytes", sid_out, frame.len());
            if !budget.accept(frame.len()) {
                // Budget exceeded: drop stale frames and flush the latest
                // screen state (a full history replay) once the channel drains.
                while let Ok(_stale) = pty_rx.try_recv() {}
                let hist = history.lock().await.clone();
                if !hist.is_empty() {
                    for chunk in binary_chunks(hist) {
                        if ws_sender.send(chunk).await.is_err() {
                            return;
                        }
                    }
                }
                continue;
            }
            if ws_sender.send(binary_frame(frame)).await.is_err() {
                break;
            }
        }
    });

    // 3. Task: WebSocket input -> PTY writer & resize handlers
    let writer = session.writer.clone();
    let master = session.master.clone();
    let _sid = session_id.clone();

    let ws_recv_task = tokio::spawn(async move {
        while let Some(Ok(msg)) = ws_receiver.next().await {
            if let Message::Text(text) = msg {
                if let Ok(client_msg) = serde_json::from_str::<ClientMessage>(&text) {
                    match client_msg {
                        ClientMessage::Input { data } => {
                            let writer = writer.clone();
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Ok(mut w) = writer.try_lock() {
                                    let _ = pty_write(&mut *w, data.as_bytes());
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
                            let size = pty_size(cols, rows, pixel_width, pixel_height);
                            let _ = tokio::task::spawn_blocking(move || {
                                if let Ok(m) = master.try_lock() {
                                    let _ = m.resize(size);
                                }
                            })
                            .await;
                        }
                    }
                }
            } else if let Message::Binary(bytes) = msg {
                // Raw PTY input: no JSON framing, bytes go straight to the shell.
                let writer = writer.clone();
                let _ = tokio::task::spawn_blocking(move || {
                    if let Ok(mut w) = writer.try_lock() {
                        let _ = pty_write(&mut *w, &bytes);
                    }
                })
                .await;
            }
        }
    });

    tokio::select! {
        _ = pty_read_task => {},
        _ = ws_recv_task => {},
    }

    // Release this client. When the last client disconnects, drop the session
    // (closing the PTY master fds, which SIGHUPs the shell) and remove it
    // from the map so a fresh session is spawned on the next connect.
    if drop_connection(&session.connections) {
        let mut sessions = state.sessions.write().await;
        if sessions.get(&session_id).map(|s| Arc::ptr_eq(s, &session)).unwrap_or(false) {
            sessions.remove(&session_id);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::AtomicUsize;
    use tower::util::ServiceExt;

    #[test]
    fn parses_input_message() {
        let msg: ClientMessage = serde_json::from_value(json!({"type": "Input", "data": "\x03"})).unwrap();
        match msg {
            ClientMessage::Input { data } => assert_eq!(data, "\x03"),
            _ => panic!("expected Input message"),
        }
    }

    #[test]
    fn parses_resize_message() {
        let msg: ClientMessage = serde_json::from_value(json!({
            "type": "Resize",
            "cols": 100,
            "rows": 30,
            "pixel_width": 800,
            "pixel_height": 600
        }))
        .unwrap();
        match msg {
            ClientMessage::Resize {
                cols,
                rows,
                pixel_width,
                pixel_height,
            } => {
                assert_eq!(cols, 100);
                assert_eq!(rows, 30);
                assert_eq!(pixel_width, 800);
                assert_eq!(pixel_height, 600);
            }
            _ => panic!("expected Resize message"),
        }
    }

    #[test]
    fn rejects_unknown_message_type() {
        let result: Result<ClientMessage, _> =
            serde_json::from_value(json!({"type": "Nope", "data": "x"}));
        assert!(result.is_err());
    }

    #[test]
    fn binary_frame_wraps_bytes_verbatim() {
        let bytes = b"\x1b[31mred\x1b[0m".to_vec();
        match binary_frame(bytes.clone()) {
            Message::Binary(b) => assert_eq!(b, bytes),
            _ => panic!("expected Message::Binary"),
        }
    }

    #[test]
    fn binary_chunks_large_history_into_16kb_frames() {
        let big = vec![b'a'; 200_000];
        let chunked = binary_chunks(big.clone());
        assert!(!chunked.is_empty());
        assert!(chunked.iter().all(|m| matches!(m, Message::Binary(b) if !b.is_empty() && b.len() <= BINARY_FRAME_MAX)));
        let total: usize = chunked
            .iter()
            .map(|m| match m {
                Message::Binary(b) => b.len(),
                _ => 0,
            })
            .sum();
        assert_eq!(total, big.len());
    }

    #[test]
    fn raw_input_binary_bytes_write_verbatim_to_pty() {
        let mut sink: Vec<u8> = Vec::new();
        pty_write(&mut sink, b"\x03").unwrap();
        pty_write(&mut sink, b"\x1b[A").unwrap();
        assert_eq!(sink, b"\x03\x1b[A");
    }

    #[test]
    fn pty_size_maps_dimensions() {
        let size = pty_size(132, 43, 1056, 688);
        assert_eq!(size.cols, 132);
        assert_eq!(size.rows, 43);
        assert_eq!(size.pixel_width, 1056);
        assert_eq!(size.pixel_height, 688);
    }

    #[test]
    fn pty_size_defaults_pixels_to_zero() {
        let size = pty_size(80, 24, 0, 0);
        assert_eq!(size.cols, 80);
        assert_eq!(size.rows, 24);
        assert_eq!(size.pixel_width, 0);
        assert_eq!(size.pixel_height, 0);
    }

    #[test]
    fn last_connection_drop_marks_session_for_cleanup() {
        let c = std::sync::atomic::AtomicUsize::new(1);
        assert!(drop_connection(&c), "last drop should request cleanup");
        assert_eq!(c.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

#[test]
    fn remaining_connections_keep_session_alive() {
        let connections = AtomicUsize::new(2);
        assert!(!drop_connection(&connections));
        assert!(drop_connection(&connections));
        assert_eq!(connections.load(std::sync::atomic::Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn root_response_has_cors_headers() {
        use crate::AppState;
        use std::collections::HashMap;
        use std::sync::Arc;
        use tokio::sync::RwLock;

        let app = Router::new()
            .route("/", get(index))
            .with_state(AppState {
                sessions: Arc::new(RwLock::new(HashMap::new())),
            })
            .layer(CorsLayer::permissive());

        let response = app
            .oneshot(
                axum::http::Request::builder()
                    .uri("/")
                    .header("origin", "http://localhost:5000")
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert!(
            response
                .headers()
                .get("access-control-allow-origin")
                .is_some(),
            "krust must emit CORS headers (cross-origin fetch from Grit web UI)"
        );
    }
}
