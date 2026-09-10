//! Real-time WebSocket probe for the krust terminal server.
//!
//! Connects as a raw WS client (like the browser does) and prints every frame
//! with real wall-clock elapsed ms since connect. Optionally answers terminal
//! queries so we can bisect whether the ~10s fish startup stall is caused by
//! unanswered queries.
//!
//! Usage: wsprobe <port> <sid> <mode> [--in <text>]
//!   mode = none     — no replies (pure observation)
//!   mode = mirror   — reply exactly like the current krust client:
//!                     DA1 (\x1b[0c -> \x1b[?1;2c), DA2 (\x1b[>c -> \x1b[>0;1;0c),
//!                     CPR (\x1b[6n -> \x1b[1;1R), OSC-11 (\x1b]11;? -> rgb reply)
//!   mode = extended — mirror PLUS kitty keyboard query (\x1b[?u),
//!                     DA2-final-q (\x1b[>0q), and DECRQSS (\x1bP+q...\x1b\\).
//!   --in <text>     — send <text> as a raw binary input frame on the first
//!                     inbound frame (drives the same pty_write path as the
//!                     browser's replies, so it can type commands for repros).

use futures_util::{SinkExt, StreamExt};
use std::time::Instant;

#[tokio::main]
async fn main() {
    let port = std::env::args().nth(1).unwrap_or_else(|| "3000".into());
    let sid = std::env::args().nth(2).unwrap_or_else(|| format!("probe{}", std::process::id()));
    let mode = std::env::args().nth(3).unwrap_or_else(|| "none".into());
    let mut pend_in: Option<Vec<u8>> = std::env::args().nth(4).map(|s| s.into_bytes());
    let url = format!("ws://127.0.0.1:{port}/ws?s={sid}");
    println!("connecting {url} mode={mode}");

    let (mut ws, _) = match tokio_tungstenite::connect_async(&url).await {
        Ok(x) => x,
        Err(e) => {
            eprintln!("connect failed: {e}");
            std::process::exit(1);
        }
    };
    let t0 = Instant::now();
    let mut buf: Vec<u8> = Vec::new();
    println!("[open]");

    loop {
        let Some(msg) = ws.next().await else { break };
        match msg {
            Ok(tokio_tungstenite::tungstenite::Message::Binary(data)) => {
                if let Ok(p) = std::env::var("KPROBE_DUMP") {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&p).unwrap();
                    let _ = f.write_all(&data);
                    let _ = f.write_all(b"\x00");
                }
                if let Some(b) = pend_in.take() {
                    let ms = t0.elapsed().as_secs_f64() * 1000.0;
                    println!("[{ms:8.1}] SEND      {:<4}B: {}", b.len(), sample(&b));
                    if let Err(e) = ws.send(tokio_tungstenite::tungstenite::Message::Binary(b)).await {
                        eprintln!("send failed: {e}");
                    }
                }
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                println!("[{ms:8.1}] {:<4}B frame: {}", data.len(), sample(&data));
                buf.extend_from_slice(&data);
                let replies = scan_replies(&mut buf, &mode);
                for r in replies {
                    let ms2 = t0.elapsed().as_secs_f64() * 1000.0;
                    println!("[{ms2:8.1}]     -> reply({mode}) {:<3}B: {}", r.len(), sample(&r));
                    if let Err(e) = ws.send(tokio_tungstenite::tungstenite::Message::Binary(r)).await {
                        eprintln!("send failed: {e}");
                    }
                }
            }
            Ok(tokio_tungstenite::tungstenite::Message::Text(t)) => {
                let ms = t0.elapsed().as_secs_f64() * 1000.0;
                println!("[{ms:8.1}] TEXT: {t}");
            }
            Ok(tokio_tungstenite::tungstenite::Message::Close(_)) => {
                println!("[close]");
                break;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("recv error: {e}");
                break;
            }
        }
    }
    println!("[done after {:.0}ms total]", t0.elapsed().as_secs_f64() * 1000.0);
}

/// Scan `buf` from the start for terminal queries and return replies.
/// Already-replied queries are consumed from `buf` (advance past them).
fn scan_replies(buf: &mut Vec<u8>, mode: &str) -> Vec<Vec<u8>> {
    if mode == "none" {
        return Vec::new();
    }
    let mut replies = Vec::new();
    let mut i = 0usize;
    while i < buf.len() {
        let b = buf[i];
        match b {
            0x1b => {
                let rest = &buf[i + 1..];
                // ESC ] OSC
                if rest.first() == Some(&b']') {
                    if let Some(end_rel) = find_osc_end(rest) {
                        let body = &rest[1..end_rel];
                        let (_, after) = rest.split_at(end_rel);
                        if body.starts_with(b"11;?") {
                            replies.push(b"\x1b]11;rgb:2b2b/2b2b/2b2b\x1b\\".to_vec());
                        }
                        // consume through the terminator
                        let used = 1 + end_rel + 1;
                        buf.drain(i..i + used);
                        continue;
                    } else {
                        break; // unterminated OSC still pending; keep rest of buf
                    }
                }
                // ESC P DCS (DECRQSS \x1bP+q<hexname>\x1b\\)
                if rest.first() == Some(&b'P') {
                    let after = i + 2;
                    if buf.len() > after && buf[after] == b'+' && buf.get(after + 1) == Some(&b'q') {
                        if let Some(term_rel_abs) = find_st(buf, after + 2) {
                            let name = &buf[after + 2..term_rel_abs]; // hex ascii
                            if mode == "extended" {
                                let mut r = b"\x1bP0$r".to_vec();
                                r.extend_from_slice(name);
                                r.extend_from_slice(b"\x1b\\");
                                replies.push(r);
                            }
                            buf.drain(i..term_rel_abs + 2);
                            continue;
                        } else {
                            break; // pending
                        }
                    }
                }
                // ESC [ CSI
                if rest.first() == Some(&b'[') {
                    let after = i + 2;
                    let mut j = after;
                    if j < buf.len() && (buf[j] == b'?' || buf[j] == b'>' || buf[j] == b'<' || buf[j] == b'=') {
                        j += 1;
                    }
                    let dstart = j;
                    while j < buf.len() && (buf[j].is_ascii_digit() || buf[j] == b';') {
                        j += 1;
                    }
                    let digits = &buf[dstart..j];
                    if j < buf.len() {
                        let final_ = buf[j];
                        let consumed = j + 1;
                        let prefix_pc = buf.get(after) == Some(&b'?');
                        let prefix_gt = buf.get(after) == Some(&b'>');
                        let num = parse_num(digits);
                        let q = if prefix_pc { b'?' } else if prefix_gt { b'>' } else { 0 };
                        match (q, final_, num) {
                            (b'?', b'u', _) if mode == "extended" => {
                                replies.push(b"\x1b[?1;2;3;4;6;9u".to_vec());
                            }
                            (b'?', b'c', _) => replies.push(b"\x1b[?1;2c".to_vec()),
                            (b'>', b'c', _) => replies.push(b"\x1b[>0;1;0c".to_vec()),
                            (b'>', b'q', _) if mode == "extended" => replies.push(b"\x1b[>0;1;0c".to_vec()),
                            (0, b'c', 0) => replies.push(b"\x1b[?1;2c".to_vec()),
                            (0, b'n', 6) => replies.push(b"\x1b[1;1R".to_vec()),
                            _ => {}
                        }
                        buf.drain(i..consumed);
                        continue;
                    }
                    break;
                }
                // some other ESC sequence; consume 1 byte to make progress
                buf.drain(i..i + 1);
            }
            _ => {
                buf.drain(i..i + 1);
            }
        }
    }
    replies
}

fn find_osc_end(rest: &[u8]) -> Option<usize> {
    for (idx, &b) in rest.iter().enumerate().skip(1) {
        if b == 0x07 {
            return Some(idx);
        }
        if b == 0x1b && rest.get(idx + 1) == Some(&b'\\') {
            return Some(idx + 1);
        }
    }
    None
}

/// index of ESC\ (ST) in buf starting from `from`; returns absolute index of '\' char.
fn find_st(buf: &[u8], from: usize) -> Option<usize> {
    let mut j = from;
    while j + 1 < buf.len() {
        if buf[j] == 0x1b && buf[j + 1] == b'\\' {
            return Some(j + 1);
        }
        j += 1;
    }
    None
}

fn parse_num(d: &[u8]) -> i32 {
    std::str::from_utf8(d).ok().and_then(|s| s.parse().ok()).unwrap_or(0)
}

fn sample(data: &[u8]) -> String {
    data.iter()
        .map(|&b| match b {
            0x1b => "ESC".to_string(),
            b'\n' => "\\n".to_string(),
            b'\r' => "\\r".to_string(),
            b if b.is_ascii_graphic() || b == b' ' => (b as char).to_string(),
            _ => format!("\\x{b:02x}"),
        })
        .collect()
}