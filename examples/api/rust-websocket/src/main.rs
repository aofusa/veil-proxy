//! veil API ライブラリの HTTP/1.1 プロトコルアップグレード（F-261）のサンプル。
//!
//! - `/echo`: 独自プロトコル `x-echo` にアップグレードし、受け取った行をそのまま返す
//!   （graceful shutdown が始まったら `bye` を送って閉じる）
//! - `/raw-upgrade`: 要求を見ずに必ずアップグレードを返す（アップグレードでない要求は veil が 400 にする）
//! - `/h2c-upgrade`: 予約済みの `h2c` を返す（500 になる）
//! - `/ws`: WebSocket のエコー（F-262。テキスト・バイナリをそのまま返す。Autobahn TestSuite の対象）
//! - `/chat`: WebSocket のブロードキャスト（`split` した送信側を全員に配る）
//! - `/typed`: JSON の型付きメッセージ（OpenAPI の `x-veil-websocket` に載り、型に合わないテキストは 1007 で閉じる）
//!
//! 環境変数: VEIL_LISTEN（平文）、VEIL_TLS_LISTEN・VEIL_TLS_CERT・VEIL_TLS_KEY（TLS）、VEIL_KTLS（"1" で kTLS）、
//! VEIL_WS_COMPRESSION（"1" で permessage-deflate）。
use bytes::{Bytes, BytesMut};
use futures::FutureExt;
use std::sync::{Mutex, OnceLock};
use veil::api::upgrade::Upgraded;
use veil::api::ws::{Message, WebSocket, WebSocketUpgrade, WsSender};
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

/// WebSocket のエコー（Ping には veil が自動で Pong を返す）。
#[veil::route(path = "/ws", method = "GET", tags = ["websocket"], ws_protocols = ["echo.v1"])]
async fn ws(upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .protocols(["echo.v1"])
        .on_upgrade(|mut ws: WebSocket| async move {
            while let Some(Ok(msg)) = ws.recv().await {
                match msg {
                    Message::Text(_) | Message::Binary(_) => {
                        if ws.send(msg).await.is_err() {
                            break;
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
        })
}

/// チャットの参加者の送信側（別のタスク・スレッドから積める）。
fn members() -> &'static Mutex<Vec<WsSender>> {
    static M: OnceLock<Mutex<Vec<WsSender>>> = OnceLock::new();
    M.get_or_init(|| Mutex::new(Vec::new()))
}

/// 受け取ったテキストを全員（自分を含む）へ配る。閉じた参加者はそのとき取り除く。
#[veil::route(path = "/chat", method = "GET", tags = ["websocket"])]
async fn chat(upgrade: WebSocketUpgrade) -> Response {
    upgrade.on_upgrade(|ws: WebSocket| async move {
        let (tx, mut rx) = ws.split();
        members().lock().unwrap().push(tx);
        while let Some(Ok(msg)) = rx.recv().await {
            if let Message::Text(t) = msg {
                members()
                    .lock()
                    .unwrap()
                    .retain(|m| m.send(Message::Text(t.clone())).is_ok());
            }
        }
    })
}

#[derive(serde::Deserialize, veil::Schema)]
struct ChatIn {
    room: String,
    text: String,
}

#[derive(serde::Serialize, veil::Schema)]
struct ChatOut {
    room: String,
    text: String,
    length: usize,
}

/// 型付きの JSON メッセージ（受け取った `ChatIn` に長さを付けて返す）。
#[veil::route(path = "/typed", method = "GET", tags = ["websocket"], ws_receive = ChatIn, ws_send = ChatOut)]
async fn typed(upgrade: WebSocketUpgrade) -> Response {
    upgrade
        .messages::<ChatIn, ChatOut>()
        .validate(true)
        .on_upgrade(|mut ws: WebSocket| async move {
            while let Some(Ok(Message::Text(t))) = ws.recv().await {
                let Ok(m) = serde_json::from_str::<ChatIn>(t.as_str()) else {
                    break;
                };
                let out = ChatOut {
                    length: m.text.chars().count(),
                    room: m.room,
                    text: m.text,
                };
                let body = serde_json::to_string(&out).unwrap_or_default();
                if ws.send(Message::text(body)).await.is_err() {
                    break;
                }
            }
        })
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
    // permessage-deflate（RFC 7692）を交渉する
    if e("VEIL_WS_COMPRESSION").as_deref() == Some("1") {
        b = b.set_value("api.websocket_compression", true)?;
    }
    if let Some(n) = e("VEIL_SHUTDOWN_SECS") {
        b = b.set_value(
            "server.graceful_shutdown_timeout_secs",
            n.parse::<i64>().unwrap_or(5),
        )?;
    }
    veil::serve(b.build()).await
}
