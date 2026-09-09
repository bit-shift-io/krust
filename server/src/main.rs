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
        .route(
            "/pkg/terminal_client_bg.wasm",
            get(|| handlers::serve_pkg_file("terminal_client_bg.wasm", "application/wasm")),
        )
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

    use crate::handlers::{binary_frame, index, ClientMessage};
    use crate::session::{
        binary_chunks, drop_connection, pty_size, pty_write, AppState, BINARY_FRAME_MAX,
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
}