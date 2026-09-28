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
    http::header,
};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::broadcast;

use crate::session::{
    binary_chunks, get_or_create_session, pty_size, pty_write, AppState, History, StreamOffset,
};

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

/// Control frame telling the client to throw away its parser state.
const RESET_MESSAGE: &str = r#"{"type":"Reset"}"#;

/// How many catch-up attempts a single client gets before we give up on it.
///
/// A client that needs one resync was briefly busy; one that needs a
/// continuous run of them is not draining its socket fast enough to ever
/// overtake the PTY, so every delta is overtaken by new output before it
/// lands. Re-sending tails in that state would spin forever -- and the
/// unbounded copying of the log to do it is what turns a slow tab into a
/// wedged server. Past this many we tell the client to start over from the
/// retained window instead, which is bounded and always leaves it with a
/// stream that is consistent with what it is parsing.
pub(crate) const MAX_CONSECUTIVE_RESYNCS: u32 = 3;

/// The write half of a split WebSocket, i.e. what PTY output is sent on.
type WsSender = futures_util::stream::SplitSink<axum::extract::ws::WebSocket, Message>;

/// What a client that has consumed up to `sent_upto` still needs before the
/// stream it is parsing is contiguous again.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Resync {
    /// Already current; nothing to send.
    UpToDate,
    /// Exactly the missing tail. It continues the client's own bytes, so the
    /// parser resumes mid-sequence exactly where it left off.
    Tail(Vec<u8>),
    /// The client's position has aged out of the retained window, so no
    /// contiguous tail exists. It must drop its parser state and start over
    /// from the whole retained log (which begins on an ESC boundary).
    FullReset(Vec<u8>),
}

/// Plan the catch-up for a client at `sent_upto` against the retained window
/// `[start, start + bytes.len())`.
///
/// This is the whole backpressure story in one pure function. The old code
/// dropped the queued frames and then re-sent the *entire* log on top of what
/// the client had already parsed, which both duplicated bytes and restarted
/// the parser mid-sequence — the source of the literal escape-sequence text
/// that ended up painted in the input field. Handing back only the missing
/// tail makes the stream contiguous and duplicate-free by construction.
pub(crate) fn resync_plan(start: StreamOffset, bytes: &[u8], sent_upto: StreamOffset) -> Resync {
    let end = start + bytes.len() as StreamOffset;
    if sent_upto >= end {
        return Resync::UpToDate;
    }
    if sent_upto < start {
        return Resync::FullReset(bytes.to_vec());
    }
    Resync::Tail(bytes[(sent_upto - start) as usize..].to_vec())
}

/// Send a client whatever it is missing, and advance `sent_upto` to the end of
/// the retained window.
///
/// `force_full` skips the "are you current?" test and always restarts the
/// client from the whole window; it is the escape hatch for a client that has
/// already spent `MAX_CONSECUTIVE_RESYNCS` attempts on catch-ups.
///
/// `ws_sender` is `&mut` and the history lock is taken only long enough to
/// clone, so neither is held across an await.
async fn send_resync(
    ws_sender: &mut WsSender,
    history: &std::sync::Arc<std::sync::Mutex<History>>,
    sent_upto: &mut StreamOffset,
    force_full: bool,
) -> Result<(), ()> {
    // A tail only needs the bytes the client is missing; a full restart needs
    // the whole window. Cloning the window for the common tail case would
    // copy the log once per resync, which is what made a slow client spin.
    let (from, bytes) = {
        let hist = history.lock().expect("history lock poisoned");
        if force_full {
            hist.snapshot()
        } else {
            hist.range_from(*sent_upto)
                .unwrap_or_else(|| hist.snapshot())
        }
    };
    let start = from;
    let end = start + bytes.len() as StreamOffset;
    let plan = resync_plan(start, &bytes, *sent_upto);
    if matches!(plan, Resync::UpToDate) && !force_full {
        return Ok(());
    }
    // A tail continues the client's own bytes, so the parser simply resumes
    // mid-sequence where it stopped. A full restart cannot: the client has
    // either aged out of the window or never caught up, so it must drop its
    // parser state first or the log would land on top of its existing screen.
    let needs_reset = force_full || matches!(plan, Resync::FullReset(_));
    if needs_reset {
        ws_sender
            .send(axum::extract::ws::Message::Text(RESET_MESSAGE.to_string()))
            .await
            .map_err(|_| ())?;
    }
    for frame in binary_chunks(bytes) {
        ws_sender.send(frame).await.map_err(|_| ())?;
    }
    *sent_upto = end;
    Ok(())
}

/// Counts catch-up attempts for one client so a hopeless one can be cut loose.
///
/// A client that needs a single resync was briefly busy. One that needs a
/// continuous run of them is not draining its socket fast enough to ever
/// overtake the PTY: every tail is overtaken by new output before it lands,
/// so resyncing again would spin forever and keep copying the log to do it.
/// After `MAX_CONSECUTIVE_RESYNCS` we hand the client a full restart instead,
/// which is bounded and always leaves it parsing a stream that is consistent
/// with what it has been told to expect.
#[derive(Default)]
pub(crate) struct ResyncStreak {
    consecutive: u32,
}

impl ResyncStreak {
    /// Record a catch-up attempt; returns `true` when the client has spent too
    /// many and should be restarted from the retained window instead.
    pub(crate) fn attempt(&mut self) -> bool {
        self.consecutive += 1;
        if self.consecutive > MAX_CONSECUTIVE_RESYNCS {
            self.consecutive = 0;
            true
        } else {
            false
        }
    }

    /// Record a frame that went out live, i.e. a client that is keeping up.
    pub(crate) fn kept_up(&mut self) {
        self.consecutive = 0;
    }
}

/// Bring one client back to a contiguous stream, escalating to a full restart
/// once it has spent too many attempts on catch-ups.
async fn resync(
    ws_sender: &mut WsSender,
    history: &std::sync::Arc<std::sync::Mutex<History>>,
    sent_upto: &mut StreamOffset,
    streak: &mut ResyncStreak,
) -> Result<(), ()> {
    let force_full = streak.attempt();
    send_resync(ws_sender, history, sent_upto, force_full).await
}

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

    // 1. Re-sync retained output to the newly connected WebSocket.
    //
    // Subscribe *before* snapshotting. Doing it the other way round leaves a
    // window where the PTY produces output that is neither in the snapshot
    // nor delivered to this subscriber, which is a permanent hole in the
    // client's stream. Subscribing first means those frames arrive on the
    // channel and are handled as ordinary live output.
    let history = session.history.clone();
    let mut pty_rx = session.tx.subscribe();

    let (hist_start, hist_bytes) = {
        let hist = history.lock().expect("history lock poisoned");
        hist.snapshot()
    };
    // A fresh client has an empty parser, so the retained log — which always
    // begins on an ESC boundary — is a safe restart point.
    let mut sent_upto = hist_start;
    let hist_len = hist_bytes.len() as StreamOffset;
    if hist_len > 0 {
        for frame in binary_chunks(hist_bytes) {
            if ws_sender.send(frame).await.is_err() {
                return;
            }
        }
        sent_upto += hist_len;
    }

    // 2. Task: PTY output -> WebSocket
    let pty_read_task = tokio::spawn(async move {
        let mut budget = ByteBudget::default();
        let mut streak = ResyncStreak::default();
        loop {
            let (offset, bytes) = match pty_rx.recv().await {
                Ok(frame) => frame,
                // The bounded channel dropped frames. The bytes are still in
                // the log, so hand the client exactly what it missed.
                Err(broadcast::error::RecvError::Lagged(_)) => {
                    if resync(&mut ws_sender, &history, &mut sent_upto, &mut streak)
                        .await
                        .is_err()
                    {
                        break;
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            };

            // A frame that starts before `sent_upto` is one a catch-up has
            // already covered; forwarding it again would duplicate bytes.
            if offset < sent_upto {
                continue;
            }

            // Anything else means the client's stream is no longer
            // contiguous, which is the one thing its parser cannot tolerate.
            if offset != sent_upto {
                budget.pending = 0;
                if resync(&mut ws_sender, &history, &mut sent_upto, &mut streak)
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }

            if !budget.accept(bytes.len()) {
                // Budget exceeded: this client is not draining fast enough
                // (a throttled or frozen browser tab). Rather than dropping
                // the frame, catch it up from the log.
                budget.pending = 0;
                if resync(&mut ws_sender, &history, &mut sent_upto, &mut streak)
                    .await
                    .is_err()
                {
                    break;
                }
                continue;
            }
            streak.kept_up();
            let len = bytes.len() as StreamOffset;
            if ws_sender.send(binary_frame(bytes)).await.is_err() {
                break;
            }
            sent_upto += len;
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