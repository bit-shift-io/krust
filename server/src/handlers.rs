// HTTP and WebSocket handler layer for the krust terminal server.
//
// `ws_handler`/`handle_socket` drive the WebSocket protocol (replay history,
// forward PTY output, accept JSON control + raw binary input). The remaining
// handlers serve the terminal HTML and the built WASM package.

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Query, State,
    },
    http::{header, StatusCode},
    response::{IntoResponse, Response},
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::sync::OnceLock;
use tokio::sync::broadcast;

use crate::session::{get_or_create_session, pty_size, pty_write, AppState};

const INDEX_HTML: &str = include_str!("../../client/res/server.html");
const RUNTIME_JS: &str = include_str!("../../client/res/krust_runtime.js");
const TERMINAL_WASM: &[u8] =
    include_bytes!("../../target/wasm/wasm32-unknown-unknown/release/terminal_client.wasm");

/// Wrap raw PTY bytes as a single binary WebSocket frame.
pub(crate) fn binary_frame(bytes: Vec<u8>) -> Message {
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

#[derive(Deserialize)]
pub(crate) struct WsQuery {
    s: Option<String>,
    dir: Option<String>,
}

#[derive(Deserialize)]
#[serde(tag = "type")]
pub(crate) enum ClientMessage {
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

pub(crate) async fn index() -> ([(header::HeaderName, &'static str); 2], &'static str) {
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

/// Serve the built WASM package, embedded directly in the binary so a
/// single compiled `krust` executable is fully self-contained.
pub(crate) async fn pkg_wasm() -> ([(header::HeaderName, &'static str); 2], &'static [u8]) {
    (
        [
            (header::CONTENT_TYPE, "application/wasm"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        TERMINAL_WASM,
    )
}

/// Serve the `krust` FFI runtime, embedded directly in the binary.
pub(crate) async fn runtime_js() -> ([(header::HeaderName, &'static str); 2], &'static str) {
    (
        [
            (header::CONTENT_TYPE, "text/javascript; charset=utf-8"),
            (header::CACHE_CONTROL, "no-store"),
        ],
        RUNTIME_JS,
    )
}

/// The system font families the browser resolves via the CSS `FONT_STACK`
/// (see `client/src/measure.rs`), in priority order. `monospace` is the
/// generic alias and always accepts fontconfig's match; the named families
/// must resolve to *themselves* to be accepted, because fontconfig fuzzily
/// falls back to the top installed font (e.g. "Noto Sans") for a missing
/// name, which the browser would not use for that family slot.
const SYSTEM_FONT_STACK: &[&str] = &[
    "JetBrains Mono",
    "Fira Code",
    "Menlo",
    "Consolas",
    "monospace",
];

/// Resolved system font bytes, computed once on the first request. System
/// fonts don't change during a server run, so a `OnceLock` cache suffices.
static SYSTEM_FONT: OnceLock<Option<Vec<u8>>> = OnceLock::new();

fn system_font_bytes() -> Option<&'static [u8]> {
    SYSTEM_FONT.get_or_init(resolve_system_font).as_deref()
}

/// Ask fontconfig which family `family` resolves to and where its file lives.
fn fc_match(family: &str) -> Option<(String, String)> {
    let out = std::process::Command::new("fc-match")
        .args(["--format=%{family[0]}|%{file}", family])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let stdout = String::from_utf8(out.stdout).ok()?;
    let (resolved_family, file) = stdout.trim().split_once('|')?;
    if file.is_empty() {
        return None;
    }
    Some((resolved_family.to_string(), file.to_string()))
}

/// Resolve the first installed family from [`SYSTEM_FONT_STACK`] and read its
/// file bytes. Returns `None` when fontconfig is unavailable or none of the
/// families are installed.
fn resolve_system_font() -> Option<Vec<u8>> {
    for family in SYSTEM_FONT_STACK {
        let (resolved, file) = fc_match(family)?;
        let installed = family.eq_ignore_ascii_case("monospace")
            || resolved.eq_ignore_ascii_case(family);
        if !installed {
            continue;
        }
        if let Ok(bytes) = std::fs::read(&file) {
            if !bytes.is_empty() {
                return Some(bytes);
            }
        }
    }
    None
}

/// Serve the resolved system monospace font bytes.
///
/// The browser will not expose installed font files to the WASM client, so
/// the local server resolves the same family stack the Canvas 2D reference
/// renderer uses and streams the matching font file over HTTP. The client
/// feeds these bytes to `ab_glyph` to rasterize its WebGL2 glyph atlas with
/// the exact font the reference path paints. Serves 404 when no matching
/// family is installed; the client falls back to its embedded Hack font.
pub(crate) async fn system_font() -> Response {
    match system_font_bytes() {
        Some(bytes) => (
            [
                (header::CONTENT_TYPE, "font/ttf"),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no matching system font").into_response(),
    }
}

pub(crate) async fn ws_handler(
    ws: WebSocketUpgrade,
    Query(query): Query<WsQuery>,
    State(state): State<AppState>,
) -> impl axum::response::IntoResponse {
    let session_id = query
        .s
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "default".to_string());
    let start_dir = query.dir;

    ws.on_upgrade(move |socket| handle_socket(socket, state, session_id, start_dir))
}

async fn handle_socket(
    socket: WebSocket,
    state: AppState,
    session_id: String,
    start_dir: Option<String>,
) {
    let session = get_or_create_session(&state, &session_id, start_dir.as_deref()).await;
    session
        .connections
        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let (mut ws_sender, mut ws_receiver) = socket.split();

    // 1. Re-sync scrollback history to the newly connected WebSocket
    {
        let hist = session.history.lock().await;
        if !hist.is_empty() {
            for frame in crate::session::binary_chunks(hist.clone()) {
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
            if !budget.accept(frame.len()) {
                // Budget exceeded: drop stale frames and flush the latest
                // screen state (a full history replay) once the channel drains.
                while let Ok(_stale) = pty_rx.try_recv() {}
                let hist = history.lock().await.clone();
                if !hist.is_empty() {
                    for chunk in crate::session::binary_chunks(hist) {
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

    // Release this client. The session is kept alive so that a refresh of the
    // terminal page will re-use the existing session rather than creating a
    // fresh one. (Closing the PTY master fds would reset the shell, which is
    // undesired for a persistent terminal session.)
    //
    // NOTE: We do NOT remove the session from the map here. The session persists
    // until the server process restarts, ensuring that a refresh lands on the
    // same session and the terminal state (scrollback, cursor position, etc.)
    // is preserved across page reloads.
    //
    // If a true session expiry is ever needed, it can be added as a background
    // TTL task later.
    let _ = crate::session::drop_connection(&session.connections);
}