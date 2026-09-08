// Server PTY session management.
//
// Sessions own a `portable-pty` PTY (shell subprocess), a broadcast channel for
// live output, and an in-memory scrollback history. A background thread reads
// PTY output into the history and fans it out to connected WebSockets.

use axum::extract::ws::Message;
use portable_pty::{
    CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem,
};
use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::Arc,
};
use tokio::sync::{broadcast, Mutex, RwLock};

/// Keep 512 KB scrollback buffer per session.
pub(crate) const MAX_HISTORY_BYTES: usize = 1024 * 512;
/// Max bytes per WS binary frame.
pub(crate) const BINARY_FRAME_MAX: usize = 16 * 1024;

/// Write raw bytes to a PTY master writer and flush.
///
/// Input travels from the WASM client to the shell as raw bytes (the client
/// encodes keystrokes; the PTY is the echo source, so there is no local echo).
pub(crate) fn pty_write(w: &mut dyn Write, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}

/// Build a [`PtySize`] from client-reported pixel dimensions.
pub(crate) fn pty_size(cols: u16, rows: u16, pixel_width: u16, pixel_height: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width,
        pixel_height,
    }
}

/// Decrement a session's connection count; returns `true` when the number of
/// remaining connections hit zero and the session should be cleaned up.
pub(crate) fn drop_connection(connections: &std::sync::atomic::AtomicUsize) -> bool {
    connections.fetch_sub(1, std::sync::atomic::Ordering::SeqCst) == 1
}

/// Split `bytes` into binary frames no larger than [`BINARY_FRAME_MAX`].
///
/// History replays can approach 512 KB; chunking keeps individual WS
/// frames small and avoids hitting the tungstenite max-frame limit.
pub(crate) fn binary_chunks(bytes: Vec<u8>) -> Vec<Message> {
    bytes
        .chunks(BINARY_FRAME_MAX)
        .map(|c| Message::Binary(c.to_vec()))
        .collect()
}

pub(crate) struct Session {
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub(crate) master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    pub(crate) tx: broadcast::Sender<Vec<u8>>,
    pub(crate) history: Arc<Mutex<Vec<u8>>>,
    pub(crate) connections: std::sync::atomic::AtomicUsize,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) sessions: Arc<RwLock<HashMap<String, Arc<Session>>>>,
}

pub(crate) async fn get_or_create_session(
    state: &AppState,
    session_id: &str,
    start_dir: Option<&str>,
) -> Arc<Session> {
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
    if let Some(dir) = start_dir {
        cmd.cwd(dir);
    }

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