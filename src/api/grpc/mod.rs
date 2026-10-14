//! gRPC サーバ（API ライブラリ、F-197）。メッセージは prost、ワイヤ処理は veil の `src/grpc/`
//! （5 バイトのフレーミング・`grpc-status`/`grpc-message`・`grpc-timeout`・gzip/deflate）。tokio・tonic に依存しない。
//!
//! ```ignore
//! use veil::api::grpc::{Request, Response, Status};
//! App::new()
//!     .grpc_unary("/helloworld.Greeter/SayHello", |req: Request<HelloRequest>| async move {
//!         Ok(Response::new(HelloReply { message: format!("Hello {}", req.get_ref().name) }))
//!     })
//!     .run()
//! ```
//!
//! 実体は F-188 のハンドラ（`POST /<package>.<Service>/<Method>`、要求本文はストリーミング受信）。
//! 応答はストリーミング応答（DATA の後にトレーラ `grpc-status`）で送る。HTTP/2・HTTP/3 で動き、
//! HTTP/1.1 で来た要求には `grpc-status: 13` を返す。`.proto` からのコード生成は `veil-build` クレート。

pub mod health;

/// `veil-build` が `OUT_DIR` に書いた `<package>.rs` を取り込む（`veil::include_proto!("helloworld");`）。
#[macro_export]
macro_rules! include_proto {
    ($package:literal) => {
        include!(concat!(env!("OUT_DIR"), "/", $package, ".rs"));
    };
}

use super::app::App;
use super::handler::{HandlerFn, HandlerFuture, OperationMeta};
use super::request::HttpVersion;
use super::stream::{BodyChunk, RequestBodyStream};
use crate::grpc::compression::GrpcCompression;
use crate::grpc::framing::GrpcFrameDecoder;
use bytes::{BufMut, Bytes, BytesMut};
use futures::{Stream, StreamExt};
use std::future::Future;
use std::marker::PhantomData;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

pub use crate::grpc::status::GrpcStatusCode as Code;
/// 生成コード・利用者が同じ版の prost を使えるように再エクスポートする。
pub use prost;

/// メタデータ（要求ヘッダ・応答ヘッダ・トレーラ）。名前は小文字。
pub type Metadata = Vec<(Bytes, Bytes)>;

/// 応答ストリームの型（生成コードのサーバ/双方向ストリーミングの戻り値）。
pub type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// 受信メッセージの上限（`[api] grpc_max_message_size`、既定 4MiB）。
static MAX_MESSAGE_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_MESSAGE_SIZE);

/// gRPC の慣習的な既定上限。
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

pub(crate) fn set_max_message_size(n: usize) {
    MAX_MESSAGE_SIZE.store(n.max(1), Ordering::Relaxed);
}

/// 現在の受信メッセージ上限。
pub fn max_message_size() -> usize {
    MAX_MESSAGE_SIZE.load(Ordering::Relaxed)
}

// ===================== Status =====================

/// gRPC のステータス（コード・メッセージ・追加のトレーラ）。
#[derive(Debug, Clone)]
pub struct Status {
    code: Code,
    message: String,
    metadata: Metadata,
}

macro_rules! status_ctor {
    ($($name:ident => $code:ident),* $(,)?) => {
        $(
            #[doc = concat!("`", stringify!($code), "` のステータス。")]
            pub fn $name(message: impl Into<String>) -> Status {
                Status::new(Code::$code, message)
            }
        )*
    };
}

impl Status {
    pub fn new(code: Code, message: impl Into<String>) -> Status {
        Status {
            code,
            message: message.into(),
            metadata: Vec::new(),
        }
    }

    status_ctor! {
        cancelled => Cancelled,
        unknown => Unknown,
        invalid_argument => InvalidArgument,
        deadline_exceeded => DeadlineExceeded,
        not_found => NotFound,
        already_exists => AlreadyExists,
        permission_denied => PermissionDenied,
        resource_exhausted => ResourceExhausted,
        failed_precondition => FailedPrecondition,
        aborted => Aborted,
        out_of_range => OutOfRange,
        unimplemented => Unimplemented,
        internal => Internal,
        unavailable => Unavailable,
        data_loss => DataLoss,
        unauthenticated => Unauthenticated,
    }

    pub fn code(&self) -> Code {
        self.code
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    /// トレーラに足すメタデータ（名前は小文字にする）。
    pub fn with_metadata(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Status {
        self.metadata.push((lower(name.into()), value.into()));
        self
    }

    /// `grpc-status`・`grpc-message`（パーセントエンコード）と追加のメタデータ。
    pub(crate) fn to_trailers(&self) -> Metadata {
        let mut out = Vec::with_capacity(2 + self.metadata.len());
        let mut buf = itoa::Buffer::new();
        out.push((
            Bytes::from_static(b"grpc-status"),
            Bytes::copy_from_slice(buf.format(self.code.as_u8()).as_bytes()),
        ));
        if !self.message.is_empty() {
            out.push((
                Bytes::from_static(b"grpc-message"),
                Bytes::from(percent_encode(&self.message)),
            ));
        }
        out.extend(self.metadata.iter().cloned());
        out
    }
}

impl std::fmt::Display for Status {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "grpc-status {} ({}): {}",
            self.code.as_u8(),
            self.code,
            self.message
        )
    }
}

impl std::error::Error for Status {}

/// gRPC の `grpc-message` のパーセントエンコード（UTF-8 のバイト単位。0x20〜0x7E の `%` 以外はそのまま）。
fn percent_encode(s: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    for &b in s.as_bytes() {
        if (0x20..=0x7e).contains(&b) && b != b'%' {
            out.push(b);
        } else {
            out.push(b'%');
            out.push(HEX[(b >> 4) as usize]);
            out.push(HEX[(b & 0xf) as usize]);
        }
    }
    out
}

const HEX: &[u8; 16] = b"0123456789ABCDEF";

/// `percent_encode` の逆（クライアント・テスト用）。
pub fn percent_decode(v: &[u8]) -> String {
    let mut out = Vec::with_capacity(v.len());
    let mut i = 0;
    while i < v.len() {
        if v[i] == b'%' && i + 2 < v.len() {
            let hex = |c: u8| (c as char).to_digit(16);
            if let (Some(h), Some(l)) = (hex(v[i + 1]), hex(v[i + 2])) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(v[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn lower(b: Bytes) -> Bytes {
    if b.iter().any(|c| c.is_ascii_uppercase()) {
        Bytes::from(b.to_ascii_lowercase())
    } else {
        b
    }
}

// ===================== Request / Response =====================

/// gRPC の要求（メタデータ + メッセージ、またはメッセージのストリーム）。
#[derive(Debug)]
pub struct Request<T> {
    metadata: Metadata,
    message: T,
    deadline: Option<Instant>,
    client_ip: IpAddr,
}

impl<T> Request<T> {
    /// テスト・直接呼び出し用。
    pub fn new(message: T) -> Request<T> {
        Request {
            metadata: Vec::new(),
            message,
            deadline: None,
            client_ip: IpAddr::from([127, 0, 0, 1]),
        }
    }

    pub fn get_ref(&self) -> &T {
        &self.message
    }

    pub fn get_mut(&mut self) -> &mut T {
        &mut self.message
    }

    pub fn into_inner(self) -> T {
        self.message
    }

    /// 要求メタデータ（予約済みヘッダ `content-type`・`te`・`grpc-timeout`・`grpc-encoding` 等を除く要求ヘッダ）。
    pub fn metadata(&self) -> &[(Bytes, Bytes)] {
        &self.metadata
    }

    /// メタデータの値（名前は大文字小文字を区別しない）。
    pub fn metadata_value(&self, name: &str) -> Option<&[u8]> {
        self.metadata
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| v.as_ref())
    }

    /// `grpc-timeout` から求めた期限。
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    pub fn client_ip(&self) -> IpAddr {
        self.client_ip
    }

    /// メッセージだけ差し替える（メタデータ・期限は保つ）。
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Request<U> {
        Request {
            metadata: self.metadata,
            message: f(self.message),
            deadline: self.deadline,
            client_ip: self.client_ip,
        }
    }
}

/// gRPC の応答（メタデータ = 応答ヘッダ、メッセージまたはストリーム、トレーラ）。
#[derive(Debug)]
pub struct Response<T> {
    metadata: Metadata,
    message: T,
    trailers: Metadata,
}

impl<T> Response<T> {
    pub fn new(message: T) -> Response<T> {
        Response {
            metadata: Vec::new(),
            message,
            trailers: Vec::new(),
        }
    }

    pub fn get_ref(&self) -> &T {
        &self.message
    }

    pub fn into_inner(self) -> T {
        self.message
    }

    /// 応答ヘッダに足す（名前は小文字にする）。
    pub fn with_metadata(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        self.metadata.push((lower(name.into()), value.into()));
        self
    }

    /// 正常終了のトレーラに足す（`grpc-status: 0` の後ろ）。
    pub fn with_trailer(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        self.trailers.push((lower(name.into()), value.into()));
        self
    }

    pub fn metadata(&self) -> &[(Bytes, Bytes)] {
        &self.metadata
    }
}

// ===================== Streaming（受信） =====================

/// 受信メッセージのストリーム（client streaming・bidi の要求）。届いた本文を gRPC のフレームに切り出し、
/// 圧縮を戻して `T::decode` する。上限超過は `RESOURCE_EXHAUSTED`、壊れたメッセージは `INTERNAL`。
pub struct Streaming<T> {
    body: RequestBodyStream,
    decoder: GrpcFrameDecoder,
    encoding: GrpcCompression,
    body_done: bool,
    finished: bool,
    _t: PhantomData<fn() -> T>,
}

impl<T> std::fmt::Debug for Streaming<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streaming")
            .field("encoding", &self.encoding)
            .finish()
    }
}

impl<T: prost::Message + Default> Streaming<T> {
    fn new(body: RequestBodyStream, encoding: GrpcCompression) -> Streaming<T> {
        Streaming {
            body,
            decoder: GrpcFrameDecoder::with_max_size(max_message_size()),
            encoding,
            body_done: false,
            finished: false,
            _t: PhantomData,
        }
    }

    /// 次のメッセージ（終わりなら `Ok(None)`）。
    pub async fn message(&mut self) -> Result<Option<T>, Status> {
        self.next().await.transpose()
    }

    fn decode_frame(&self, compressed: bool, data: Vec<u8>) -> Result<T, Status> {
        let data = if compressed {
            if self.encoding == GrpcCompression::Identity {
                return Err(Status::internal("compressed message without grpc-encoding"));
            }
            // B-127: 解凍後の大きさは上限 + 1 バイトで打ち切る（全部解凍してから比べない）
            match self.encoding.decompress_limited(&data, max_message_size()) {
                Ok(d) => d,
                Err(crate::grpc::framing::GrpcError::MessageTooLarge { max, .. }) => {
                    return Err(Status::resource_exhausted(format!(
                        "decompressed message larger than max ({})",
                        max
                    )))
                }
                Err(e) => {
                    return Err(Status::internal(format!(
                        "failed to decompress message: {}",
                        e
                    )))
                }
            }
        } else {
            data
        };
        if data.len() > max_message_size() {
            return Err(Status::resource_exhausted(format!(
                "message larger than max ({} vs. {})",
                data.len(),
                max_message_size()
            )));
        }
        T::decode(&data[..])
            .map_err(|e| Status::internal(format!("failed to decode message: {}", e)))
    }
}

impl<T: prost::Message + Default> Stream for Streaming<T> {
    type Item = Result<T, Status>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = &mut *self;
        loop {
            if this.finished {
                return Poll::Ready(None);
            }
            match this.decoder.decode_next() {
                Ok(Some(frame)) => {
                    let r = this.decode_frame(frame.compressed, frame.data);
                    if r.is_err() {
                        this.finished = true;
                    }
                    return Poll::Ready(Some(r));
                }
                Ok(None) => {}
                Err(crate::grpc::framing::GrpcError::MessageTooLarge { size, max }) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(Status::resource_exhausted(format!(
                        "message larger than max ({} vs. {})",
                        size, max
                    )))));
                }
                Err(e) => {
                    this.finished = true;
                    return Poll::Ready(Some(Err(Status::internal(e.to_string()))));
                }
            }
            if this.body_done {
                this.finished = true;
                if this.decoder.has_pending() {
                    return Poll::Ready(Some(Err(Status::internal(
                        "request body ended in the middle of a message",
                    ))));
                }
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.body).poll_next(cx) {
                Poll::Ready(Some(chunk)) => this.decoder.push(&chunk),
                Poll::Ready(None) => this.body_done = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ===================== 送信 =====================

/// メッセージを 5 バイトの見出し付きで符号化する（非圧縮）。
pub(crate) fn encode_message<T: prost::Message>(m: &T) -> Bytes {
    let len = m.encoded_len();
    let mut buf = BytesMut::with_capacity(5 + len);
    buf.put_u8(0);
    buf.put_u32(len as u32);
    // 容量を確保済みなので失敗しない
    let _ = m.encode(&mut buf);
    buf.freeze()
}

/// 応答のメッセージ列を本文の断片とトレーラに変える（最後に `grpc-status`、途中の `Err` はその状態で終える）。
struct GrpcBody<S> {
    inner: S,
    trailers: Option<Metadata>,
    deadline: Option<Instant>,
}

impl<S, T> Stream for GrpcBody<S>
where
    S: Stream<Item = Result<T, Status>> + Unpin,
    T: prost::Message,
{
    type Item = BodyChunk;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<BodyChunk>> {
        let this = &mut *self;
        let Some(ok_trailers) = this.trailers.as_ref() else {
            return Poll::Ready(None);
        };
        if this.deadline.is_some_and(|d| Instant::now() >= d) {
            this.trailers = None;
            return Poll::Ready(Some(BodyChunk::Trailers(
                Status::deadline_exceeded("deadline exceeded").to_trailers(),
            )));
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(m))) => Poll::Ready(Some(BodyChunk::Data(encode_message(&m)))),
            Poll::Ready(Some(Err(s))) => {
                this.trailers = None;
                Poll::Ready(Some(BodyChunk::Trailers(s.to_trailers())))
            }
            Poll::Ready(None) => {
                let mut t = Status::new(Code::Ok, "").to_trailers();
                t.extend(ok_trailers.iter().cloned());
                this.trailers = None;
                Poll::Ready(Some(BodyChunk::Trailers(t)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// 受け付ける `grpc-encoding`（`compression` feature 無しのビルドでは identity だけ。B-127）。
const GRPC_ACCEPT_ENCODING: &str = if cfg!(feature = "compression") {
    "identity,gzip,deflate"
} else {
    "identity"
};

fn grpc_head(status: u16) -> super::response::Response {
    super::response::Response::new(status)
        .with_header("content-type", "application/grpc")
        .with_header("grpc-accept-encoding", GRPC_ACCEPT_ENCODING)
}

/// エラーだけを返す応答（Trailers-Only: `grpc-status` を応答ヘッダに載せて本文無しで終える）。
fn trailers_only(status: &Status) -> super::response::Response {
    let mut r = grpc_head(200);
    r.headers.extend(status.to_trailers());
    r
}

/// メッセージ列の応答。
fn stream_response<S, T>(resp: Response<S>, deadline: Option<Instant>) -> super::response::Response
where
    S: Stream<Item = Result<T, Status>> + Send + Unpin + 'static,
    T: prost::Message + 'static,
{
    let Response {
        metadata,
        message,
        trailers,
    } = resp;
    let mut head = grpc_head(200);
    head.headers.extend(metadata);
    let body = GrpcBody {
        inner: message,
        trailers: Some(trailers),
        deadline,
    };
    let mut r = super::response::Response::stream(200, body);
    r.headers = head.headers;
    r
}

// ===================== 受け付け =====================

/// 要求ヘッダから読み取った呼び出しの情報。
struct Call {
    metadata: Metadata,
    deadline: Option<Instant>,
    encoding: GrpcCompression,
    client_ip: IpAddr,
}

/// gRPC の要求として受け付けられるかを見る（だめなら返す応答）。
fn accept(req: &super::request::Request) -> Result<Call, super::response::Response> {
    let ct = req.header("content-type").unwrap_or_default();
    // gRPC-Web（`application/grpc-web*`）は本文内のトレーラ等の形式が違う。通常の gRPC として扱うと
    // ブラウザが読めない応答を返すので断る（B-127 / F-209 #4）。
    if !crate::grpc::headers::is_grpc_content_type(ct.as_bytes()) || is_grpc_web(ct.as_bytes()) {
        return Err(super::response::Response::error(
            415,
            "Unsupported Media Type (expected application/grpc)",
        ));
    }
    if req.version == HttpVersion::Http11 {
        return Err(trailers_only(&Status::internal(
            "gRPC requires HTTP/2 or HTTP/3",
        )));
    }
    let encoding = match req.header("grpc-encoding") {
        None => GrpcCompression::Identity,
        Some(v) => match GrpcCompression::from_bytes(v.as_bytes()) {
            Some(c) if c.is_supported() => c,
            _ => {
                return Err(trailers_only(&Status::unimplemented(format!(
                    "grpc-encoding '{}' is not supported",
                    v
                ))))
            }
        },
    };
    let deadline = req
        .header("grpc-timeout")
        .and_then(|v| crate::grpc::headers::parse_grpc_timeout(v.as_bytes()))
        .map(|d| Instant::now() + d);
    let metadata = req
        .headers()
        .iter()
        .filter(|(n, _)| {
            !matches!(
                &n.to_ascii_lowercase()[..],
                b"content-type"
                    | b"te"
                    | b"grpc-timeout"
                    | b"grpc-encoding"
                    | b"grpc-accept-encoding"
                    | b"content-length"
                    | b"connection"
            )
        })
        .cloned()
        .collect();
    Ok(Call {
        metadata,
        deadline,
        encoding,
        client_ip: req.client_ip(),
    })
}

/// `application/grpc-web`・`application/grpc-web+proto`・`application/grpc-web-text` 等。
fn is_grpc_web(ct: &[u8]) -> bool {
    const P: &[u8] = b"application/grpc-web";
    ct.len() >= P.len() && ct[..P.len()].eq_ignore_ascii_case(P)
}

/// 期限付きで Future を待つ（期限切れは `DEADLINE_EXCEEDED`）。
async fn with_deadline<F, T>(deadline: Option<Instant>, fut: F) -> Result<T, Status>
where
    F: Future<Output = Result<T, Status>>,
{
    match deadline {
        None => fut.await,
        Some(d) => {
            let left = d.saturating_duration_since(Instant::now());
            if left == Duration::ZERO {
                return Err(Status::deadline_exceeded("deadline exceeded"));
            }
            match crate::runtime::time::timeout(left, fut).await {
                Ok(r) => r,
                Err(_) => Err(Status::deadline_exceeded("deadline exceeded")),
            }
        }
    }
}

/// 単項の要求メッセージを 1 つ読む（0 個・2 個以上はエラー）。
async fn read_single<T: prost::Message + Default>(mut s: Streaming<T>) -> Result<T, Status> {
    let Some(first) = s.message().await? else {
        return Err(Status::internal("missing request message"));
    };
    if s.message().await?.is_some() {
        return Err(Status::internal(
            "unary call received more than one request message",
        ));
    }
    Ok(first)
}

fn request_parts<T: prost::Message + Default>(
    req: &super::request::Request,
    call: &Call,
) -> Streaming<T> {
    Streaming::new(req.take_body_stream(), call.encoding)
}

fn make_request<T>(call: Call, message: T) -> (Request<T>, Option<Instant>) {
    let deadline = call.deadline;
    (
        Request {
            metadata: call.metadata,
            message,
            deadline,
            client_ip: call.client_ip,
        },
        deadline,
    )
}

/// 単発のメッセージを 1 要素のストリームにする。
fn once<T: Send + 'static>(
    m: T,
) -> futures::stream::Once<futures::future::Ready<Result<T, Status>>> {
    futures::stream::once(futures::future::ready(Ok(m)))
}

fn grpc_route(mut app: App, path: &str, func: HandlerFn) -> App {
    let meta = OperationMeta {
        operation_id: path.trim_start_matches('/').replace(['/', '.'], "_"),
        hidden: true,
        ..Default::default()
    };
    // 同じサービスの未定義メソッドは UNIMPLEMENTED（既定の 404 では gRPC のクライアントが本文を
    // メッセージとして読もうとして INTERNAL になる）。定義済みのメソッドの方が具体的なので優先される。
    if let Some(service) = path
        .strip_prefix('/')
        .and_then(|p| p.split_once('/'))
        .map(|(svc, _)| svc)
    {
        let fallback = format!("/{}/{{*method}}", service);
        if !app.routes.iter().any(|r| r.path == fallback) {
            let func: HandlerFn = Arc::new(|req: super::request::Request| -> HandlerFuture {
                Box::pin(async move {
                    match accept(&req) {
                        Ok(_) => trailers_only(&Status::unimplemented(format!(
                            "method {} is not implemented",
                            req.path()
                        ))),
                        Err(r) => r,
                    }
                })
            });
            let meta = OperationMeta {
                operation_id: format!("{}_unimplemented", service.replace('.', "_")),
                hidden: true,
                ..Default::default()
            };
            app = app.route_fn(&meta.operation_id.clone(), &["POST"], &fallback, func, meta);
        }
    }
    app.route_fn_streaming(path, &["POST"], path, func, meta)
}

impl App {
    /// 単項 RPC（1 要求 → 1 応答）を登録する。`path` は `/<package>.<Service>/<Method>`。
    pub fn grpc_unary<Req, Resp, F, Fut>(self, path: &str, f: F) -> App
    where
        Req: prost::Message + Default + 'static,
        Resp: prost::Message + Send + 'static,
        F: Fn(Request<Req>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response<Resp>, Status>> + 'static,
    {
        let f = Arc::new(f);
        let func: HandlerFn = Arc::new(move |req| -> HandlerFuture {
            let f = f.clone();
            Box::pin(async move {
                let call = match accept(&req) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                let body: Streaming<Req> = request_parts(&req, &call);
                let deadline = call.deadline;
                let r = with_deadline(deadline, async move {
                    let m = read_single(body).await?;
                    let (req, _) = make_request(call, m);
                    f(req).await
                })
                .await;
                match r {
                    Ok(resp) => {
                        let Response {
                            metadata,
                            message,
                            trailers,
                        } = resp;
                        stream_response(
                            Response {
                                metadata,
                                message: once(message),
                                trailers,
                            },
                            None,
                        )
                    }
                    Err(s) => trailers_only(&s),
                }
            })
        });
        grpc_route(self, path, func)
    }

    /// サーバストリーミング RPC（1 要求 → 応答のストリーム）を登録する。
    pub fn grpc_server_streaming<Req, Resp, S, F, Fut>(self, path: &str, f: F) -> App
    where
        Req: prost::Message + Default + 'static,
        Resp: prost::Message + 'static,
        S: Stream<Item = Result<Resp, Status>> + Send + 'static,
        F: Fn(Request<Req>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response<S>, Status>> + 'static,
    {
        let f = Arc::new(f);
        let func: HandlerFn = Arc::new(move |req| -> HandlerFuture {
            let f = f.clone();
            Box::pin(async move {
                let call = match accept(&req) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                let body: Streaming<Req> = request_parts(&req, &call);
                let deadline = call.deadline;
                let r = with_deadline(deadline, async move {
                    let m = read_single(body).await?;
                    let (req, _) = make_request(call, m);
                    f(req).await
                })
                .await;
                match r {
                    Ok(resp) => stream_response(boxed(resp), deadline),
                    Err(s) => trailers_only(&s),
                }
            })
        });
        grpc_route(self, path, func)
    }

    /// クライアントストリーミング RPC（要求のストリーム → 1 応答）を登録する。
    pub fn grpc_client_streaming<Req, Resp, F, Fut>(self, path: &str, f: F) -> App
    where
        Req: prost::Message + Default + 'static,
        Resp: prost::Message + Send + 'static,
        F: Fn(Request<Streaming<Req>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response<Resp>, Status>> + 'static,
    {
        let f = Arc::new(f);
        let func: HandlerFn = Arc::new(move |req| -> HandlerFuture {
            let f = f.clone();
            Box::pin(async move {
                let call = match accept(&req) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                let body: Streaming<Req> = request_parts(&req, &call);
                let (req, deadline) = make_request(call, body);
                match with_deadline(deadline, f(req)).await {
                    Ok(resp) => {
                        let Response {
                            metadata,
                            message,
                            trailers,
                        } = resp;
                        stream_response(
                            Response {
                                metadata,
                                message: once(message),
                                trailers,
                            },
                            None,
                        )
                    }
                    Err(s) => trailers_only(&s),
                }
            })
        });
        grpc_route(self, path, func)
    }

    /// 双方向ストリーミング RPC（要求のストリーム → 応答のストリーム）を登録する。
    /// HTTP/2 では要求と応答が全二重で流れる（HTTP/3 では要求本文を受け終えてから呼ばれる）。
    pub fn grpc_bidi<Req, Resp, S, F, Fut>(self, path: &str, f: F) -> App
    where
        Req: prost::Message + Default + 'static,
        Resp: prost::Message + 'static,
        S: Stream<Item = Result<Resp, Status>> + Send + 'static,
        F: Fn(Request<Streaming<Req>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response<S>, Status>> + 'static,
    {
        let f = Arc::new(f);
        let func: HandlerFn = Arc::new(move |req| -> HandlerFuture {
            let f = f.clone();
            Box::pin(async move {
                let call = match accept(&req) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                let body: Streaming<Req> = request_parts(&req, &call);
                let (req, deadline) = make_request(call, body);
                match with_deadline(deadline, f(req)).await {
                    Ok(resp) => stream_response(boxed(resp), deadline),
                    Err(s) => trailers_only(&s),
                }
            })
        });
        grpc_route(self, path, func)
    }
}

/// 応答ストリームを `Unpin` にする（利用者のストリームは `Unpin` とは限らない）。
fn boxed<S, T>(resp: Response<S>) -> Response<BoxStream<T>>
where
    S: Stream<Item = Result<T, Status>> + Send + 'static,
{
    Response {
        metadata: resp.metadata,
        message: Box::pin(resp.message),
        trailers: resp.trailers,
    }
}

#[cfg(test)]
mod tests;
