use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::header,
    response::Html,
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

// Embed static files from project-root res/ folder
const INDEX_HTML: &str = include_str!("../res/index.html");
const XTERM_CSS: &str = include_str!("../res/xterm.css");
const XTERM_JS: &str = include_str!("../res/xterm.js");
const XTERM_FIT_JS: &str = include_str!("../res/xterm-addon-fit.js");
const XTERM_WEBGL_JS: &str = include_str!("../res/xterm-addon-webgl.js");

const MAX_HISTORY_BYTES: usize = 1024 * 512; // Keep 512 KB scrollback buffer per session

struct Session {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    tx: broadcast::Sender<Vec<u8>>,
    history: Arc<Mutex<Vec<u8>>>,
}

#[derive(Clone)]
struct AppState {
    sessions: Arc<RwLock<HashMap<String, Arc<Session>>>>,
}

#[derive(Deserialize)]
struct WsQuery {
    session_id: Option<String>,
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
        )
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await.unwrap();
    println!("Web terminal listening on http://localhost:3000");
    axum::serve(listener, app).await.unwrap();
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<AppState>,
) -> impl axum::response::IntoResponse {
    let session_id = query
        .session_id
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

    let (tx, _) = broadcast::channel::<Vec<u8>>(512);
    let history = Arc::new(Mutex::new(Vec::new()));

    // Background thread reading from PTY output -> broadcasting & storing history
    let tx_clone = tx.clone();
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

            // 2. Broadcast output to active WebSocket listeners
            let _ = tx_clone.send(data);
        }
    });

    let session = Arc::new(Session {
        writer: Arc::new(Mutex::new(writer)),
        master: Arc::new(Mutex::new(pair.master)),
        tx,
        history,
    });

    sessions.insert(session_id.to_string(), session.clone());
    session
}

async fn handle_socket(socket: WebSocket, state: AppState, session_id: String) {
    let session = get_or_create_session(&state, &session_id).await;
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // 1. Re-sync scrollback history to the newly connected WebSocket
    {
        let hist = session.history.lock().await;
        if !hist.is_empty() {
            let history_text = String::from_utf8_lossy(&hist).to_string();
            let _ = ws_sender.send(Message::Text(history_text)).await;
        }
    }

    // Subscribe to ongoing PTY output stream
    let mut pty_rx = session.tx.subscribe();

    // 2. Task: PTY output -> WebSocket
    let pty_read_task = tokio::spawn(async move {
        while let Ok(bytes) = pty_rx.recv().await {
            if ws_sender
                .send(Message::Text(String::from_utf8_lossy(&bytes).to_string()))
                .await
                .is_err()
            {
                break;
            }
        }
    });

    // 3. Task: WebSocket input -> PTY writer & resize handlers
    let writer = session.writer.clone();
    let master = session.master.clone();

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
}
