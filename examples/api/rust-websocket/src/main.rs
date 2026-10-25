//! veil API ライブラリの HTTP/1.1 プロトコルアップグレード（F-261）のサンプル。
//!
//! - `/echo`: 独自プロトコル `x-echo` にアップグレードし、受け取った行をそのまま返す
//!   （graceful shutdown が始まったら `bye` を送って閉じる）
//! - `/raw-upgrade`: 要求を見ずに必ずアップグレードを返す（アップグレードでない要求は veil が 400 にする）
//! - `/h2c-upgrade`: 予約済みの `h2c` を返す（500 になる）
//!
//! 環境変数: VEIL_LISTEN（平文）、VEIL_TLS_LISTEN・VEIL_TLS_CERT・VEIL_TLS_KEY（TLS）、VEIL_KTLS（"1" で kTLS）。
use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use veil::api::upgrade::Upgraded;
use veil::prelude::*;

#[veil::route(path = "/echo", method = "GET", tags = ["upgrade"])]
async fn echo(req: Request) -> Response {
    let wants = req
        .upgrade_protocol()
        .is_some_and(|p| p.eq_ignore_ascii_case(b"x-echo"));
    if !wants {
        return Response::error(426, "Upgrade Required").with_header("upgrade", "x-echo");
    }
    Response::switching_protocols("x-echo", echo_lines).with_header("x-echo-version", "1")
}

#[veil::route(path = "/raw-upgrade", method = "GET")]
async fn raw_upgrade() -> Response {
    Response::switching_protocols("x-echo", echo_lines)
}

#[veil::route(path = "/h2c-upgrade", method = "GET")]
async fn h2c_upgrade() -> Response {
    Response::switching_protocols("h2c", echo_lines)
}

#[veil::route(path = "/plaintext", method = "GET")]
async fn plaintext() -> &'static str {
    "Hello, World!"
}

/// 行ごとのエコー。可読と終了の合図を同時に待つ（読み取りそのものは途中で捨てない）。
async fn echo_lines(mut up: Upgraded) {
    let mut shutdown = up.shutdown_signal();
    let mut buf = BytesMut::with_capacity(4096);
    loop {
        let readable = futures::select_biased! {
            _ = (&mut shutdown).fuse() => false,
            r = up.readable().fuse() => r.is_ok(),
        };
        if !readable {
            let _ = up.write_all(Bytes::from_static(b"bye\n")).await;
            let _ = up.shutdown().await;
            return;
        }
        let (res, b) = up.read(buf).await;
        buf = b;
        match res {
            Ok(n) if n > 0 => {}
            _ => return,
        }
        // 完結した行だけ返す（まとめて届いた行は 1 回で書く）
        if let Some(end) = buf.iter().rposition(|&c| c == b'\n') {
            let lines = buf.split_to(end + 1).freeze();
            if up.write_all(lines).await.is_err() {
                return;
            }
        }
    }
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    let e = |k: &str| std::env::var(k).ok();
    let mut b = Config::load()?.builder();
    if let Some(l) = e("VEIL_LISTEN") {
        b = b.listen_cleartext(&l);
    }
    if let (Some(l), Some(c), Some(k)) =
        (e("VEIL_TLS_LISTEN"), e("VEIL_TLS_CERT"), e("VEIL_TLS_KEY"))
    {
        b = b.listen(&l).tls(&c, &k);
    }
    let ktls = e("VEIL_KTLS").as_deref() == Some("1");
    b = b.set_value("tls.ktls_enabled", ktls)?;
    if let Some(n) = e("VEIL_MAX_CONN") {
        b = b.set_value(
            "security.max_concurrent_connections",
            n.parse::<i64>().unwrap_or(0),
        )?;
    }
    if let Some(n) = e("VEIL_SHUTDOWN_SECS") {
        b = b.set_value(
            "server.graceful_shutdown_timeout_secs",
            n.parse::<i64>().unwrap_or(5),
        )?;
    }
    veil::serve(b.build()).await
}
