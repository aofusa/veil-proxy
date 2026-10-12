//! HTTP-01 チャレンジの応答（RFC 8555 §8.3）。専用スレッドの小さな平文 HTTP/1.1 サーバで、
//! `GET /.well-known/acme-challenge/<token>` に key authorization を返す。それ以外のパスは
//! `https://<Host><path>` へ 301（ポート 80 を HTTPS リダイレクトとしても使えるように）。
//! メインのデータプレーン（ワーカーのイベントループ）は一切変更しない。

use ftlog::{error, info, warn};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

const PREFIX: &str = "/.well-known/acme-challenge/";
/// 同時に処理する接続数の上限（遅いクライアントでスレッドを使い切らないため）。
const MAX_CONCURRENT: usize = 64;

static TOKENS: Mutex<Option<HashMap<String, String>>> = Mutex::new(None);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// チャレンジのトークンを公開する。
pub fn publish(token: &str, key_authorization: &str) {
    TOKENS
        .lock()
        .unwrap()
        .get_or_insert_with(HashMap::new)
        .insert(token.to_string(), key_authorization.to_string());
}

/// チャレンジのトークンを取り下げる。
pub fn withdraw(token: &str) {
    if let Some(m) = TOKENS.lock().unwrap().as_mut() {
        m.remove(token);
    }
}

fn lookup(token: &str) -> Option<String> {
    TOKENS.lock().unwrap().as_ref()?.get(token).cloned()
}

/// 応答を組み立てる（テスト用に分離）。
pub(crate) fn respond(request: &[u8]) -> Vec<u8> {
    let text = String::from_utf8_lossy(request);
    let mut lines = text.split("\r\n");
    let first = lines.next().unwrap_or("");
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");
    let host = lines
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.trim().eq_ignore_ascii_case("host"))
        .map(|(_, v)| v.trim().to_string());
    if (method == "GET" || method == "HEAD") && path.starts_with(PREFIX) {
        let token = &path[PREFIX.len()..];
        if let Some(ka) = lookup(token) {
            let body = if method == "HEAD" { "" } else { ka.as_str() };
            return format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                ka.len(),
                body
            )
            .into_bytes();
        }
        return b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            .to_vec();
    }
    match host {
        Some(h) if !h.is_empty() && !h.contains(['\r', '\n']) && path.starts_with('/') => {
            let host_only = h.rsplit_once(':').map(|(a, _)| a).unwrap_or(&h);
            format!(
                "HTTP/1.1 301 Moved Permanently\r\nLocation: https://{}{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                host_only, path
            )
            .into_bytes()
        }
        _ => b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec(),
    }
}

/// `listen` で応答を始める（bind は呼び出し元のスレッドで行うので、特権降格の前に呼ぶこと）。
// 理由付き allow: ACME チャレンジ専用スレッドのブロッキングなソケット I/O（データプレーン外）。
#[allow(clippy::disallowed_methods)]
pub fn start(listen: &str) -> Result<(), String> {
    let listener = std::net::TcpListener::bind(listen)
        .map_err(|e| format!("ACME challenge listener bind {}: {}", listen, e))?;
    info!("[ACME] HTTP-01 challenge listener on {}", listen);
    std::thread::Builder::new()
        .name("veil-acme-http01".into())
        .spawn(move || {
            for conn in listener.incoming() {
                if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                    break;
                }
                let mut s = match conn {
                    Ok(s) => s,
                    Err(e) => {
                        warn!("[ACME] accept error: {}", e);
                        continue;
                    }
                };
                if ACTIVE.load(Ordering::Relaxed) >= MAX_CONCURRENT {
                    continue;
                }
                ACTIVE.fetch_add(1, Ordering::Relaxed);
                let spawned = std::thread::Builder::new()
                    .name("veil-acme-conn".into())
                    .spawn(move || {
                        let _ = s.set_read_timeout(Some(Duration::from_secs(5)));
                        let _ = s.set_write_timeout(Some(Duration::from_secs(5)));
                        let mut buf = Vec::with_capacity(1024);
                        let mut chunk = [0u8; 1024];
                        while buf.len() < 8192 && !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                            match s.read(&mut chunk) {
                                Ok(0) | Err(_) => break,
                                Ok(n) => buf.extend_from_slice(&chunk[..n]),
                            }
                        }
                        let _ = s.write_all(&respond(&buf));
                        ACTIVE.fetch_sub(1, Ordering::Relaxed);
                    });
                if spawned.is_err() {
                    ACTIVE.fetch_sub(1, Ordering::Relaxed);
                    error!("[ACME] failed to spawn a connection thread");
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_tokens_and_redirects_others() {
        publish("tok1", "tok1.thumb");
        let r = String::from_utf8(respond(
            b"GET /.well-known/acme-challenge/tok1 HTTP/1.1\r\nHost: a.example\r\n\r\n",
        ))
        .unwrap();
        assert!(r.starts_with("HTTP/1.1 200"), "{}", r);
        assert!(r.ends_with("tok1.thumb"));
        let r = String::from_utf8(respond(
            b"GET /.well-known/acme-challenge/none HTTP/1.1\r\nHost: a\r\n\r\n",
        ))
        .unwrap();
        assert!(r.starts_with("HTTP/1.1 404"));
        let r = String::from_utf8(respond(
            b"GET /x?y=1 HTTP/1.1\r\nHost: a.example:80\r\n\r\n",
        ))
        .unwrap();
        assert!(r.contains("Location: https://a.example/x?y=1"), "{}", r);
        withdraw("tok1");
        let r = String::from_utf8(respond(
            b"GET /.well-known/acme-challenge/tok1 HTTP/1.1\r\nHost: a\r\n\r\n",
        ))
        .unwrap();
        assert!(r.starts_with("HTTP/1.1 404"));
    }
}
