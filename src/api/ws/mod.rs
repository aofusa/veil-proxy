// F-262: ハンドラで WebSocket（RFC 6455）を終端する。設計は docs/backlog/features/F-262-handler-websocket-rust.md。
// F-261 のアップグレード（`Response::switching_protocols`）の上に載る。本体のプロキシの WebSocket の中継
// （src/proxy/websocket.rs）には触れない。
//! WebSocket (RFC 6455) for handlers.
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil::api::ws::{Message, WebSocket, WebSocketUpgrade};
//!
//! #[veil::route(path = "/ws", method = "GET")]
//! async fn ws(upgrade: WebSocketUpgrade) -> Response {
//!     upgrade.on_upgrade(|mut ws: WebSocket| async move {
//!         while let Some(Ok(msg)) = ws.recv().await {
//!             match msg {
//!                 Message::Text(_) | Message::Binary(_) => {
//!                     if ws.send(msg).await.is_err() {
//!                         break;
//!                     }
//!                 }
//!                 Message::Close(_) => break,
//!                 _ => {} // Ping is answered automatically
//!             }
//!         }
//!     })
//! }
//! ```
//!
//! Frames are decoded in the receive buffer (the mask is removed in place and payloads are `Bytes` slices of it);
//! frames sent while more input is already buffered are written together with one vectored write.

pub(crate) mod codec;
#[cfg(feature = "compression")]
pub(crate) mod deflate;

use super::request::Request;
use super::response::Response;
use super::upgrade::{ShutdownSignal, Upgraded};
use bytes::{Bytes, BytesMut};
use codec::{Decoded, Decoder};
use futures::FutureExt;
use std::cell::Cell;
use std::collections::VecDeque;
use std::future::Future;
use std::io;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

// ===================== 設定（[api] websocket_*） =====================

static MAX_MESSAGE: AtomicUsize = AtomicUsize::new(16 * 1024 * 1024);
static MAX_FRAME: AtomicUsize = AtomicUsize::new(16 * 1024 * 1024);
static PING_INTERVAL_SECS: AtomicU64 = AtomicU64::new(0);
static IDLE_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(0);
#[cfg(feature = "compression")]
static COMPRESSION: AtomicBool = AtomicBool::new(false);

/// Applies the `[api] websocket_*` keys (called by the configuration hook).
pub(crate) fn apply_settings(s: &super::app::ApiSettings) {
    MAX_MESSAGE.store(s.websocket_max_message_size.max(1), Ordering::Relaxed);
    MAX_FRAME.store(s.websocket_max_frame_size.max(1), Ordering::Relaxed);
    PING_INTERVAL_SECS.store(s.websocket_ping_interval_secs, Ordering::Relaxed);
    IDLE_TIMEOUT_SECS.store(s.websocket_idle_timeout_secs, Ordering::Relaxed);
    #[cfg(feature = "compression")]
    COMPRESSION.store(s.websocket_compression, Ordering::Relaxed);
    #[cfg(not(feature = "compression"))]
    if s.websocket_compression {
        ftlog::warn!("[api] websocket_compression needs the compression feature; ignored");
    }
}

/// The message size limit of `[api] websocket_max_message_size` (for the OpenAPI document).
#[cfg(feature = "openapi")]
pub(crate) fn max_message_size() -> usize {
    MAX_MESSAGE.load(Ordering::Relaxed)
}

// ===================== メトリクス =====================

#[cfg(feature = "metrics")]
mod metrics {
    use once_cell::sync::Lazy;
    use prometheus::{IntCounter, IntCounterVec, IntGauge, Opts};

    /// Open WebSocket connections terminated by handlers.
    pub(super) static CONNECTIONS: Lazy<IntGauge> = Lazy::new(|| {
        let g = IntGauge::with_opts(
            Opts::new(
                "websocket_connections",
                "Number of open WebSocket connections terminated by API handlers",
            )
            .namespace("veil_proxy"),
        )
        .unwrap();
        let _ = crate::metrics::METRICS_REGISTRY.register(Box::new(g.clone()));
        g
    });
    static MESSAGES: Lazy<IntCounterVec> = Lazy::new(|| {
        let c = IntCounterVec::new(
            Opts::new(
                "websocket_messages_total",
                "WebSocket messages received and sent by API handlers",
            )
            .namespace("veil_proxy"),
            &["direction"],
        )
        .unwrap();
        let _ = crate::metrics::METRICS_REGISTRY.register(Box::new(c.clone()));
        c
    });
    // メッセージごとに系列を引かない（ラベルの値は 2 つだけなので先に解決しておく）
    pub(super) static RECEIVED: Lazy<IntCounter> =
        Lazy::new(|| MESSAGES.with_label_values(&["received"]));
    pub(super) static SENT: Lazy<IntCounter> = Lazy::new(|| MESSAGES.with_label_values(&["sent"]));
}

// 開いている WebSocket の数（drop で減らす）。
struct ConnGauge;
impl ConnGauge {
    fn new() -> ConnGauge {
        #[cfg(feature = "metrics")]
        metrics::CONNECTIONS.inc();
        ConnGauge
    }
}
impl Drop for ConnGauge {
    fn drop(&mut self) {
        #[cfg(feature = "metrics")]
        metrics::CONNECTIONS.dec();
    }
}

#[inline]
fn count_received() {
    #[cfg(feature = "metrics")]
    metrics::RECEIVED.inc();
}

#[inline]
fn count_sent(n: u64) {
    #[cfg(feature = "metrics")]
    metrics::SENT.inc_by(n);
    #[cfg(not(feature = "metrics"))]
    let _ = n;
}

// ===================== メッセージ =====================

/// UTF-8 text held as `Bytes` (validated once; cloning only increments a reference count).
#[derive(Clone, PartialEq, Eq, Hash, Default)]
pub struct Utf8Bytes(Bytes);

impl Utf8Bytes {
    /// From a static string.
    pub const fn from_static(s: &'static str) -> Utf8Bytes {
        Utf8Bytes(Bytes::from_static(s.as_bytes()))
    }

    /// The text.
    pub fn as_str(&self) -> &str {
        // SAFETY: 作るときに UTF-8 を確かめている（復号器・`From<String>`・`TryFrom<Bytes>`）
        unsafe { std::str::from_utf8_unchecked(&self.0) }
    }

    /// The underlying bytes.
    pub fn into_bytes(self) -> Bytes {
        self.0
    }

    /// Wraps bytes already known to be UTF-8.
    pub(crate) fn from_validated(b: Bytes) -> Utf8Bytes {
        debug_assert!(std::str::from_utf8(&b).is_ok());
        Utf8Bytes(b)
    }
}

// rustdoc が str のメソッドの文書（英語以外の例を含む）を Utf8Bytes の頁へ展開するので impl を隠す（tools/release/check-docs.sh）
#[doc(hidden)]
impl std::ops::Deref for Utf8Bytes {
    type Target = str;
    fn deref(&self) -> &str {
        self.as_str()
    }
}

impl std::fmt::Debug for Utf8Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(self.as_str(), f)
    }
}

impl std::fmt::Display for Utf8Bytes {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl From<String> for Utf8Bytes {
    fn from(s: String) -> Utf8Bytes {
        Utf8Bytes(Bytes::from(s))
    }
}

impl From<&'static str> for Utf8Bytes {
    fn from(s: &'static str) -> Utf8Bytes {
        Utf8Bytes::from_static(s)
    }
}

impl TryFrom<Bytes> for Utf8Bytes {
    type Error = std::str::Utf8Error;
    fn try_from(b: Bytes) -> Result<Utf8Bytes, Self::Error> {
        std::str::from_utf8(&b)?;
        Ok(Utf8Bytes(b))
    }
}

impl PartialEq<str> for Utf8Bytes {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for Utf8Bytes {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// The status code and reason of a Close frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CloseFrame {
    /// The status code (1000 normal closure, 1001 going away, ...).
    pub code: u16,
    /// The reason (at most 123 bytes are sent).
    pub reason: Utf8Bytes,
}

/// A WebSocket message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A text message.
    Text(Utf8Bytes),
    /// A binary message.
    Binary(Bytes),
    /// A Ping (answered automatically unless `auto_pong(false)`).
    Ping(Bytes),
    /// A Pong.
    Pong(Bytes),
    /// A Close frame (the closing handshake is completed automatically).
    Close(Option<CloseFrame>),
}

impl Message {
    /// A text message.
    pub fn text(s: impl Into<Utf8Bytes>) -> Message {
        Message::Text(s.into())
    }

    /// A binary message.
    pub fn binary(b: impl Into<Bytes>) -> Message {
        Message::Binary(b.into())
    }
}

/// Errors of a WebSocket connection.
#[derive(Debug)]
pub enum WsError {
    /// The peer violated the protocol; the connection was closed with this close code (1002, 1007, 1009, ...).
    Protocol(u16),
    /// An I/O error.
    Io(io::Error),
    /// The connection is closed (a Close frame was sent or received, or the peer is gone).
    Closed,
    /// The send queue of a [`WsSender`] is full.
    Full,
}

impl std::fmt::Display for WsError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WsError::Protocol(c) => write!(f, "websocket protocol error (close code {})", c),
            WsError::Io(e) => write!(f, "websocket io error: {}", e),
            WsError::Closed => f.write_str("websocket connection closed"),
            WsError::Full => f.write_str("websocket send queue is full"),
        }
    }
}

impl std::error::Error for WsError {}

impl From<io::Error> for WsError {
    fn from(e: io::Error) -> WsError {
        WsError::Io(e)
    }
}

// ===================== ハンドシェイク（抽出子） =====================

/// Validator of received JSON text messages (`WebSocketUpgrade::messages` + `validate`).
type TextCheck = fn(&[u8]) -> bool;

/// The WebSocket handshake of a request (an extractor). Checks the request (HTTP/1.1, `GET`,
/// `Upgrade: websocket`, `Connection: upgrade`, `Sec-WebSocket-Version: 13` and a 16-byte `Sec-WebSocket-Key`):
/// a malformed handshake is 400, another version is 426 with `Sec-WebSocket-Version: 13`.
pub struct WebSocketUpgrade {
    key: Bytes,
    offered: Option<Bytes>,
    origin: Option<Bytes>,
    #[cfg(feature = "compression")]
    extensions: Option<Bytes>,
    supported: Vec<String>,
    allowed_origins: Option<Vec<String>>,
    max_message: Option<usize>,
    auto_pong: bool,
    typed_check: Option<TextCheck>,
    validate: bool,
}

impl std::fmt::Debug for WebSocketUpgrade {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocketUpgrade")
            .field("supported", &self.supported)
            .finish()
    }
}

fn header<'a>(req: &'a Request, name: &str) -> Option<&'a Bytes> {
    req.headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
        .map(|(_, v)| v)
}

fn has_token(v: &[u8], token: &[u8]) -> bool {
    v.split(|&b| b == b',')
        .any(|t| t.trim_ascii().eq_ignore_ascii_case(token))
}

impl super::extract::FromRequest for WebSocketUpgrade {
    fn from_request(req: &Request) -> Result<Self, Response> {
        let bad = |m: &str| Err(Response::error(400, m));
        if req.version != super::request::HttpVersion::Http11 {
            return bad("WebSocket requires HTTP/1.1");
        }
        if &req.method[..] != b"GET" {
            return Err(Response::error(405, "Method Not Allowed"));
        }
        let upgrade_ok = header(req, "upgrade").is_some_and(|v| has_token(v, b"websocket"));
        let conn_ok = req
            .headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(b"connection"))
            .any(|(_, v)| has_token(v, b"upgrade"));
        if !upgrade_ok || !conn_ok {
            return bad("not a WebSocket handshake (Upgrade: websocket and Connection: upgrade are required)");
        }
        if header(req, "sec-websocket-version").map(|v| v.trim_ascii()) != Some(&b"13"[..]) {
            return Err(Response::error(426, "Unsupported Sec-WebSocket-Version")
                .with_header("sec-websocket-version", "13"));
        }
        let key = match header(req, "sec-websocket-key") {
            Some(k) if codec::base64_decoded_len(k.trim_ascii()) == Some(16) => {
                let t = k.trim_ascii();
                k.slice_ref(t)
            }
            _ => return bad("invalid Sec-WebSocket-Key"),
        };
        Ok(WebSocketUpgrade {
            key,
            offered: header(req, "sec-websocket-protocol").cloned(),
            origin: header(req, "origin").cloned(),
            #[cfg(feature = "compression")]
            extensions: header(req, "sec-websocket-extensions").cloned(),
            supported: Vec::new(),
            allowed_origins: None,
            max_message: None,
            auto_pong: true,
            typed_check: None,
            validate: false,
        })
    }
}

impl WebSocketUpgrade {
    /// The subprotocols the server supports, in order of preference; the first one the client offered in
    /// `Sec-WebSocket-Protocol` is selected (none if there is no match).
    pub fn protocols<I, S>(mut self, protocols: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.supported = protocols.into_iter().map(Into::into).collect();
        self
    }

    /// Accepts only these `Origin` values (others get 403). By default the origin is not checked: a browser
    /// sends cookies with a cross-site WebSocket handshake, so restrict it when you authenticate with cookies.
    pub fn origin(mut self, allowed: &[&str]) -> Self {
        self.allowed_origins = Some(allowed.iter().map(|s| s.to_string()).collect());
        self
    }

    /// Maximum size of an assembled message (defaults to `[api] websocket_max_message_size`; larger closes with 1009).
    pub fn max_message_size(mut self, n: usize) -> Self {
        self.max_message = Some(n.max(1));
        self
    }

    /// Whether to answer Pings automatically (defaults to `true`).
    pub fn auto_pong(mut self, on: bool) -> Self {
        self.auto_pong = on;
        self
    }

    /// Declares the JSON message types: `In` is what the client sends, `Out` what the server sends. With
    /// [`validate(true)`](WebSocketUpgrade::validate), received text messages that do not deserialize as `In`
    /// close the connection with 1007. (The OpenAPI document takes the types from the `ws_receive` / `ws_send`
    /// arguments of `#[veil::route]`, since it is generated before any request.)
    pub fn messages<In: serde::de::DeserializeOwned, Out>(mut self) -> Self {
        fn check<T: serde::de::DeserializeOwned>(b: &[u8]) -> bool {
            serde_json::from_slice::<T>(b).is_ok()
        }
        self.typed_check = Some(check::<In>);
        self
    }

    /// Whether to validate received text messages against the type given to
    /// [`messages`](WebSocketUpgrade::messages) (defaults to `false`).
    pub fn validate(mut self, on: bool) -> Self {
        self.validate = on;
        self
    }

    /// The subprotocol that will be selected (`None` if none matched).
    pub fn selected_protocol(&self) -> Option<&str> {
        let offered = self.offered.as_ref()?;
        let offered = std::str::from_utf8(offered).ok()?;
        offered
            .split(',')
            .map(str::trim)
            .find(|o| self.supported.iter().any(|s| s == o))
            .and_then(|o| self.supported.iter().find(|s| *s == o))
            .map(String::as_str)
    }

    /// Answers `101 Switching Protocols` and runs `f` with the connection. When `f` returns, unsent messages are
    /// flushed and the closing handshake is completed (1000 if `f` did not close).
    pub fn on_upgrade<F, Fut>(self, f: F) -> Response
    where
        F: FnOnce(WebSocket) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + 'static,
    {
        if let Some(allowed) = &self.allowed_origins {
            let ok = self
                .origin
                .as_ref()
                .is_some_and(|o| allowed.iter().any(|a| a.as_bytes() == &o[..]));
            if !ok {
                return Response::error(403, "Forbidden: origin not allowed");
            }
        }
        let accept = codec::accept_key(&self.key);
        let protocol = self.selected_protocol().map(str::to_string);
        #[cfg(feature = "compression")]
        let deflate = match &self.extensions {
            Some(e) if COMPRESSION.load(Ordering::Relaxed) => {
                deflate::negotiate(std::iter::once(&e[..]))
            }
            _ => None,
        };
        let opts = Options {
            max_message: self.max_message,
            auto_pong: self.auto_pong,
            text_check: if self.validate {
                self.typed_check
            } else {
                None
            },
            protocol: protocol.clone(),
            #[cfg(feature = "compression")]
            deflate: deflate.as_ref().map(|(_, p)| *p),
        };
        let mut resp =
            Response::switching_protocols("websocket", move |up: Upgraded| serve(up, opts, f))
                .with_header("sec-websocket-accept", accept);
        if let Some(p) = protocol {
            resp = resp.with_header("sec-websocket-protocol", p);
        }
        #[cfg(feature = "compression")]
        if let Some((ext, _)) = deflate {
            resp = resp.with_header("sec-websocket-extensions", ext);
        }
        resp
    }
}

/// Per-connection options decided at the handshake.
struct Options {
    max_message: Option<usize>,
    auto_pong: bool,
    text_check: Option<TextCheck>,
    protocol: Option<String>,
    #[cfg(feature = "compression")]
    deflate: Option<deflate::Params>,
}

// 接続の一生: 利用者の関数を呼び、戻ったら未送信を送って閉じる（`WebSocket` の drop で状態が戻る）。
async fn serve<F, Fut>(up: Upgraded, opts: Options, f: F)
where
    F: FnOnce(WebSocket) -> Fut,
    Fut: Future<Output = ()>,
{
    let home: Rc<Cell<Option<Box<Conn>>>> = Rc::new(Cell::new(None));
    let ws = WebSocket {
        conn: Some(Box::new(Conn::new(up, opts))),
        home: home.clone(),
    };
    f(ws).await;
    if let Some(mut conn) = home.take() {
        conn.finish().await;
    }
}

// ===================== 接続 =====================

// `WsSender` の送信キュー（どのスレッドからでも積める。ワーカーは旗 + 通知 fd で起きる）。
struct SendQueue {
    q: Mutex<VecDeque<Message>>,
    bytes: AtomicUsize,
    closed: AtomicBool,
    wake: crate::runtime::offload::WakeFlag,
}

// `WsSender` が溜められる量（超えたら `WsError::Full`）。
const SEND_QUEUE_MAX_BYTES: usize = 16 * 1024 * 1024;
// 送信のまとめ: これ以下の本文はヘッダと同じバッファへ写して 1 回の書き込みに並べる。
const COPY_MAX: usize = 4 * 1024;
// 送信を溜めずに書き出す量。
const FLUSH_AT: usize = 64 * 1024;
// 自分から Close を送った後、相手の Close を待つ時間。
const CLOSE_WAIT: Duration = Duration::from_secs(5);
// 読み取りバッファに最低限空けておく量。
const READ_MIN: usize = 4 * 1024;

struct Conn {
    up: Upgraded,
    rbuf: BytesMut,
    dec: Decoder,
    // 送る順に並べた断片（ヘッダ・本文）。
    out: Vec<Bytes>,
    // 小さなフレームをまとめるバッファ（`out` の末尾の続き）。
    wbuf: BytesMut,
    out_msgs: u64,
    sent_close: bool,
    // 受信側が終わった（Close を受けた・失敗した・相手が切った）。
    done: bool,
    auto_pong: bool,
    text_check: Option<TextCheck>,
    protocol: Option<String>,
    shutdown: ShutdownSignal,
    queue: Option<Arc<SendQueue>>,
    ping_interval: Option<Duration>,
    idle_timeout: Option<Duration>,
    last_rx: Instant,
    #[cfg(feature = "compression")]
    max_message: usize,
    // permessage-deflate の展開器（交渉したときだけ）
    #[cfg(feature = "compression")]
    inflater: Option<Box<deflate::Inflater>>,
    _gauge: ConnGauge,
}

// 待ちの結果。
enum Wake {
    Readable,
    Queue,
    Shutdown,
    Tick,
}

impl Conn {
    fn new(up: Upgraded, opts: Options) -> Conn {
        let secs = |a: &AtomicU64| match a.load(Ordering::Relaxed) {
            0 => None,
            s => Some(Duration::from_secs(s)),
        };
        let max_message = opts
            .max_message
            .unwrap_or_else(|| MAX_MESSAGE.load(Ordering::Relaxed));
        let max_frame = MAX_FRAME.load(Ordering::Relaxed).min(max_message);
        let shutdown = up.shutdown_signal();
        #[allow(unused_mut)]
        let mut dec = Decoder::new(max_frame, max_message, true);
        #[cfg(feature = "compression")]
        let inflater = opts.deflate.map(|p| {
            dec.deflate = true;
            Box::new(deflate::Inflater::new(p))
        });
        Conn {
            up,
            rbuf: BytesMut::new(),
            dec,
            out: Vec::new(),
            wbuf: BytesMut::new(),
            out_msgs: 0,
            sent_close: false,
            done: false,
            auto_pong: opts.auto_pong,
            text_check: opts.text_check,
            protocol: opts.protocol,
            shutdown,
            queue: None,
            ping_interval: secs(&PING_INTERVAL_SECS),
            idle_timeout: secs(&IDLE_TIMEOUT_SECS),
            last_rx: crate::runtime::time::coarse_now(),
            #[cfg(feature = "compression")]
            max_message,
            #[cfg(feature = "compression")]
            inflater,
            _gauge: ConnGauge::new(),
        }
    }

    // フレームを送信の列に積む（書くのは `flush`）。
    fn queue_frame(&mut self, opcode: u8, payload: &Bytes) {
        #[cfg(feature = "compression")]
        if self.inflater.is_some()
            && opcode < codec::OP_CLOSE
            && payload.len() >= deflate::COMPRESS_MIN
        {
            if let Some(c) = deflate::compress(payload) {
                return self.queue_raw(opcode | 0x40, &c);
            }
        }
        self.queue_raw(opcode, payload)
    }

    // `first` はヘッダの 1 バイト目の FIN 以外（opcode と RSV1）。
    fn queue_raw(&mut self, first: u8, payload: &Bytes) {
        let mut h = [0u8; 10];
        let n = codec::encode_header(&mut h, true, first & 0x0F, payload.len());
        h[0] |= first & 0x40;
        if self.wbuf.capacity() == 0 {
            self.wbuf = crate::pool::accum_buf_get();
        }
        self.wbuf.extend_from_slice(&h[..n]);
        if payload.len() <= COPY_MAX {
            self.wbuf.extend_from_slice(payload);
        } else {
            // 大きな本文は写さない（ヘッダまでを切り出し、本文の `Bytes` をそのまま並べる）
            self.out.push(self.wbuf.split().freeze());
            self.out.push(payload.clone());
        }
        self.out_msgs += 1;
    }

    fn queue_message(&mut self, msg: &Message) -> Result<(), WsError> {
        if self.sent_close {
            return Err(WsError::Closed);
        }
        match msg {
            Message::Text(t) => self.queue_frame(codec::OP_TEXT, &t.0),
            Message::Binary(b) => self.queue_frame(codec::OP_BINARY, b),
            Message::Ping(b) => self.queue_frame(codec::OP_PING, &truncate_control(b)),
            Message::Pong(b) => self.queue_frame(codec::OP_PONG, &truncate_control(b)),
            Message::Close(c) => self.queue_close(c.as_ref().map(|c| (c.code, c.reason.0.clone()))),
        }
        Ok(())
    }

    fn queue_close(&mut self, c: Option<(u16, Bytes)>) {
        if self.sent_close {
            return;
        }
        self.sent_close = true;
        let payload = match c {
            None => Bytes::new(),
            Some((code, reason)) => {
                let mut p = BytesMut::with_capacity(2 + reason.len().min(123));
                p.extend_from_slice(&code.to_be_bytes());
                p.extend_from_slice(&reason[..utf8_floor(&reason, 123)]);
                p.freeze()
            }
        };
        self.queue_frame(codec::OP_CLOSE, &payload);
        if let Some(q) = &self.queue {
            q.closed.store(true, Ordering::Release);
        }
    }

    // 送信の列を 1 回の書き込みで送る（ヘッダと本文を `writev`、小さなフレームは 1 個のバッファ）。
    async fn flush(&mut self) -> Result<(), WsError> {
        if !self.wbuf.is_empty() {
            let tail = self.wbuf.split().freeze();
            self.out.push(tail);
        }
        if self.out.is_empty() {
            return Ok(());
        }
        let r = self.up.write_vectored_all(&self.out).await;
        self.out.clear();
        count_sent(std::mem::take(&mut self.out_msgs));
        if r.is_err() {
            self.done = true;
            if let Some(q) = &self.queue {
                q.closed.store(true, Ordering::Release);
            }
        }
        r.map_err(WsError::Io)
    }

    // 受信バッファに次のフレームが丸ごと届いているか（送信をまとめるかの判断）。
    fn input_pending(&self) -> bool {
        matches!(
            codec::parse_header(&self.rbuf, usize::MAX, false, true),
            Ok(Some(h)) if self.rbuf.len() >= h.hlen + h.len
        )
    }

    // `WsSender` が積んだメッセージを送信の列へ移す。
    fn drain_queue(&mut self) -> bool {
        let Some(q) = self.queue.clone() else {
            return false;
        };
        q.wake.reset();
        let mut moved = false;
        loop {
            let m = q.q.lock().unwrap().pop_front();
            let Some(m) = m else { break };
            q.bytes.fetch_sub(message_len(&m), Ordering::AcqRel);
            moved = true;
            let _ = self.queue_message(&m);
        }
        moved
    }

    // 失敗させる: Close(code) を送って閉じる。
    async fn fail(&mut self, code: u16) -> Option<Result<Message, WsError>> {
        self.queue_close(Some((code, Bytes::new())));
        let _ = self.flush().await;
        let _ = self.up.shutdown().await;
        self.done = true;
        Some(Err(WsError::Protocol(code)))
    }

    async fn recv(&mut self) -> Option<Result<Message, WsError>> {
        loop {
            if self.done {
                return None;
            }
            match self.dec.decode(&mut self.rbuf) {
                Ok(Some(d)) => {
                    count_received();
                    if self.idle_timeout.is_some() {
                        self.last_rx = crate::runtime::time::coarse_now();
                    }
                    match d {
                        Decoded::Text(b) => {
                            if self.text_check.is_some_and(|c| !c(&b)) {
                                return self.fail(codec::CLOSE_INVALID_DATA).await;
                            }
                            return Some(Ok(Message::Text(Utf8Bytes::from_validated(b))));
                        }
                        Decoded::Binary(b) => return Some(Ok(Message::Binary(b))),
                        #[cfg(feature = "compression")]
                        Decoded::Compressed { text, data } => {
                            let max = self.max_message;
                            let r = match self.inflater.as_mut() {
                                Some(inf) => inf.inflate(&data, max),
                                None => Err(codec::CLOSE_PROTOCOL),
                            };
                            let b = match r {
                                Ok(b) => b,
                                Err(code) => return self.fail(code).await,
                            };
                            if !text {
                                return Some(Ok(Message::Binary(b)));
                            }
                            if std::str::from_utf8(&b).is_err() {
                                return self.fail(codec::CLOSE_INVALID_DATA).await;
                            }
                            if self.text_check.is_some_and(|c| !c(&b)) {
                                return self.fail(codec::CLOSE_INVALID_DATA).await;
                            }
                            return Some(Ok(Message::Text(Utf8Bytes::from_validated(b))));
                        }
                        // 交渉していなければ RSV1 は復号器が 1002 にする
                        #[cfg(not(feature = "compression"))]
                        Decoded::Compressed { .. } => {
                            return self.fail(codec::CLOSE_PROTOCOL).await
                        }
                        Decoded::Ping(p) => {
                            if self.auto_pong && !self.sent_close {
                                self.queue_frame(codec::OP_PONG, &p);
                            }
                            return Some(Ok(Message::Ping(p)));
                        }
                        Decoded::Pong(p) => return Some(Ok(Message::Pong(p))),
                        Decoded::Close(c) => {
                            // 閉じる手順: 同じコードを返し（自分が先に送っていなければ）、送信側を閉じる
                            self.queue_close(c.as_ref().map(|(code, _)| (*code, Bytes::new())));
                            let _ = self.flush().await;
                            let _ = self.up.shutdown().await;
                            self.done = true;
                            return Some(Ok(Message::Close(c.map(|(code, reason)| CloseFrame {
                                code,
                                reason: Utf8Bytes::from_validated(reason),
                            }))));
                        }
                    }
                }
                Ok(None) => {}
                Err(code) => return self.fail(code).await,
            }
            // 次のフレームを読む前に、溜まった送信をまとめて書く
            if self.drain_queue() || !self.out.is_empty() || !self.wbuf.is_empty() {
                if let Err(e) = self.flush().await {
                    return Some(Err(e));
                }
            }
            // 待つ間はバッファを持たない（大量の接続が待っているときの常駐量を抑える。B-101）
            if self.rbuf.is_empty() && self.rbuf.capacity() > 0 {
                crate::pool::accum_buf_put(std::mem::take(&mut self.rbuf));
            }
            if self.wbuf.capacity() > 0 {
                crate::pool::accum_buf_put(std::mem::take(&mut self.wbuf));
            }
            match self.wait().await {
                Wake::Readable => {}
                Wake::Queue => continue,
                Wake::Shutdown => {
                    self.queue_close(Some((1001, Bytes::from_static(b"server shutting down"))));
                    let _ = self.flush().await;
                    let _ = self.up.shutdown().await;
                    self.done = true;
                    return Some(Ok(Message::Close(Some(CloseFrame {
                        code: 1001,
                        reason: Utf8Bytes::from_static("server shutting down"),
                    }))));
                }
                Wake::Tick => {
                    if self.sent_close {
                        // 送った Close に相手が応えない
                        let _ = self.up.shutdown().await;
                        self.done = true;
                        return None;
                    }
                    let now = crate::runtime::time::coarse_now();
                    if let Some(idle) = self.idle_timeout {
                        if now.duration_since(self.last_rx) >= idle {
                            self.queue_close(Some((1001, Bytes::from_static(b"idle timeout"))));
                            let _ = self.flush().await;
                            let _ = self.up.shutdown().await;
                            self.done = true;
                            return None;
                        }
                    }
                    if self.ping_interval.is_some() && !self.sent_close {
                        self.queue_frame(codec::OP_PING, &Bytes::new());
                    }
                    continue;
                }
            }
            if self.rbuf.capacity() == 0 {
                self.rbuf = crate::pool::accum_buf_get();
            }
            let want = self
                .dec
                .need()
                .saturating_sub(self.rbuf.len())
                .max(READ_MIN);
            if self.rbuf.capacity() - self.rbuf.len() < want {
                self.rbuf.reserve(want);
            }
            let (res, b) = self.up.read(std::mem::take(&mut self.rbuf)).await;
            self.rbuf = b;
            match res {
                Ok(0) => {
                    // 相手が Close を送らずに切った（1006 相当）
                    self.done = true;
                    return None;
                }
                Ok(_) => {}
                Err(e) => {
                    self.done = true;
                    return Some(Err(WsError::Io(e)));
                }
            }
        }
    }

    // 可読・送信キュー・終了の合図・定期処理（Ping・アイドル）のどれかを待つ（どれも途中で捨ててよい）。
    async fn wait(&mut self) -> Wake {
        let tick = if self.sent_close {
            Some(CLOSE_WAIT)
        } else {
            match (self.ping_interval, self.idle_timeout) {
                (Some(p), Some(i)) => Some(p.min(i)),
                (p, i) => p.or(i),
            }
        };
        let queue = self.queue.clone();
        let up = &self.up;
        let shutdown = &mut self.shutdown;
        let readable = async {
            match tick {
                Some(d) => match crate::runtime::time::timeout(d, up.readable()).await {
                    Ok(_) => Wake::Readable,
                    Err(_) => Wake::Tick,
                },
                // 失敗も読み取りで確かめる（read が同じエラーを返す）
                None => {
                    let _ = up.readable().await;
                    Wake::Readable
                }
            }
        };
        let queued = async {
            match &queue {
                Some(q) => {
                    q.wake.wait().await;
                    Wake::Queue
                }
                None => futures::future::pending().await,
            }
        };
        futures::select_biased! {
            _ = shutdown.fuse() => Wake::Shutdown,
            w = queued.fuse() => w,
            w = readable.fuse() => w,
        }
    }

    // 利用者の関数が戻った後: 未送信を送り、閉じる手順を終える。
    async fn finish(&mut self) {
        if let Some(q) = &self.queue {
            q.closed.store(true, Ordering::Release);
        }
        self.drain_queue();
        if self.done {
            let _ = self.flush().await;
            return;
        }
        let first = !self.sent_close;
        self.queue_close(Some((codec::CLOSE_NORMAL, Bytes::new())));
        if self.flush().await.is_err() {
            return;
        }
        // 相手の Close（か切断）を少し待ってから閉じる
        if first || !self.done {
            let _ = crate::runtime::time::timeout(CLOSE_WAIT, async {
                while let Some(r) = self.recv().await {
                    if matches!(r, Ok(Message::Close(_)) | Err(_)) {
                        break;
                    }
                }
            })
            .await;
        }
        let _ = self.up.shutdown().await;
    }
}

// 制御フレームの本文は 125 バイトまで。
fn truncate_control(b: &Bytes) -> Bytes {
    if b.len() > 125 {
        b.slice(..125)
    } else {
        b.clone()
    }
}

// `b[..n]` が UTF-8 の文字の途中で切れないように `n` 以下で最大の境界を返す。
fn utf8_floor(b: &[u8], n: usize) -> usize {
    if b.len() <= n {
        return b.len();
    }
    let mut i = n;
    while i > 0 && (b[i] & 0xC0) == 0x80 {
        i -= 1;
    }
    i
}

fn message_len(m: &Message) -> usize {
    match m {
        Message::Text(t) => t.len(),
        Message::Binary(b) | Message::Ping(b) | Message::Pong(b) => b.len(),
        Message::Close(_) => 0,
    }
}

// ===================== 公開の型 =====================

/// A WebSocket connection. Receive with [`recv`](WebSocket::recv); messages sent while more input is already
/// buffered are written together before the next read.
///
/// Keep it inside the `on_upgrade` future: when that future returns (or the `WebSocket` is dropped), unsent
/// messages are flushed and the closing handshake is completed.
pub struct WebSocket {
    conn: Option<Box<Conn>>,
    home: Rc<Cell<Option<Box<Conn>>>>,
}

impl std::fmt::Debug for WebSocket {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebSocket")
            .field("protocol", &self.protocol())
            .finish()
    }
}

impl Drop for WebSocket {
    fn drop(&mut self) {
        // 閉じる手順は `serve` が非同期で行う（drop の中では待てない）
        if let Some(c) = self.conn.take() {
            self.home.set(Some(c));
        }
    }
}

impl WebSocket {
    fn c(&mut self) -> &mut Conn {
        self.conn
            .as_mut()
            .expect("WebSocket connection is present until drop")
    }

    /// The next message. `None` after the connection closed (Close received and answered, or the peer left);
    /// `Some(Err(_))` when the peer violated the protocol (the connection is then closed with the code).
    /// A Close frame is returned once as `Message::Close`. When graceful shutdown starts, a Close 1001 is sent
    /// and returned as `Message::Close(Some(1001))`.
    pub async fn recv(&mut self) -> Option<Result<Message, WsError>> {
        self.c().recv().await
    }

    /// Sends a message. When the next message is already buffered (the client sent several at once), the write
    /// is deferred until the next `recv` so that the replies go out together.
    pub async fn send(&mut self, msg: Message) -> Result<(), WsError> {
        let c = self.c();
        c.queue_message(&msg)?;
        if c.out.len() + c.wbuf.len() > FLUSH_AT || !c.input_pending() || c.sent_close {
            c.flush().await?;
        }
        Ok(())
    }

    /// Sends several messages with one write.
    pub async fn send_batch(&mut self, msgs: &[Message]) -> Result<(), WsError> {
        let c = self.c();
        for m in msgs {
            c.queue_message(m)?;
        }
        c.flush().await
    }

    /// Writes messages deferred by `send`.
    pub async fn flush(&mut self) -> Result<(), WsError> {
        self.c().flush().await
    }

    /// Sends a Ping.
    pub async fn ping(&mut self, payload: Bytes) -> Result<(), WsError> {
        self.send(Message::Ping(payload)).await
    }

    /// Starts the closing handshake with `code` and `reason`, then waits (at most 5 seconds) for the peer's Close.
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<(), WsError> {
        let c = self.c();
        if c.sent_close {
            return Err(WsError::Closed);
        }
        c.queue_close(Some((code, Bytes::copy_from_slice(reason.as_bytes()))));
        c.flush().await?;
        let _ = crate::runtime::time::timeout(CLOSE_WAIT, async {
            while let Some(r) = c.recv().await {
                if matches!(r, Ok(Message::Close(_)) | Err(_)) {
                    break;
                }
            }
        })
        .await;
        let _ = c.up.shutdown().await;
        c.done = true;
        Ok(())
    }

    /// The selected subprotocol (`Sec-WebSocket-Protocol`), if any.
    pub fn protocol(&self) -> Option<&str> {
        self.conn.as_ref().and_then(|c| c.protocol.as_deref())
    }

    /// The client's IP address.
    pub fn client_ip(&self) -> &str {
        self.conn.as_ref().map(|c| c.up.client_ip()).unwrap_or("")
    }

    /// Splits into a receiver and a sender. The sender is `Clone + Send` and may be used from other tasks and
    /// threads (for example to broadcast); the receiver drives the connection, so keep calling
    /// [`WsReceiver::recv`] for queued messages to be written.
    pub fn split(mut self) -> (WsSender, WsReceiver) {
        let q = Arc::new(SendQueue {
            q: Mutex::new(VecDeque::new()),
            bytes: AtomicUsize::new(0),
            closed: AtomicBool::new(false),
            wake: crate::runtime::offload::WakeFlag::new(),
        });
        let c = self.c();
        if c.sent_close || c.done {
            q.closed.store(true, Ordering::Release);
        }
        c.queue = Some(q.clone());
        (WsSender { q }, WsReceiver { ws: self })
    }
}

/// The receiving half of a split [`WebSocket`] (it also writes what the [`WsSender`]s queued).
#[derive(Debug)]
pub struct WsReceiver {
    ws: WebSocket,
}

impl WsReceiver {
    /// The next message (see [`WebSocket::recv`]).
    pub async fn recv(&mut self) -> Option<Result<Message, WsError>> {
        self.ws.recv().await
    }

    /// Sends a message directly (see [`WebSocket::send`]).
    pub async fn send(&mut self, msg: Message) -> Result<(), WsError> {
        self.ws.send(msg).await
    }

    /// Starts the closing handshake (see [`WebSocket::close`]).
    pub async fn close(&mut self, code: u16, reason: &str) -> Result<(), WsError> {
        self.ws.close(code, reason).await
    }

    /// Writes what the [`WsSender`]s queued without waiting for input (for a caller that has stopped calling
    /// `recv` for a while, for example because its consumer is behind).
    pub async fn flush(&mut self) -> Result<(), WsError> {
        let c = self.ws.c();
        c.drain_queue();
        c.flush().await
    }
}

/// The sending half of a split [`WebSocket`]: a queue that any task or thread can push to.
#[derive(Clone)]
pub struct WsSender {
    q: Arc<SendQueue>,
}

impl std::fmt::Debug for WsSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WsSender")
    }
}

impl WsSender {
    /// Queues a message (does not wait). `Err(Closed)` after the connection closed, `Err(Full)` when 16 MiB are
    /// already queued.
    pub fn send(&self, msg: Message) -> Result<(), WsError> {
        if self.q.closed.load(Ordering::Acquire) {
            return Err(WsError::Closed);
        }
        let n = message_len(&msg);
        if self.q.bytes.fetch_add(n, Ordering::AcqRel) + n > SEND_QUEUE_MAX_BYTES {
            self.q.bytes.fetch_sub(n, Ordering::AcqRel);
            return Err(WsError::Full);
        }
        self.q.q.lock().unwrap().push_back(msg);
        self.q.wake.notify();
        Ok(())
    }

    /// Whether the connection has closed.
    pub fn is_closed(&self) -> bool {
        self.q.closed.load(Ordering::Acquire)
    }
}

// ===================== ブロッキングなコールバック（C 用） =====================

// F-262: C の `veil_ws_accept` が使う。メッセージごとにハンドラ用プールでコールバックを 1 回呼ぶ
// （同じ接続のコールバックは並行しない。送信は `WsSender` のキュー経由なので、どのスレッドからでもよい）。
/// Callbacks of a WebSocket served on the handler pool ([`WebSocketUpgrade::on_upgrade_blocking`]).
pub trait WsSession: Send {
    /// A text or binary message.
    fn on_message(&mut self, msg: Message);
    /// The connection closed (`code` is the peer's close code: 1005 without a code, 1006 if it left without a
    /// Close frame, or the code veil closed with after a protocol error).
    fn on_close(&mut self, _code: u16) {}
}

impl WebSocketUpgrade {
    /// Like [`on_upgrade`](WebSocketUpgrade::on_upgrade), but the session's callbacks run on the handler pool
    /// (for blocking code such as C). `connect` runs first on the pool with a [`WsSender`] (which any thread may
    /// use to send) and returns the session; `None` closes the connection.
    pub fn on_upgrade_blocking<F>(self, connect: F) -> Response
    where
        F: FnOnce(WsSender) -> Option<Box<dyn WsSession>> + Send + 'static,
    {
        self.on_upgrade(move |ws: WebSocket| async move {
            let (tx, mut rx) = ws.split();
            let session = match crate::runtime::offload::offload_handler(move || connect(tx)).await
            {
                Some(Some(s)) => Arc::new(Mutex::new(s)),
                _ => return,
            };
            let mut code = 1006;
            while let Some(r) = rx.recv().await {
                match r {
                    Ok(Message::Close(c)) => {
                        code = c.map(|c| c.code).unwrap_or(1005);
                        break;
                    }
                    Ok(m @ (Message::Text(_) | Message::Binary(_))) => {
                        let s = session.clone();
                        let done = crate::runtime::offload::offload_handler(move || {
                            s.lock().unwrap_or_else(|e| e.into_inner()).on_message(m)
                        })
                        .await;
                        if done.is_none() {
                            // コールバックが panic した
                            code = 1011;
                            let _ = rx.close(1011, "").await;
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(WsError::Protocol(c)) => {
                        code = c;
                        break;
                    }
                    Err(_) => break,
                }
            }
            let _ = crate::runtime::offload::offload_handler(move || {
                session
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .on_close(code)
            })
            .await;
        })
    }
}

/// Fuzzing entry point (`fuzz/fuzz_targets/ws_frame_decode.rs`): decodes `data` whole and in small pieces,
/// with and without permessage-deflate.
pub(crate) fn fuzz_decode(data: &[u8]) {
    for deflate in [false, true] {
        for step in [data.len().max(1), 1, 7] {
            let mut d = Decoder::new(1 << 16, 1 << 18, data.first().is_some_and(|b| b & 1 == 0));
            d.deflate = deflate;
            let mut buf = BytesMut::new();
            for chunk in data.chunks(step) {
                buf.extend_from_slice(chunk);
                loop {
                    match d.decode(&mut buf) {
                        Ok(Some(_d)) =>
                        {
                            #[cfg(feature = "compression")]
                            if let Decoded::Compressed { data, .. } = _d {
                                let mut inf = deflate::Inflater::new(deflate::Params {
                                    client_no_context_takeover: false,
                                });
                                let _ = inf.inflate(&data, 1 << 18);
                            }
                        }
                        Ok(None) => break,
                        Err(_) => return,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::extract::FromRequest;

    fn req(h: &[(&'static str, &'static str)]) -> Request {
        Request::new(
            "GET",
            "/ws",
            h.iter()
                .map(|(n, v)| {
                    (
                        Bytes::from_static(n.as_bytes()),
                        Bytes::from_static(v.as_bytes()),
                    )
                })
                .collect(),
            "",
        )
    }

    const OK: &[(&str, &str)] = &[
        ("Upgrade", "websocket"),
        ("Connection", "keep-alive, Upgrade"),
        ("Sec-WebSocket-Version", "13"),
        ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
    ];

    #[test]
    fn handshake_accepts_and_computes_accept() {
        let up = WebSocketUpgrade::from_request(&req(OK)).unwrap();
        let resp = up.on_upgrade(|_ws| async {});
        assert_eq!(resp.status_code(), 101);
        assert_eq!(
            resp.header("sec-websocket-accept"),
            Some(&b"s3pPLMBiTxaQ9kYGzzhZRbK+xOo="[..])
        );
        assert!(resp.header("sec-websocket-protocol").is_none());
    }

    #[test]
    fn handshake_errors() {
        let status =
            |h: &[(&'static str, &'static str)]| match WebSocketUpgrade::from_request(&req(h)) {
                Ok(_) => 101,
                Err(r) => r.status_code(),
            };
        // 版違いは 426 + Sec-WebSocket-Version: 13
        let mut v = OK.to_vec();
        v[2] = ("Sec-WebSocket-Version", "8");
        let r = WebSocketUpgrade::from_request(&req(&v)).unwrap_err();
        assert_eq!(r.status_code(), 426);
        assert_eq!(r.header("sec-websocket-version"), Some(&b"13"[..]));
        // 鍵の長さ・形式
        let mut v = OK.to_vec();
        v[3] = ("Sec-WebSocket-Key", "c2hvcnQ=");
        assert_eq!(status(&v), 400);
        let mut v = OK.to_vec();
        v[3] = ("Sec-WebSocket-Key", "!!!!!!!!!!!!!!!!!!!!!!!!");
        assert_eq!(status(&v), 400);
        // Upgrade・Connection が無い（アップグレードのヘッダの無い GET /ws は 4xx）
        assert_eq!(status(&OK[1..]), 400);
        assert_eq!(status(&[OK[0], OK[2], OK[3]]), 400);
        assert_eq!(status(&[]), 400);
        // HTTP/2 では使えない
        let mut r2 = req(OK);
        r2.version = crate::api::request::HttpVersion::Http2;
        assert_eq!(
            WebSocketUpgrade::from_request(&r2)
                .unwrap_err()
                .status_code(),
            400
        );
    }

    #[test]
    fn subprotocol_and_origin() {
        let mut v = OK.to_vec();
        v.push(("Sec-WebSocket-Protocol", "x.v2, chat.v1"));
        v.push(("Origin", "https://a.example"));
        let up = WebSocketUpgrade::from_request(&req(&v))
            .unwrap()
            .protocols(["chat.v1", "x.v2"]);
        assert_eq!(up.selected_protocol(), Some("x.v2"));
        let resp = up.on_upgrade(|_ws| async {});
        assert_eq!(resp.header("sec-websocket-protocol"), Some(&b"x.v2"[..]));
        let up = WebSocketUpgrade::from_request(&req(&v))
            .unwrap()
            .protocols(["other"]);
        assert_eq!(up.selected_protocol(), None);
        let resp = WebSocketUpgrade::from_request(&req(&v))
            .unwrap()
            .origin(&["https://b.example"])
            .on_upgrade(|_ws| async {});
        assert_eq!(resp.status_code(), 403);
        let resp = WebSocketUpgrade::from_request(&req(&v))
            .unwrap()
            .origin(&["https://a.example"])
            .on_upgrade(|_ws| async {});
        assert_eq!(resp.status_code(), 101);
    }

    #[test]
    fn utf8_floor_and_text() {
        assert_eq!(utf8_floor("aé".as_bytes(), 2), 1);
        assert_eq!(utf8_floor(b"abc", 5), 3);
        let t = Utf8Bytes::try_from(Bytes::from_static(b"hi")).unwrap();
        assert_eq!(t, "hi");
        assert!(Utf8Bytes::try_from(Bytes::from_static(&[0xff])).is_err());
    }
}
