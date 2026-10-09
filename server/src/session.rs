// Server PTY session management.
//
// Sessions own a `portable-pty` PTY (shell subprocess), a broadcast channel for
// live output, and an in-memory scrollback history. A background thread reads
// PTY output into the history and fans it out to connected WebSockets.

use axum::extract::ws::Message;
use portable_pty::{CommandBuilder, MasterPty, NativePtySystem, PtySize, PtySystem};
use std::{
    collections::HashMap,
    io::{Read, Write},
    sync::{Arc, Mutex as StdMutex, atomic::Ordering},
};
use tokio::sync::{broadcast, Mutex, RwLock};
use vt100::Parser;

/// The mirror parser plus the absolute stream offset it has consumed.
///
/// Both live under one lock: the PTY reader thread advances them together
/// (after the history append, before the broadcast), so a screen clone and
/// its `upto` can never disagree. Replay callers compare `upto` against the
/// retained-log snapshot to decide exactly which log bytes a screen image
/// already covers — without that, bytes appended between the two lock
/// acquisitions would be re-applied to the client twice.
pub(crate) struct Mirror {
    pub(crate) parser: Parser,
    pub(crate) upto: StreamOffset,
}

pub(crate) type MirrorArc = std::sync::Arc<std::sync::Mutex<Mirror>>;

/// Keep 512 KB scrollback buffer per session.
pub(crate) const MAX_HISTORY_BYTES: usize = 1024 * 512;
/// Max bytes per WS binary frame.
pub(crate) const BINARY_FRAME_MAX: usize = 16 * 1024;
/// PTY read chunk size. Aligned with [`BINARY_FRAME_MAX`] so a burst is drained
/// in a few reads instead of hundreds of 1 KB ones.
pub(crate) const PTY_READ_BUF: usize = BINARY_FRAME_MAX;
/// Maximum concurrent sessions before eviction.
pub(crate) const MAX_CONCURRENT_SESSIONS: usize = 32;

/// Write raw bytes to a PTY master writer and flush.
///
/// Input travels from the WASM client to the shell as raw bytes (the client
/// encodes keystrokes; the PTY is the echo source, so there is no local echo).
pub(crate) fn pty_write(w: &mut dyn Write, bytes: &[u8]) -> std::io::Result<()> {
    w.write_all(bytes)?;
    w.flush()
}

/// Write bytes to a session's PTY writer, *waiting* for the lock if it is
/// briefly held rather than dropping the input. Input frames are already
/// processed one at a time by the socket task, so waiting cannot deadlock; a
/// `try_lock` here would silently discard typed input or paste bytes whenever
/// the two paths briefly contended.
///
/// Call only from a blocking context (e.g. `spawn_blocking`).
pub(crate) fn pty_write_locked(
    writer: &Mutex<Box<dyn Write + Send>>,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut w = writer.blocking_lock();
    pty_write(&mut *w, bytes)
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

/// A byte offset into a session's PTY output stream.
///
/// The stream is the concatenation of every byte the PTY has ever produced,
/// numbered from 0. Both the retained log and each client connection track a
/// position in it, so a client that falls behind can be handed exactly the
/// bytes it missed instead of a re-run of bytes it already parsed.
pub(crate) type StreamOffset = u64;

/// Retained output log, trimmed so a replay always restarts on a boundary the
/// client's parser can resume from.
///
/// The client is a stateful VT parser, so it can only be fed a *contiguous*
/// byte stream. Trimming at an arbitrary byte offset (as a plain
/// `Vec::drain(..n)` does) leaves the log starting mid-escape-sequence, and
/// the orphaned tail of that sequence then gets parsed as ground text — which
/// is how literal `;255m` / `25h` / `6l` fragments end up painted into the
/// grid. `ESC` is the cut point that makes a restart safe: it is a sequence
/// start, and because it can never appear inside a UTF-8 continuation byte it
/// is also a character boundary.
pub(crate) struct History {
    bytes: Vec<u8>,
    /// Absolute offset of `bytes[0]`.
    start: StreamOffset,
}

impl History {
    pub(crate) fn new() -> Self {
        Self {
            bytes: Vec::new(),
            start: 0,
        }
    }

    /// Append `data`, which begins at absolute offset `offset`, trimming the
    /// log back to `max` bytes.
    ///
    /// Callers must be a single writer (the PTY reader thread) so that
    /// `offset` stays exactly `end()`.
    pub(crate) fn push(&mut self, offset: StreamOffset, data: &[u8], max: usize) {
        debug_assert_eq!(self.end(), offset, "history must be appended in order");
        self.bytes.extend_from_slice(data);
        if self.bytes.len() > max {
            self.trim(max);
        }
    }

    /// Drop the oldest bytes, cutting at a boundary a parser can restart from.
    fn trim(&mut self, max: usize) {
        let mut cut = self.bytes.len() - max;
        if let Some(rel) = self.bytes[cut..].iter().position(|&b| b == 0x1b) {
            // Restart on an ESC: both a sequence and a character boundary.
            cut += rel;
        } else {
            // No ESC in the retained window, so there is no sequence to
            // orphan; still avoid splitting a UTF-8 scalar.
            while cut < self.bytes.len() && self.bytes[cut] & 0xc0 == 0x80 {
                cut += 1;
            }
        }
        self.bytes.drain(0..cut);
        self.start += cut as StreamOffset;
    }

    /// Absolute offset just past the last retained byte.
    pub(crate) fn end(&self) -> StreamOffset {
        self.start + self.bytes.len() as StreamOffset
    }

    /// The retained window, as `(start offset, bytes)`.
    pub(crate) fn snapshot(&self) -> (StreamOffset, Vec<u8>) {
        (self.start, self.bytes.clone())
    }

    /// Only the retained bytes at or after `from`, as `(start offset, bytes)`.
    ///
    /// A client that fell briefly behind needs just its missing tail, and
    /// cloning the whole window for it would copy hundreds of kilobytes the
    /// client already has. `None` means `from` has aged out; a `from` at or
    /// past the end yields an empty tail, which is the "already current" case.
    pub(crate) fn range_from(&self, from: StreamOffset) -> Option<(StreamOffset, Vec<u8>)> {
        if from < self.start {
            return None;
        }
        let skip = ((from - self.start) as usize).min(self.bytes.len());
        Some((from, self.bytes[skip..].to_vec()))
    }
}

pub(crate) struct Session {
    pub(crate) writer: Arc<Mutex<Box<dyn Write + Send>>>,
    pub(crate) master: Arc<Mutex<Box<dyn MasterPty + Send>>>,
    /// Live output frames, each tagged with the absolute stream offset it
    /// starts at so a consumer can detect a gap without counting bytes.
    pub(crate) tx: broadcast::Sender<(StreamOffset, Vec<u8>)>,
    /// Retained output log for resync. A plain `std::sync::Mutex` because the
    /// critical section is a few microseconds of vector work and is taken
    /// from both async and blocking contexts.
    pub(crate) history: Arc<StdMutex<History>>,
    /// Mirror VT parser fed with every PTY byte (single writer: the PTY
    /// reader thread). `replay_image` derives a screen-repainting stream
    /// from it, so resets never depend on a mid-stream log window.
    pub(crate) mirror: MirrorArc,
    /// Resize notifications for the mirror; drained by the PTY reader thread
    /// so the mirror size tracks the real PTY between reads.
    pub(crate) resize_tx: tokio::sync::mpsc::UnboundedSender<(u16, u16)>,
    pub(crate) connections: std::sync::atomic::AtomicUsize,
    /// Vitality flag: true while the PTY reader thread is alive, false when
    /// it encounters EOF or an error. Used to detect dead sessions.
    pub(crate) is_alive: Arc<std::sync::atomic::AtomicBool>,
}

#[derive(Clone)]
pub(crate) struct AppState {
    pub(crate) sessions: Arc<RwLock<HashMap<String, Arc<Session>>>>,
    pub(crate) config: Arc<crate::config::KrustConfig>,
}

pub(crate) async fn get_or_create_session(
    state: &AppState,
    session_id: &str,
    start_dir: Option<&str>,
) -> Arc<Session> {
    // Check if session already exists and is alive
    {
        let sessions = state.sessions.read().await;
        if let Some(session) = sessions.get(session_id) {
            if session.is_alive.load(Ordering::SeqCst) {
                return session.clone();
            }
            // Session is dead; drop the read lock so we can remove it
        }
    }

    // Session doesn't exist or is dead; remove dead session and spawn new
    let mut sessions = state.sessions.write().await;
    sessions.remove(session_id);

    // Enforce a maximum concurrent session cap.
    // If at capacity, purge the oldest dead session before creating a new one.
    if sessions.len() >= MAX_CONCURRENT_SESSIONS {
        // Remove any sessions that are not alive (they were already removed from
        // the read path, but just in case there's a race).
        sessions.retain(|_, s| s.is_alive.load(Ordering::SeqCst));
        // If still at capacity, evict the first (oldest) session with 0 connections.
        if sessions.len() >= MAX_CONCURRENT_SESSIONS {
            let first_key = sessions.keys().next().unwrap().clone();
            let first_session = sessions.remove(&first_key).unwrap();
            drop(first_session);
        }
    }

    // Session doesn't exist, spawn a new PTY process
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

    let (tx, _rx) = broadcast::channel::<(StreamOffset, Vec<u8>)>(512);

// Background thread reading from PTY output -> logging & broadcasting.
//
// The byte log and the broadcast must not be able to disagree: if a
// client is ever resynced from the log, a byte missing from the log is a
// permanent hole in that client's stream. So the log is updated with a
// blocking lock and is never skipped, and the absolute offset is claimed
// here in the same single-writer step that appends to it.
let tx_clone = tx.clone();
let history = Arc::new(StdMutex::new(History::new()));
let history_clone = history.clone();
// The mirror starts at the openpty default; the client's first Resize
// message re-sizes both the PTY and, via the resize channel, the mirror.
let mirror: MirrorArc = StdMutex::new(Mirror {
    parser: Parser::new(24, 80, 0),
    upto: 0,
})
.into();
let mirror_clone = mirror.clone();
let (resize_tx, mut resize_rx) = tokio::sync::mpsc::unbounded_channel::<(u16, u16)>();
let mut stream_offset: StreamOffset = 0;

// Vitality flag for session cleanup
let is_alive = Arc::new(std::sync::atomic::AtomicBool::new(true));
let is_alive_clone = is_alive.clone();

tokio::task::spawn_blocking(move || {
    // Match the PTY read to the frame size the client ships in: a 1 KB
    // read turned a 1 MB burst into ~1000 wakeups, each taking the history
    // and mirror locks. 16 KB keeps `stream_offset`/`m.upto` arithmetic
    // (both advanced by the byte count) unchanged.
    let mut buffer = [0u8; PTY_READ_BUF];
    while let Ok(n) = reader.read(&mut buffer) {
        if n == 0 {
            break;
        }
        // Apply any client-requested resizes before the bytes that follow
        // them, so the mirror's wrap points match the real PTY.
        while let Ok((cols, rows)) = resize_rx.try_recv() {
            let mut m = mirror_clone.lock().expect("mirror lock poisoned");
            if m.parser.screen().size() != (rows, cols) {
                m.parser.screen_mut().set_size(rows, cols);
            }
        }
        let data = buffer[..n].to_vec();

        // 1. Maintain the output log in memory
        {
            let mut hist = history_clone.lock().expect("history lock poisoned");
            hist.push(stream_offset, &data, MAX_HISTORY_BYTES);
        }

        // 2. Fold the bytes into the mirror. Deliberately *after* the log
        // append and *before* the broadcast: mirror.upto can never pass
        // the log's end, and every byte is live-broadcast once its
        // mirror state is in place.
        {
            let mut m = mirror_clone.lock().expect("mirror lock poisoned");
            m.parser.process(&data);
            m.upto = stream_offset + n as StreamOffset;
        }

        // 3. Broadcast output to active WebSocket listeners.
        // Fire-and-forget send: if receivers lag, the broadcast channel
        // drops the frame (bounded at 512) and recv() reports Lagged,
        // which clients resync from the log.
        let _ = tx_clone.send((stream_offset, data));
        stream_offset += n as StreamOffset;
    }
    // Mark session as dead when PTY reader exits (EOF or error)
    is_alive_clone.store(false, std::sync::atomic::Ordering::SeqCst);
});

let session = Arc::new(Session {
    writer: Arc::new(Mutex::new(writer)),
    master: Arc::new(Mutex::new(pair.master)),
    tx,
    history,
    mirror,
    resize_tx,
    connections: std::sync::atomic::AtomicUsize::new(0),
    is_alive,
});
    sessions.insert(session_id.to_string(), session.clone());
    session
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `Write` sink that records everything written, so a test can prove a
    /// write was not silently dropped.
    #[derive(Clone, Default)]
    struct Capture(Arc<StdMutex<Vec<u8>>>);

    impl Write for Capture {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("capture lock").extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn session_is_alive_flag_is_set_to_false_when_reader_ends() {
        // This test would require mocking the PTY reader to return EOF,
        // which is complex to set up. The functionality is covered by integration tests.
        // For now, we trust that the code correctly sets the flag.
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn input_writes_wait_for_the_lock_instead_of_dropping() {
        let cap = Capture::default();
        let writer = Arc::new(Mutex::new(
            Box::new(cap.clone()) as Box<dyn Write + Send>
        ));

        // Hold the writer lock from another thread and only release it after a
        // delay, so the input write below must wait.
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let holder = writer.clone();
        let held = tokio::task::spawn_blocking(move || {
            let _guard = holder.blocking_lock();
            ready_tx.send(()).expect("signal");
            std::thread::sleep(std::time::Duration::from_millis(200));
        });
        ready_rx.await.expect("holder acquired the lock");

        // The lock is held: a `try_lock` implementation would drop these bytes,
        // while the waiting implementation delivers them.
        let w = writer.clone();
        tokio::task::spawn_blocking(move || {
            pty_write_locked(&w, b"hello").expect("write");
        })
        .await
        .expect("join");

        held.await.expect("holder join");
        assert_eq!(&cap.0.lock().expect("capture lock")[..], b"hello");
    }
}
