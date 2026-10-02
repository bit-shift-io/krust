// krust — Rust/WASM terminal server.
//
// Axum server that spawns a `portable-pty` PTY per session and streams raw
// bytes to WASM clients over the WebSocket protocol in `handlers::`.
//
// Module layout:
// - `session.rs` — PTY session management (spawn, history, broadcast)
// - `handlers.rs` — HTTP/WS handler layer (router endpoints live in `main()`)

mod handlers;
mod session;

use axum::{routing::get, Router};
use tower_http::cors::CorsLayer;

#[tokio::main]
async fn main() {
    use std::{collections::HashMap, sync::Arc};
    use tokio::sync::RwLock;

    let state = session::AppState {
        sessions: Arc::new(RwLock::new(HashMap::new())),
    };

    let app = Router::new()
        .route("/", get(handlers::index))
        .route("/ws", get(handlers::ws_handler))
        .route("/pkg/terminal_client_bg.wasm", get(handlers::pkg_wasm))
        .route("/krust_runtime.js", get(handlers::runtime_js))
        .layer(CorsLayer::permissive());
    let app = app.with_state(state);

    let port = std::env::var("PORT").unwrap_or_else(|_| "3000".to_string());
    let addr = format!("0.0.0.0:{}", port);
    let listener = tokio::net::TcpListener::bind(&addr).await.unwrap();
    println!("Web terminal listening on http://localhost:{}", port);
    axum::serve(listener, app).await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::ws::Message, routing::get, Router};
    use serde_json::json;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tower::util::ServiceExt;

    use crate::handlers::{
        binary_frame, index, resync_plan, ClientMessage, Resync, ResyncStreak,
        MAX_CONSECUTIVE_RESYNCS,
    };
    use crate::session::{
        binary_chunks, drop_connection, pty_size, pty_write, AppState, History, BINARY_FRAME_MAX,
    };

    #[test]
    fn parses_input_message() {
        let msg: ClientMessage =
            serde_json::from_value(json!({"type": "Input", "data": "\x03"})).unwrap();
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
        let c = AtomicUsize::new(1);
        assert!(drop_connection(&c), "last drop should request cleanup");
        assert_eq!(c.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn remaining_connections_keep_session_alive() {
        let connections = AtomicUsize::new(2);
        assert!(!drop_connection(&connections));
        assert!(drop_connection(&connections));
        assert_eq!(connections.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn root_response_has_cors_headers() {
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

    // --- Resync correctness -------------------------------------------------
    //
    // The client is a stateful VT parser and can only be fed a contiguous,
    // duplicate-free byte stream. These pin the property that the old
    // drop-then-replay-the-whole-log backpressure path violated.

    #[test]
    fn resync_plan_is_up_to_date_at_and_past_the_window_end() {
        let log = b"hello".to_vec();
        assert_eq!(resync_plan(0, &log, 5), Resync::UpToDate);
        assert_eq!(resync_plan(0, &log, 9), Resync::UpToDate);
        assert_eq!(resync_plan(0, &log, 0), Resync::Tail(log.clone()));
    }

    #[test]
    fn resync_plan_sends_only_the_missing_tail() {
        // Client already has "he"; the log now holds "hello". It must get
        // exactly "llo" — sending the whole log would re-run bytes it has
        // already parsed, which is what desynced the grid.
        let log = b"hello".to_vec();
        assert_eq!(resync_plan(0, &log, 2), Resync::Tail(b"llo".to_vec()));
    }

    #[test]
    fn resync_plan_tail_continues_the_clients_own_bytes() {
        // Client sits mid-window, having consumed a prefix.
        let log = b"\x1b[?1006l\x1b[38;5;255mtext".to_vec();
        let sent = 6; // past "\x1b[?10"
        let Resync::Tail(tail) = resync_plan(0, &log, sent) else {
            panic!("expected a tail resync");
        };
        let mut whole = log[..sent as usize].to_vec();
        whole.extend_from_slice(&tail);
        assert_eq!(whole, log, "tail must rejoin the stream seamlessly");
    }

    #[test]
    fn resync_plan_requests_a_reset_when_the_client_aged_out() {
        let log = b"recent".to_vec();
        // Window starts at 100; the client only ever saw up to 50.
        assert_eq!(
            resync_plan(100, &log, 50),
            Resync::FullReset(log.clone())
        );
    }

    #[test]
    fn resync_plan_at_the_window_start_sends_a_tail_not_a_reset() {
        let log = b"recent".to_vec();
        assert_eq!(resync_plan(100, &log, 100), Resync::Tail(log));
    }

    // --- Retained log boundary safety --------------------------------------

    #[test]
    fn history_trim_never_starts_mid_escape_sequence() {
        // A log whose trim point would land inside "\x1b[?1006l" and
        // "\x1b[38;5;255m". Trimming used to cut at an arbitrary byte, so a
        // replay began with the orphaned tail ("6l", ";255m") which the
        // client parsed as ground text and painted into the grid.
        let mut h = History::new();
        let data = b"aaaa\x1b[?1006lbbbb\x1b[38;5;255mcccc".to_vec();
        h.push(0, &data, 1024);
        let (_, retained) = h.snapshot();
        assert_eq!(retained, data);

        // Now force a trim, then check the retained window starts on an ESC.
        h.push(retained.len() as u64, b"dddd", 20);
        let (start, retained) = h.snapshot();
        assert!(
            retained.first() == Some(&0x1b),
            "retained log must start on an ESC, got {:?}",
            &retained[..retained.len().min(8)]
        );
        assert_eq!(
            start, 16,
            "start offset must advance past the drained bytes"
        );
    }

    #[test]
    fn history_trim_never_splits_a_utf8_scalar() {
        // "é" is two bytes; cutting between them would emit a replacement
        // char on every replay.
        let mut h = History::new();
        let mut data = Vec::new();
        for _ in 0..8 {
            data.extend_from_slice("é".as_bytes());
        }
        assert!(data.len() > 8);
        h.push(0, &data, 1024);
        // No ESC anywhere, so the trim must still land on a char boundary.
        h.push(data.len() as u64, "é".as_bytes(), 9);
        let (_, retained) = h.snapshot();
        assert!(
            std::str::from_utf8(&retained).is_ok(),
            "retained log must stay valid UTF-8, got {:?}",
            retained
        );
    }

    #[test]
    fn history_keeps_the_window_under_its_budget() {
        let mut h = History::new();
        let mut offset = 0u64;
        for _ in 0..200 {
            let data = vec![b'x'; 1024];
            h.push(offset, &data, 4096);
            offset += data.len() as u64;
        }
        let (start, retained) = h.snapshot();
        assert!(retained.len() <= 4096, "window grew to {}", retained.len());
        assert_eq!(start + retained.len() as u64, offset);
    }

    #[test]
    fn history_range_from_returns_only_the_missing_tail() {
        let mut h = History::new();
        h.push(0, b"abcdef", 4096);
        h.push(6, b"ghijkl", 4096);
        let (from, tail) = h.range_from(8).expect("tail is still retained");
        assert_eq!(from, 8);
        assert_eq!(tail, b"ijkl");
    }

    #[test]
    fn history_range_from_is_none_once_aged_out() {
        let mut h = History::new();
        let mut offset = 0u64;
        for _ in 0..200 {
            let data = vec![b'x'; 1024];
            h.push(offset, &data, 4096);
            offset += data.len() as u64;
        }
        let (start, _) = h.snapshot();
        assert!(start > 0, "window should have trimmed");
        assert!(
            h.range_from(start - 1).is_none(),
            "a position before the window must not index the log"
        );
    }

    #[test]
    fn history_range_from_past_the_end_is_empty_not_a_panic() {
        let mut h = History::new();
        h.push(0, b"abcdef", 4096);
        let (from, tail) = h.range_from(999).expect("not aged out");
        assert_eq!(from, 999);
        assert!(
            tail.is_empty(),
            "a client already at the end needs no bytes, got {}",
            tail.len()
        );
    }

    #[test]
    fn resync_streak_tolerates_a_brief_stall() {
        let mut streak = ResyncStreak::default();
        for _ in 0..MAX_CONSECUTIVE_RESYNCS {
            assert!(
                !streak.attempt(),
                "a short stall must still be served a tail"
            );
        }
    }

    #[test]
    fn resync_streak_cuts_loose_a_client_that_never_catches_up() {
        let mut streak = ResyncStreak::default();
        let mut last = false;
        for _ in 0..=MAX_CONSECUTIVE_RESYNCS {
            last = streak.attempt();
        }
        assert!(last, "stalled client never reset");
    }

    #[test]
    fn resync_streak_starts_over_after_a_clean_send() {
        let mut streak = ResyncStreak::default();
        for _ in 0..MAX_CONSECUTIVE_RESYNCS {
            assert!(!streak.attempt());
        }
        streak.kept_up();
        // A client that caught up gets the full budget again.
        assert!(!streak.attempt());
    }

    #[test]
    fn history_end_is_the_offset_past_the_last_byte() {
        let mut h = History::new();
        h.push(0, b"abc", 1024);
        assert_eq!(h.end(), 3);
        h.push(3, b"de", 1024);
        assert_eq!(h.end(), 5);
    }

    #[test]
    fn history_trim_searches_forward_to_the_next_esc() {
        // 31 bytes total, budget 26 -> the naive cut would be byte 5, which
        // is inside "0123456789". The trim must instead advance to the next
        // ESC at byte 10 so the log restarts on a sequence boundary.
        let mut h = History::new();
        h.push(0, b"0123456789\x1b[1;1Hmored", 1024);
        h.push(21, b"0123456789", 26);
        let (start, retained) = h.snapshot();
        assert_eq!(retained[0], 0x1b);
        assert_eq!(&retained[1..], b"[1;1Hmored0123456789");
        assert_eq!(start, 10);
    }

    #[test]
    fn history_trim_lands_on_an_esc_sitting_exactly_at_the_cut() {
        let mut h = History::new();
        h.push(0, b"0123456789\x1b[1;1Hmored", 1024);
        h.push(21, b"tail", 15);
        let (start, retained) = h.snapshot();
        assert_eq!(retained[0], 0x1b);
        assert_eq!(&retained[1..], b"[1;1Hmoredtail");
        assert_eq!(start, 10);
    }

}