//! プロトコル違反を返すモックバックエンド（カオステスト用・std のみ）。
//!
//! 旧 `bad_backend_server.py` の Rust 置換。リクエストパスに応じて意図的に不正な
//! HTTP レスポンスを返し、プロキシ（Veil）が安全に処理（502 応答／接続クローズ）し、
//! クラッシュ・無限ハング・バッファ破損を起こさないことを検証するために用いる。
//!
//! `differential` テストの共有バックエンドとしても使えるよう、既定パスでは
//! Content-Length のボディを読み切ってから 200 を返す。
//!
//! 0.0.0.0:8080 で待受（環境変数 BAD_BACKEND_PORT で上書き可）。

use std::env;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;
use std::time::Duration;

fn parse_content_length(head: &[u8]) -> usize {
    // ヘッダ部（\r\n\r\n まで）から Content-Length を大文字小文字無視で探す。
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("content-length") {
                return v.trim().parse().unwrap_or(0);
            }
        }
    }
    0
}

fn header_is_chunked(head: &[u8]) -> bool {
    let text = String::from_utf8_lossy(head);
    for line in text.split("\r\n") {
        if let Some((k, v)) = line.split_once(':') {
            if k.trim().eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                return true;
            }
        }
    }
    false
}

/// chunked ボディを終端（"0\r\n\r\n"）まで読み捨てる。`initial` はヘッダ読み取り時に
/// 既にバッファへ入っているボディ先頭バイト。単純にバイト列上で終端シーケンスを探す。
fn drain_chunked_body(conn: &mut TcpStream, initial: &[u8], buf: &mut [u8; 4096]) {
    let mut acc: Vec<u8> = initial.to_vec();
    let terminator: &[u8] = b"0\r\n\r\n";
    loop {
        if acc.windows(terminator.len()).any(|w| w == terminator) {
            return;
        }
        if acc.len() > 1024 * 1024 {
            return;
        }
        match conn.read(buf) {
            Ok(0) | Err(_) => return,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
        }
    }
}

fn handle(mut conn: TcpStream) {
    let _ = conn.set_read_timeout(Some(Duration::from_secs(5)));
    let mut req: Vec<u8> = Vec::new();
    let mut buf = [0u8; 4096];
    // ヘッダ終端まで読む。
    loop {
        match conn.read(&mut buf) {
            Ok(0) => return,
            Ok(n) => {
                req.extend_from_slice(&buf[..n]);
                if req.windows(4).any(|w| w == b"\r\n\r\n") {
                    break;
                }
                if req.len() > 1024 * 1024 {
                    break;
                }
            }
            Err(_) => break,
        }
    }

    // リクエストパスを抽出（"METHOD PATH VERSION"）。
    let path: Vec<u8> = req
        .split(|&b| b == b' ')
        .nth(1)
        .map(|s| s.to_vec())
        .unwrap_or_else(|| b"/".to_vec());
    let starts = |p: &[u8]| path.starts_with(p);

    if starts(b"/truncated-headers") {
        // ヘッダー途中で切断。
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/pl");
    } else if starts(b"/cl-too-large") {
        // Content-Length > 実ボディ。
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\nshort");
    } else if starts(b"/cl-too-small") {
        // Content-Length < 実ボディ（余剰バイト＝スマグリング誘発）。
        let mut resp = b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n".to_vec();
        resp.extend(std::iter::repeat(b'X').take(500));
        let _ = conn.write_all(&resp);
    } else if starts(b"/huge-headers") {
        // 巨大ヘッダー（256 KB）。
        let mut resp = b"HTTP/1.1 200 OK\r\nX-Huge: ".to_vec();
        resp.extend(std::iter::repeat(b'A').take(256 * 1024));
        resp.extend_from_slice(b"\r\nContent-Length: 2\r\n\r\nok");
        let _ = conn.write_all(&resp);
    } else if starts(b"/bad-status") {
        // 不正なステータスライン。
        let _ = conn.write_all(b"HTTP/1.1 999 \x00\x01\x02\r\nContent-Length: 2\r\n\r\nok");
    } else if starts(b"/no-response") {
        // 受理後に無応答（読みタイムアウト検証）。
        thread::sleep(Duration::from_secs(30));
    } else if starts(b"/instant-close") {
        // 何も返さず即クローズ。
    } else {
        // 既定: ボディを読み切ってから 200（differential の共有バックエンド用途）。
        // Content-Length と Transfer-Encoding: chunked の両方に対応する（プロキシが
        // どちらの framing で転送してきても早期応答して接続を落とさないようにする）。
        let header_end = req
            .windows(4)
            .position(|w| w == b"\r\n\r\n")
            .map(|p| p + 4)
            .unwrap_or(req.len());
        if header_is_chunked(&req) {
            drain_chunked_body(&mut conn, &req[header_end..], &mut buf);
        } else {
            let cl = parse_content_length(&req);
            if cl > 0 {
                let already = req.len().saturating_sub(header_end);
                let mut remaining = cl.saturating_sub(already);
                while remaining > 0 {
                    match conn.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => remaining = remaining.saturating_sub(n),
                    }
                }
            }
        }
        let _ = conn.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok");
    }
}

fn main() {
    let port: u16 = env::var("BAD_BACKEND_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(8080);
    let listener = TcpListener::bind(("0.0.0.0", port)).expect("bind failed");
    println!("bad_backend listening on 0.0.0.0:{port}");
    let _ = std::io::stdout().flush();
    for conn in listener.incoming() {
        match conn {
            Ok(c) => {
                thread::spawn(move || handle(c));
            }
            Err(_) => continue,
        }
    }
}
