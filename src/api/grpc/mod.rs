//! gRPC サーバ（API ライブラリ、F-197）。ワイヤ処理は veil の `src/grpc/`
//! （5 バイトのフレーミング・`grpc-status`/`grpc-message`・`grpc-timeout`・gzip/deflate）。tokio・tonic に依存しない。
//!
//! メッセージの符号化と復号は [`Codec`] が担い、veil 自身は protobuf の実装に依存しない（F-208）。
//! `.proto` から `veil-build` で生成したコードは prost のメッセージと `ProstCodec` を使う。
//! 別の protobuf 実装・JSON 等は `Codec` を実装すれば使える。[`BytesCodec`] は符号化前のバイト列をそのまま渡す。
//!
//! ```ignore
//! use veil::api::grpc::{Request, Response, Status};
//! App::new()
//!     .grpc_unary("/helloworld.Greeter/SayHello", ProstCodec, |req: Request<HelloRequest>| async move {
//!         Ok(Response::new(HelloReply { message: format!("Hello {}", req.get_ref().name) }))
//!     })
//!     .run()
//! ```
//!
//! 実体は F-188 のハンドラ（`POST /<package>.<Service>/<Method>`、要求本文はストリーミング受信）。
//! 応答はストリーミング応答（DATA の後にトレーラ `grpc-status`）で送る。HTTP/2・HTTP/3 で動き、
//! HTTP/1.1 で来た要求には `grpc-status: 13` を返す。`.proto` からのコード生成は `veil-build` クレート。

pub mod health;
pub(crate) mod wire;

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
use bytes::BufMut;
use futures::{Stream, StreamExt};
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

/// 応答の圧縮方式（`Response::with_compression`）。
pub use crate::grpc::compression::GrpcCompression as Compression;
pub use crate::grpc::status::GrpcStatusCode as Code;
/// codec の実装（生成コードの `ProstCodec` 等）が使うバッファの型。
pub use bytes::{Bytes, BytesMut};

// ===================== Codec =====================

/// メッセージ型 `T` の符号化と復号（F-208）。
///
/// 「メッセージ型に実装する trait」ではなく「codec の型に実装する trait」にしている。生成コードは利用者の
/// クレートにローカルな codec の型（`ProstCodec`）を置き、`impl<T: prost::Message + Default> Codec<T> for ProstCodec`
/// と書ける（orphan rule。外部クレートのメッセージ型 `prost_types::Empty` 等も rpc に使える）。
pub trait Codec<T>: Clone + Send + Sync + 'static {
    /// `out` の末尾にメッセージを書く（veil が先に 5 バイトの見出しの領域を空け、書いた後に長さを入れる）。
    fn encode(&self, message: &T, out: &mut BytesMut) -> Result<(), Status>;
    /// 1 メッセージ分（解凍済み）を復号する。
    fn decode(&self, buf: Bytes) -> Result<T, Status>;
    /// 符号化後の大きさ（分かれば。送信バッファを 1 回で確保するため。0 = 不明）。
    fn encoded_len(&self, _message: &T) -> usize {
        0
    }
}

/// 符号化前のバイト列をそのまま受け渡す codec（生成コード無しで使う・C や Python の橋渡し）。
#[derive(Clone, Copy, Debug, Default)]
pub struct BytesCodec;

impl Codec<Bytes> for BytesCodec {
    fn encode(&self, message: &Bytes, out: &mut BytesMut) -> Result<(), Status> {
        out.extend_from_slice(message);
        Ok(())
    }
    fn decode(&self, buf: Bytes) -> Result<Bytes, Status> {
        Ok(buf)
    }
    fn encoded_len(&self, message: &Bytes) -> usize {
        message.len()
    }
}

/// 受信ストリームが持つ復号関数（ルートの登録時に 1 回作り、呼び出しごとには `Arc` を複製するだけ）。
type DecodeFn<T> = Arc<dyn Fn(Bytes) -> Result<T, Status> + Send + Sync>;

fn decode_fn<T: 'static, C: Codec<T>>(codec: &C) -> DecodeFn<T> {
    let codec = codec.clone();
    Arc::new(move |b| codec.decode(b))
}

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

/// 応答の圧縮（`[api] grpc_response_compression`、0 = しない・1 = gzip・2 = deflate。F-209 #2）。
static RESPONSE_COMPRESSION: AtomicU8 = AtomicU8::new(0);
/// この大きさ未満のメッセージは圧縮しない（`[api] grpc_response_compression_min_size`）。
static RESPONSE_COMPRESSION_MIN: AtomicUsize = AtomicUsize::new(1024);

/// `[api] grpc_response_compression`（`"none"`・`"gzip"`・`"deflate"`）を反映する。知らない値は `none` にして返す。
pub(crate) fn set_response_compression(name: &str, min_size: usize) -> Result<(), String> {
    let (v, r) = match name.to_ascii_lowercase().as_str() {
        "" | "none" | "identity" => (0, Ok(())),
        "gzip" => (1, Ok(())),
        "deflate" => (2, Ok(())),
        other => (
            0,
            Err(format!(
                "[api] grpc_response_compression = {:?} is not one of none, gzip, deflate",
                other
            )),
        ),
    };
    RESPONSE_COMPRESSION.store(v, Ordering::Relaxed);
    RESPONSE_COMPRESSION_MIN.store(min_size, Ordering::Relaxed);
    r
}

fn configured_compression() -> Compression {
    match RESPONSE_COMPRESSION.load(Ordering::Relaxed) {
        1 => Compression::Gzip,
        2 => Compression::Deflate,
        _ => Compression::Identity,
    }
}

/// 要求の `grpc-accept-encoding` が受けると言っている方式（ビット: 1 = gzip・2 = deflate）。
fn accepted_encodings(v: Option<&str>) -> u8 {
    let mut out = 0;
    for e in v.unwrap_or("").split(',') {
        match e.trim() {
            x if x.eq_ignore_ascii_case("gzip") => out |= 1,
            x if x.eq_ignore_ascii_case("deflate") => out |= 2,
            _ => {}
        }
    }
    out
}

/// 応答に使う圧縮方式: 呼び出しの指定 > 設定。クライアントが受けると言っていない・このビルドで使えない方式は使わない。
fn response_encoding(want: Option<Compression>, accepts: u8) -> Compression {
    let want = want.unwrap_or_else(configured_compression);
    let bit = match want {
        Compression::Gzip => 1,
        Compression::Deflate => 2,
        Compression::Identity => return Compression::Identity,
    };
    if accepts & bit != 0 && want.is_supported() {
        want
    } else {
        Compression::Identity
    }
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
    compression: Option<Compression>,
}

impl<T> Response<T> {
    pub fn new(message: T) -> Response<T> {
        Response {
            metadata: Vec::new(),
            message,
            trailers: Vec::new(),
            compression: None,
        }
    }

    /// この呼び出しの応答の圧縮方式（`[api] grpc_response_compression` より優先。`Compression::Identity` で圧縮しない）。
    /// クライアントが `grpc-accept-encoding` で受けると言っていない方式は使わない（非圧縮で送る）。
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = Some(compression);
        self
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
/// 圧縮を戻して codec で復号する。上限超過は `RESOURCE_EXHAUSTED`、壊れたメッセージは `INTERNAL`。
pub struct Streaming<T> {
    body: RequestBodyStream,
    decoder: GrpcFrameDecoder,
    encoding: GrpcCompression,
    decode: DecodeFn<T>,
    body_done: bool,
    finished: bool,
}

impl<T> std::fmt::Debug for Streaming<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Streaming")
            .field("encoding", &self.encoding)
            .finish()
    }
}

impl<T> Streaming<T> {
    fn new(
        body: RequestBodyStream,
        encoding: GrpcCompression,
        decode: DecodeFn<T>,
    ) -> Streaming<T> {
        Streaming {
            body,
            decoder: GrpcFrameDecoder::with_max_size(max_message_size()),
            encoding,
            decode,
            body_done: false,
            finished: false,
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
        (self.decode)(Bytes::from(data))
    }
}

impl<T> Stream for Streaming<T> {
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

/// メッセージを 5 バイトの見出し付きで符号化する（非圧縮）。見出しの領域を空けて codec に書かせ、後で長さを入れる。
pub(crate) fn encode_message<T, C: Codec<T>>(codec: &C, m: &T) -> Result<Bytes, Status> {
    let mut buf = BytesMut::with_capacity(5 + codec.encoded_len(m));
    buf.put_bytes(0, 5);
    codec.encode(m, &mut buf)?;
    let len = buf.len() - 5;
    let len = u32::try_from(len)
        .map_err(|_| Status::resource_exhausted(format!("response message too large ({})", len)))?;
    buf[1..5].copy_from_slice(&len.to_be_bytes());
    Ok(buf.freeze())
}

/// 圧縮する設定なら、最小サイズ以上のメッセージを圧縮した枠に作り直す（圧縮フラグ 1）。
fn compress_frame(frame: Bytes, encoding: Compression) -> Result<Bytes, Status> {
    if encoding == Compression::Identity
        || frame.len() - 5 < RESPONSE_COMPRESSION_MIN.load(Ordering::Relaxed)
    {
        return Ok(frame);
    }
    let c = encoding
        .compress(&frame[5..], 6)
        .map_err(|e| Status::internal(format!("failed to compress message: {}", e)))?;
    let mut buf = BytesMut::with_capacity(5 + c.len());
    buf.put_u8(1);
    buf.put_u32(c.len() as u32);
    buf.extend_from_slice(&c);
    Ok(buf.freeze())
}

/// 応答のメッセージ列を本文の断片とトレーラに変える（最後に `grpc-status`、途中の `Err` はその状態で終える）。
struct GrpcBody<S, C> {
    inner: S,
    codec: C,
    /// 応答の圧縮（F-209 #2）。`Identity` なら圧縮しない
    encoding: Compression,
    trailers: Option<Metadata>,
    deadline: Option<Instant>,
}

impl<S, T, C> Stream for GrpcBody<S, C>
where
    S: Stream<Item = Result<T, Status>> + Unpin,
    C: Codec<T> + Unpin,
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
            Poll::Ready(Some(Ok(m))) => match encode_message(&this.codec, &m)
                .and_then(|b| compress_frame(b, this.encoding))
            {
                Ok(b) => Poll::Ready(Some(BodyChunk::Data(b))),
                Err(s) => {
                    this.trailers = None;
                    Poll::Ready(Some(BodyChunk::Trailers(s.to_trailers())))
                }
            },
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
fn stream_response<S, T, C>(
    resp: Response<S>,
    deadline: Option<Instant>,
    codec: C,
    accepts: u8,
) -> super::response::Response
where
    S: Stream<Item = Result<T, Status>> + Send + Unpin + 'static,
    T: 'static,
    C: Codec<T> + Unpin,
{
    let Response {
        metadata,
        message,
        trailers,
        compression,
    } = resp;
    let encoding = response_encoding(compression, accepts);
    let mut head = grpc_head(200);
    if encoding != Compression::Identity {
        head.headers.push((
            Bytes::from_static(b"grpc-encoding"),
            Bytes::from_static(encoding.as_bytes()),
        ));
    }
    head.headers.extend(metadata);
    let body = GrpcBody {
        inner: message,
        codec,
        encoding,
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
    /// `grpc-accept-encoding`（`accepted_encodings` のビット）
    accepts: u8,
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
        accepts: accepted_encodings(req.header("grpc-accept-encoding")),
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
async fn read_single<T>(mut s: Streaming<T>) -> Result<T, Status> {
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

fn request_parts<T>(
    req: &super::request::Request,
    call: &Call,
    decode: &DecodeFn<T>,
) -> Streaming<T> {
    Streaming::new(req.take_body_stream(), call.encoding, decode.clone())
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
    /// `codec` が要求と応答のメッセージを符号化・復号する（`veil-build` の生成コードは `ProstCodec`）。
    pub fn grpc_unary<C, Req, Resp, F, Fut>(self, path: &str, codec: C, f: F) -> App
    where
        C: Codec<Req> + Codec<Resp> + Unpin,
        Req: 'static,
        Resp: Send + 'static,
        F: Fn(Request<Req>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response<Resp>, Status>> + 'static,
    {
        let f = Arc::new(f);
        let decode = decode_fn::<Req, C>(&codec);
        let func: HandlerFn = Arc::new(move |req| -> HandlerFuture {
            let f = f.clone();
            let codec = codec.clone();
            let decode = decode.clone();
            Box::pin(async move {
                let call = match accept(&req) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                let accepts = call.accepts;
                let body: Streaming<Req> = request_parts(&req, &call, &decode);
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
                            compression,
                        } = resp;
                        stream_response(
                            Response {
                                metadata,
                                message: once(message),
                                trailers,
                                compression,
                            },
                            None,
                            codec,
                            accepts,
                        )
                    }
                    Err(s) => trailers_only(&s),
                }
            })
        });
        grpc_route(self, path, func)
    }

    /// サーバストリーミング RPC（1 要求 → 応答のストリーム）を登録する。
    pub fn grpc_server_streaming<C, Req, Resp, S, F, Fut>(self, path: &str, codec: C, f: F) -> App
    where
        C: Codec<Req> + Codec<Resp> + Unpin,
        Req: 'static,
        Resp: 'static,
        S: Stream<Item = Result<Resp, Status>> + Send + 'static,
        F: Fn(Request<Req>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response<S>, Status>> + 'static,
    {
        let f = Arc::new(f);
        let decode = decode_fn::<Req, C>(&codec);
        let func: HandlerFn = Arc::new(move |req| -> HandlerFuture {
            let f = f.clone();
            let codec = codec.clone();
            let decode = decode.clone();
            Box::pin(async move {
                let call = match accept(&req) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                let accepts = call.accepts;
                let body: Streaming<Req> = request_parts(&req, &call, &decode);
                let deadline = call.deadline;
                let r = with_deadline(deadline, async move {
                    let m = read_single(body).await?;
                    let (req, _) = make_request(call, m);
                    f(req).await
                })
                .await;
                match r {
                    Ok(resp) => stream_response(boxed(resp), deadline, codec, accepts),
                    Err(s) => trailers_only(&s),
                }
            })
        });
        grpc_route(self, path, func)
    }

    /// クライアントストリーミング RPC（要求のストリーム → 1 応答）を登録する。
    pub fn grpc_client_streaming<C, Req, Resp, F, Fut>(self, path: &str, codec: C, f: F) -> App
    where
        C: Codec<Req> + Codec<Resp> + Unpin,
        Req: 'static,
        Resp: Send + 'static,
        F: Fn(Request<Streaming<Req>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response<Resp>, Status>> + 'static,
    {
        let f = Arc::new(f);
        let decode = decode_fn::<Req, C>(&codec);
        let func: HandlerFn = Arc::new(move |req| -> HandlerFuture {
            let f = f.clone();
            let codec = codec.clone();
            let decode = decode.clone();
            Box::pin(async move {
                let call = match accept(&req) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                let accepts = call.accepts;
                let body: Streaming<Req> = request_parts(&req, &call, &decode);
                let (req, deadline) = make_request(call, body);
                match with_deadline(deadline, f(req)).await {
                    Ok(resp) => {
                        let Response {
                            metadata,
                            message,
                            trailers,
                            compression,
                        } = resp;
                        stream_response(
                            Response {
                                metadata,
                                message: once(message),
                                trailers,
                                compression,
                            },
                            None,
                            codec,
                            accepts,
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
    pub fn grpc_bidi<C, Req, Resp, S, F, Fut>(self, path: &str, codec: C, f: F) -> App
    where
        C: Codec<Req> + Codec<Resp> + Unpin,
        Req: 'static,
        Resp: 'static,
        S: Stream<Item = Result<Resp, Status>> + Send + 'static,
        F: Fn(Request<Streaming<Req>>) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<Response<S>, Status>> + 'static,
    {
        let f = Arc::new(f);
        let decode = decode_fn::<Req, C>(&codec);
        let func: HandlerFn = Arc::new(move |req| -> HandlerFuture {
            let f = f.clone();
            let codec = codec.clone();
            let decode = decode.clone();
            Box::pin(async move {
                let call = match accept(&req) {
                    Ok(c) => c,
                    Err(r) => return r,
                };
                let accepts = call.accepts;
                let body: Streaming<Req> = request_parts(&req, &call, &decode);
                let (req, deadline) = make_request(call, body);
                match with_deadline(deadline, f(req)).await {
                    Ok(resp) => stream_response(boxed(resp), deadline, codec, accepts),
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
        compression: resp.compression,
    }
}

#[cfg(test)]
mod tests;
