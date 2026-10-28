// F-197, F-208: メッセージの符号化は `Codec` が担い、veil は protobuf の実装に依存しない。実体は F-188 のハンドラ。
//! gRPC server for the API library. Wire handling uses veil's `src/grpc/` (5-byte framing,
//! `grpc-status`/`grpc-message`, `grpc-timeout`, gzip/deflate). It depends on neither tokio nor tonic.
//!
//! Encoding and decoding of messages is done by a [`Codec`]; veil itself does not depend on a protobuf implementation.
//! Code generated from a `.proto` file by `veil-build` uses prost messages and `ProstCodec`.
//! Another protobuf implementation, JSON and so on can be used by implementing `Codec`. [`BytesCodec`] passes the
//! unencoded bytes through as they are.
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
//! Under the hood this is an ordinary handler (`POST /<package>.<Service>/<Method>`, with the request body
//! received as a stream). The response is sent as a streaming response (DATA followed by the `grpc-status`
//! trailer). It works over HTTP/2 and HTTP/3; a request arriving over HTTP/1.1 gets `grpc-status: 13`. Code
//! generation from `.proto` files is provided by the `veil-build` crate.

// F-210: C・Python 用。
/// Blocking gRPC handlers (for C and Python).
pub mod blocking;
// F-267
mod channel;
// F-265
mod describe;
pub mod health;
pub mod reflection;
mod web;
pub(crate) mod wire;

/// Includes the `<package>.rs` that `veil-build` wrote to `OUT_DIR` (`veil::include_proto!("helloworld");`).
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
pub use futures::Stream;
use futures::StreamExt;
use std::future::Future;
use std::net::IpAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

// F-265
#[cfg(feature = "openapi")]
pub(crate) use describe::add_to_openapi;
pub use describe::MethodKind;

/// The compression method of a response (`Response::with_compression`).
pub use crate::grpc::compression::GrpcCompression as Compression;
pub use crate::grpc::status::GrpcStatusCode as Code;
/// The buffer types used by codec implementations (such as the generated `ProstCodec`).
pub use bytes::{Bytes, BytesMut};
pub use channel::{Channel, IntoRequest, IntoStreamingRequest, Streaming as ClientStreaming};

// ===================== Codec =====================

// F-208
/// Encoding and decoding of the message type `T`.
///
/// This is a trait implemented on a codec type, not on the message type. Generated code places a codec type
/// (`ProstCodec`) local to the user's crate and writes `impl<T: prost::Message + Default> Codec<T> for ProstCodec`
/// (orphan rule; message types from external crates such as `prost_types::Empty` can also be used in an RPC).
pub trait Codec<T>: Clone + Send + Sync + 'static {
    /// Writes the message at the end of `out` (veil first reserves room for the 5-byte header and fills in the length afterwards).
    fn encode(&self, message: &T, out: &mut BytesMut) -> Result<(), Status>;
    /// Decodes one message (already decompressed).
    fn decode(&self, buf: Bytes) -> Result<T, Status>;
    /// The encoded size, if known (so the send buffer can be allocated at once; 0 means unknown).
    fn encoded_len(&self, _message: &T) -> usize {
        0
    }
}

/// A codec that passes unencoded bytes through as they are (usable without generated code, and as a bridge for C and Python).
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

/// The decode function held by a receive stream (created once when the route is registered; each call only clones the `Arc`).
type DecodeFn<T> = Arc<dyn Fn(Bytes) -> Result<T, Status> + Send + Sync>;

fn decode_fn<T: 'static, C: Codec<T>>(codec: &C) -> DecodeFn<T> {
    let codec = codec.clone();
    Arc::new(move |b| codec.decode(b))
}

/// Metadata (request headers, response headers and trailers). Names are lower case.
pub type Metadata = Vec<(Bytes, Bytes)>;

/// The type of a response stream (the return value of server and bidirectional streaming in generated code).
pub type BoxStream<T> = Pin<Box<dyn Stream<Item = Result<T, Status>> + Send + 'static>>;

/// Limit on received messages (`[api] grpc_max_message_size`, default 4 MiB).
static MAX_MESSAGE_SIZE: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_MESSAGE_SIZE);

/// The conventional default limit of gRPC.
pub const DEFAULT_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

pub(crate) fn set_max_message_size(n: usize) {
    MAX_MESSAGE_SIZE.store(n.max(1), Ordering::Relaxed);
}

/// The current limit on received messages.
pub fn max_message_size() -> usize {
    MAX_MESSAGE_SIZE.load(Ordering::Relaxed)
}

// F-209 #8
/// Limit on sent messages (`[api] grpc_max_send_message_size`, 0 = unlimited).
static MAX_SEND_MESSAGE_SIZE: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn set_max_send_message_size(n: usize) {
    MAX_SEND_MESSAGE_SIZE.store(n, Ordering::Relaxed);
}

// F-209 #2
/// Compression of responses (`[api] grpc_response_compression`; 0 = none, 1 = gzip, 2 = deflate).
static RESPONSE_COMPRESSION: AtomicU8 = AtomicU8::new(0);
/// Messages smaller than this are not compressed (`[api] grpc_response_compression_min_size`).
static RESPONSE_COMPRESSION_MIN: AtomicUsize = AtomicUsize::new(1024);

/// Applies `[api] grpc_response_compression` (`"none"`, `"gzip"` or `"deflate"`). An unknown value is reported as an error and treated as `none`.
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

// F-209 #4
/// Applies `[api] grpc_web_cors_origins`.
pub(crate) fn set_grpc_web_cors_origins(origins: Vec<String>) {
    web::set_cors_origins(origins);
}

fn configured_compression() -> Compression {
    match RESPONSE_COMPRESSION.load(Ordering::Relaxed) {
        1 => Compression::Gzip,
        2 => Compression::Deflate,
        _ => Compression::Identity,
    }
}

/// The methods the request's `grpc-accept-encoding` says it accepts (bits: 1 = gzip, 2 = deflate).
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

/// The compression method to use for the response: the call's choice takes precedence over the setting. A method the client did not say it accepts, or one unavailable in this build, is not used.
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

/// A gRPC status (code, message and extra trailers).
#[derive(Debug, Clone)]
pub struct Status {
    code: Code,
    message: String,
    metadata: Metadata,
    // F-209 #6
    /// `grpc-status-details-bin` (an encoded `google.rpc.Status`).
    details: Option<Bytes>,
}

macro_rules! status_ctor {
    ($($name:ident => $code:ident),* $(,)?) => {
        $(
            #[doc = concat!("A `", stringify!($code), "` status.")]
            pub fn $name(message: impl Into<String>) -> Status {
                Status::new(Code::$code, message)
            }
        )*
    };
}

impl Status {
    /// Creates a `Status` with the given code and message (metadata and details are empty).
    pub fn new(code: Code, message: impl Into<String>) -> Status {
        Status {
            code,
            message: message.into(),
            metadata: Vec::new(),
            details: None,
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

    /// The code carried in `grpc-status`.
    pub fn code(&self) -> Code {
        self.code
    }

    /// The message carried in `grpc-message`.
    pub fn message(&self) -> &str {
        &self.message
    }

    // F-268: クライアント（veil::api::grpc::Channel）が受けた状態の trailers を読む
    /// The extra metadata (trailers other than `grpc-status` / `grpc-message`; on a client, those of the response).
    pub fn metadata(&self) -> &[(Bytes, Bytes)] {
        &self.metadata
    }

    /// Metadata to add to the trailers (names are lowercased).
    pub fn with_metadata(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Status {
        self.metadata.push((lower(name.into()), value.into()));
        self
    }

    // F-209
    /// Error details, sent in `grpc-status-details-bin` as an encoded `google.rpc.Status`.
    /// Encode a `google.rpc.Status` containing `google.rpc.BadRequest` and the like with your own protobuf library and
    /// pass it in (veil does not interpret the contents; by the gRPC convention `code` and `message` should match this `Status`).
    pub fn with_details(mut self, details: impl Into<Bytes>) -> Status {
        self.details = Some(details.into());
        self
    }

    /// The details set with `with_details`.
    pub fn details(&self) -> Option<&Bytes> {
        self.details.as_ref()
    }

    /// `grpc-status`, `grpc-message` (percent-encoded) and the extra metadata.
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
        if let Some(d) = &self.details {
            // バイナリのメタデータ（`-bin`）は base64。gRPC の実装は `=` の無い形を送る
            let mut enc = web::Base64Encoder::default();
            let mut v = BytesMut::from(&enc.encode(d)[..]);
            v.extend_from_slice(&enc.finish());
            while v.last() == Some(&b'=') {
                v.truncate(v.len() - 1);
            }
            out.push((Bytes::from_static(b"grpc-status-details-bin"), v.freeze()));
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

/// Percent-encoding of the gRPC `grpc-message` (per UTF-8 byte; bytes in 0x20 to 0x7E other than `%` are kept as they are).
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

/// The inverse of `percent_encode` (for clients and tests).
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

/// A gRPC request (metadata plus a message, or a stream of messages).
#[derive(Debug)]
pub struct Request<T> {
    metadata: Metadata,
    message: T,
    deadline: Option<Instant>,
    client_ip: IpAddr,
    /// `/<package>.<Service>/<Method>`
    path: Bytes,
}

impl<T> Request<T> {
    /// For tests and direct calls.
    pub fn new(message: T) -> Request<T> {
        Request {
            metadata: Vec::new(),
            message,
            deadline: None,
            client_ip: IpAddr::from([127, 0, 0, 1]),
            path: Bytes::new(),
        }
    }

    /// A reference to the decoded request message.
    pub fn get_ref(&self) -> &T {
        &self.message
    }

    /// A mutable reference to the decoded request message.
    pub fn get_mut(&mut self) -> &mut T {
        &mut self.message
    }

    /// Takes out the request message (the metadata is discarded).
    pub fn into_inner(self) -> T {
        self.message
    }

    /// The request metadata (request headers other than reserved ones such as `content-type`, `te`, `grpc-timeout` and `grpc-encoding`).
    pub fn metadata(&self) -> &[(Bytes, Bytes)] {
        &self.metadata
    }

    /// A metadata value (the name is case-insensitive).
    pub fn metadata_value(&self, name: &str) -> Option<&[u8]> {
        self.metadata
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| v.as_ref())
    }

    /// The deadline derived from `grpc-timeout`.
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// The client's IP address (`127.0.0.1` for a request built with `Request::new`).
    pub fn client_ip(&self) -> IpAddr {
        self.client_ip
    }

    /// The method called (`/<package>.<Service>/<Method>`; empty for a request built with `Request::new`).
    pub fn method(&self) -> &str {
        std::str::from_utf8(&self.path).unwrap_or("")
    }

    /// Replaces only the message (metadata and deadline are kept).
    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> Request<U> {
        Request {
            metadata: self.metadata,
            message: f(self.message),
            deadline: self.deadline,
            client_ip: self.client_ip,
            path: self.path,
        }
    }
}

/// A gRPC response (metadata = response headers, a message or a stream, and trailers).
#[derive(Debug)]
pub struct Response<T> {
    metadata: Metadata,
    message: T,
    trailers: Metadata,
    compression: Option<Compression>,
}

impl<T> Response<T> {
    /// Creates a `Response` from a response message (metadata and trailers are empty; compression follows the settings).
    pub fn new(message: T) -> Response<T> {
        Response {
            metadata: Vec::new(),
            message,
            trailers: Vec::new(),
            compression: None,
        }
    }

    /// The compression method for this call's response; it takes precedence over `[api] grpc_response_compression`
    /// (`Compression::Identity` disables compression). A method the client did not say it accepts in `grpc-accept-encoding` is not used (the response is sent uncompressed).
    pub fn with_compression(mut self, compression: Compression) -> Self {
        self.compression = Some(compression);
        self
    }

    /// A reference to the response message.
    pub fn get_ref(&self) -> &T {
        &self.message
    }

    /// Takes out the response message (metadata and trailers are discarded).
    pub fn into_inner(self) -> T {
        self.message
    }

    /// Adds a response header (the name is lowercased).
    pub fn with_metadata(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        self.metadata.push((lower(name.into()), value.into()));
        self
    }

    /// Adds a trailer for a normal completion (after `grpc-status: 0`).
    pub fn with_trailer(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        self.trailers.push((lower(name.into()), value.into()));
        self
    }

    /// The metadata added to the response headers.
    pub fn metadata(&self) -> &[(Bytes, Bytes)] {
        &self.metadata
    }
}

// ===================== Streaming（受信） =====================

/// A stream of received messages (the request of client streaming and bidi). It cuts the received body into gRPC
/// frames, undoes compression and decodes with the codec. Exceeding the limit gives `RESOURCE_EXHAUSTED`; a malformed message gives `INTERNAL`.
pub struct Streaming<T> {
    body: RequestBodyStream,
    decoder: GrpcFrameDecoder,
    encoding: GrpcCompression,
    decode: DecodeFn<T>,
    // F-209 #4
    /// For gRPC-Web `-text` (base64 body), decodes while reading.
    text: Option<web::Base64Decoder>,
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
        text: bool,
    ) -> Streaming<T> {
        Streaming {
            body,
            decoder: GrpcFrameDecoder::with_max_size(max_message_size()),
            encoding,
            decode,
            text: text.then(web::Base64Decoder::default),
            body_done: false,
            finished: false,
        }
    }

    /// The next message (`Ok(None)` at the end).
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
                // B-187: クライアントのリセット・切断で切れた要求のストリームを、正常な終わりに見せない
                if this.body.is_aborted() {
                    return Poll::Ready(Some(Err(Status::cancelled(
                        "request stream ended before the client finished it",
                    ))));
                }
                if this.text.as_ref().is_some_and(|d| d.finish().is_err()) {
                    return Poll::Ready(Some(Err(Status::internal(
                        "grpc-web-text request body is not valid base64",
                    ))));
                }
                if this.decoder.has_pending() {
                    return Poll::Ready(Some(Err(Status::internal(
                        "request body ended in the middle of a message",
                    ))));
                }
                return Poll::Ready(None);
            }
            match Pin::new(&mut this.body).poll_next(cx) {
                Poll::Ready(Some(chunk)) => match this.text.as_mut() {
                    None => this.decoder.push(&chunk),
                    Some(dec) => {
                        let mut raw = BytesMut::with_capacity(chunk.len() / 4 * 3 + 3);
                        if dec.push(&chunk, &mut raw).is_err() {
                            this.finished = true;
                            return Poll::Ready(Some(Err(Status::internal(
                                "grpc-web-text request body is not valid base64",
                            ))));
                        }
                        this.decoder.push(&raw);
                    }
                },
                Poll::Ready(None) => this.body_done = true,
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

// ===================== 送信 =====================

/// Encodes a message with the 5-byte header (uncompressed). Room for the header is reserved, the codec writes the message, and the length is filled in afterwards.
pub(crate) fn encode_message<T, C: Codec<T>>(codec: &C, m: &T) -> Result<Bytes, Status> {
    let mut buf = BytesMut::with_capacity(5 + codec.encoded_len(m));
    buf.put_bytes(0, 5);
    codec.encode(m, &mut buf)?;
    let len = buf.len() - 5;
    if let Some(s) = send_limit_error(len, MAX_SEND_MESSAGE_SIZE.load(Ordering::Relaxed)) {
        return Err(s);
    }
    let len = u32::try_from(len)
        .map_err(|_| Status::resource_exhausted(format!("response message too large ({})", len)))?;
    buf[1..5].copy_from_slice(&len.to_be_bytes());
    Ok(buf.freeze())
}

/// When compression is configured, rebuilds a message of at least the minimum size as a compressed frame (compression flag 1).
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

/// `RESOURCE_EXHAUSTED` if the sent message exceeds the limit (0 = unlimited).
fn send_limit_error(len: usize, max: usize) -> Option<Status> {
    (max > 0 && len > max).then(|| {
        Status::resource_exhausted(format!(
            "response message larger than max ({} vs. {})",
            len, max
        ))
    })
}

/// Turns a sequence of response messages into body chunks and trailers (`grpc-status` at the end; an `Err` midway ends the stream with that status).
struct GrpcBody<S, C> {
    inner: S,
    codec: C,
    // F-209 #2
    /// Response compression (`Identity` means none).
    encoding: Compression,
    // F-209 #4
    /// gRPC-Web: sends the trailers as the last frame of the body. `Some(Some(_))` is `-text` (base64).
    web: Option<Option<web::Base64Encoder>>,
    trailers: Option<Metadata>,
    deadline: Option<Instant>,
}

impl<S, C> GrpcBody<S, C> {
    /// A chunk of the body (base64-encoded for gRPC-Web `-text`).
    fn data(&mut self, b: Bytes) -> BodyChunk {
        match &mut self.web {
            Some(Some(enc)) => BodyChunk::Data(enc.encode(&b)),
            _ => BodyChunk::Data(b),
        }
    }

    /// The final trailers (for gRPC-Web, the last frame of the body). Returns `None` afterwards.
    fn end(&mut self, trailers: Metadata) -> BodyChunk {
        self.trailers = None;
        match &mut self.web {
            None => BodyChunk::Trailers(trailers),
            Some(None) => BodyChunk::Data(web::trailer_frame(&trailers)),
            Some(Some(enc)) => {
                let mut out = BytesMut::from(&enc.encode(&web::trailer_frame(&trailers))[..]);
                out.extend_from_slice(&enc.finish());
                BodyChunk::Data(out.freeze())
            }
        }
    }
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
            let t = Status::deadline_exceeded("deadline exceeded").to_trailers();
            return Poll::Ready(Some(this.end(t)));
        }
        match Pin::new(&mut this.inner).poll_next(cx) {
            Poll::Ready(Some(Ok(m))) => match encode_message(&this.codec, &m)
                .and_then(|b| compress_frame(b, this.encoding))
            {
                Ok(b) => Poll::Ready(Some(this.data(b))),
                Err(s) => Poll::Ready(Some(this.end(s.to_trailers()))),
            },
            Poll::Ready(Some(Err(s))) => Poll::Ready(Some(this.end(s.to_trailers()))),
            Poll::Ready(None) => {
                let mut t = Status::new(Code::Ok, "").to_trailers();
                t.extend(ok_trailers.iter().cloned());
                Poll::Ready(Some(this.end(t)))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

// B-127
/// The accepted `grpc-encoding` (identity only in a build without the `compression` feature).
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

/// Response head for gRPC-Web (the same `content-type` as the request, and CORS).
fn web_head(w: &web::WebMode) -> super::response::Response {
    let mut r = super::response::Response::new(200)
        .with_header("content-type", w.content_type.clone())
        .with_header("grpc-accept-encoding", GRPC_ACCEPT_ENCODING);
    r.headers.extend(w.cors.iter().cloned());
    r
}

/// A response carrying only an error. For gRPC-Web the status goes into both the headers and the body's trailer frame
/// (browsers may be unable to read headers that CORS does not expose).
fn error_response(web: &Option<web::WebMode>, status: &Status) -> super::response::Response {
    let Some(w) = web else {
        return trailers_only(status);
    };
    let mut r = web_head(w);
    let t = status.to_trailers();
    let frame = web::trailer_frame(&t);
    r.headers.extend(t);
    if w.text {
        let mut enc = web::Base64Encoder::default();
        let mut b = BytesMut::from(&enc.encode(&frame)[..]);
        b.extend_from_slice(&enc.finish());
        r.with_body(b.freeze())
    } else {
        r.with_body(frame)
    }
}

/// A response carrying only an error (Trailers-Only: `grpc-status` goes into the response headers and the body is empty).
fn trailers_only(status: &Status) -> super::response::Response {
    let mut r = grpc_head(200);
    r.headers.extend(status.to_trailers());
    r
}

/// A response made of a sequence of messages.
fn stream_response<S, T, C>(
    resp: Response<S>,
    deadline: Option<Instant>,
    codec: C,
    accepts: u8,
    web: Option<web::WebMode>,
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
    let mut head = match &web {
        None => grpc_head(200),
        Some(w) => web_head(w),
    };
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
        web: web.map(|w| w.text.then(web::Base64Encoder::default)),
        trailers: Some(trailers),
        deadline,
    };
    let mut r = super::response::Response::stream(200, body);
    r.headers = head.headers;
    r
}

// ===================== 受け付け =====================

/// Call information read from the request headers.
struct Call {
    metadata: Metadata,
    deadline: Option<Instant>,
    encoding: GrpcCompression,
    /// `grpc-accept-encoding` (the bits of `accepted_encodings`).
    accepts: u8,
    // F-209 #4
    /// `Some` for a gRPC-Web request.
    web: Option<web::WebMode>,
    client_ip: IpAddr,
    path: Bytes,
}

/// Checks whether the request can be accepted as a gRPC request (the response to return if not).
fn accept(req: &super::request::Request) -> Result<Call, super::response::Response> {
    let ct = req.header("content-type").unwrap_or_default();
    // gRPC-Web（`application/grpc-web*`）は本文の最後の枠でトレーラを返す（F-209 #4）。HTTP/1.1 でも受ける
    let web = web::web_mode(ct.as_bytes()).map(|(text, content_type)| web::WebMode {
        text,
        content_type,
        cors: web::cors_response_headers(req.header("origin")),
    });
    if web.is_none() {
        if !crate::grpc::headers::is_grpc_content_type(ct.as_bytes()) {
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
    }
    let encoding = match req.header("grpc-encoding") {
        None => GrpcCompression::Identity,
        Some(v) => match GrpcCompression::from_bytes(v.as_bytes()) {
            Some(c) if c.is_supported() => c,
            _ => {
                return Err(error_response(
                    &web,
                    &Status::unimplemented(format!("grpc-encoding '{}' is not supported", v)),
                ))
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
        web,
        client_ip: req.client_ip(),
        path: req.path.clone(),
    })
}

// F-209 #9: gRPC の仕様では HTTP の 404 は `UNIMPLEMENTED` と読み替えられるが、ログ・メトリクスに 404 が出てクライアントのメッセージも曖昧になる。
/// If a request that matched no route is an ordinary gRPC request (not gRPC-Web), the response head of a
/// Trailers-Only `UNIMPLEMENTED` (sent with `:status` 200). Used in place of the protocol layer's 404.
pub(crate) fn unknown_route_response<'a>(
    mut headers: impl Iterator<Item = (&'a [u8], &'a [u8])>,
    path: &[u8],
) -> Option<Metadata> {
    let ct = headers
        .find(|(n, _)| n.eq_ignore_ascii_case(b"content-type"))?
        .1;
    if !crate::grpc::headers::is_grpc_content_type(ct) || web::web_mode(ct).is_some() {
        return None;
    }
    let service = path
        .strip_prefix(b"/")
        .and_then(|p| p.split(|&b| b == b'/').next())
        .unwrap_or_default();
    let mut out = vec![(
        Bytes::from_static(b"content-type"),
        Bytes::from_static(b"application/grpc"),
    )];
    out.extend(
        Status::unimplemented(format!(
            "service {} is not implemented",
            String::from_utf8_lossy(service)
        ))
        .to_trailers(),
    );
    Some(out)
}

/// Awaits a future with a deadline (expiry gives `DEADLINE_EXCEEDED`).
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

/// Reads the single request message of a unary call (zero or two or more messages is an error).
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
    Streaming::new(
        req.take_body_stream(),
        call.encoding,
        decode.clone(),
        call.web.as_ref().is_some_and(|w| w.text),
    )
}

fn make_request<T>(call: Call, message: T) -> (Request<T>, Option<Instant>) {
    let deadline = call.deadline;
    (
        Request {
            metadata: call.metadata,
            message,
            deadline,
            client_ip: call.client_ip,
            path: call.path,
        },
        deadline,
    )
}

/// Turns a single message into a one-element stream.
fn once<T: Send + 'static>(
    m: T,
) -> futures::stream::Once<futures::future::Ready<Result<T, Status>>> {
    futures::stream::once(futures::future::ready(Ok(m)))
}

impl App {
    // F-209 #7
    /// Calls `f` before a gRPC method is called (for authentication, metadata checks and so on). A `prefix` of `"/"`
    /// covers everything and `"/pkg.Svc/"` only that service. Returning `Err` ends the call with that status without
    /// calling the method (Trailers-Only). Requests that are neither gRPC nor gRPC-Web (HTTP routes under the same prefix) are left alone.
    pub fn grpc_interceptor<F>(self, prefix: &str, f: F) -> App
    where
        F: Fn(&Request<()>) -> Result<(), Status> + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        self.wrap_on(prefix, move |inner: HandlerFn| -> HandlerFn {
            let f = f.clone();
            Arc::new(move |req: super::request::Request| -> HandlerFuture {
                let ct = req.header("content-type").unwrap_or_default();
                if req.method() == "OPTIONS"
                    || !crate::grpc::headers::is_grpc_content_type(ct.as_bytes())
                {
                    return inner(req);
                }
                match accept(&req) {
                    Ok(call) => {
                        let web = call.web.clone();
                        let (r, _) = make_request(call, ());
                        match f(&r) {
                            Ok(()) => inner(req),
                            Err(s) => Box::pin(futures::future::ready(error_response(&web, &s))),
                        }
                    }
                    Err(resp) => Box::pin(futures::future::ready(resp)),
                }
            })
        })
    }
}

// F-209 #4
/// Answers a gRPC-Web preflight (`OPTIONS`) before the handler.
fn with_preflight(func: HandlerFn) -> HandlerFn {
    Arc::new(move |req: super::request::Request| -> HandlerFuture {
        if req.method() == "OPTIONS" {
            return Box::pin(futures::future::ready(web::preflight(&req)));
        }
        func(req)
    })
}

fn grpc_route(mut app: App, path: &str, kind: MethodKind, func: HandlerFn) -> App {
    // F-265: OpenAPI は gRPC の記述子と種類から別に作る（ここでは一般の操作として載せない）
    describe::record_kind(path, kind);
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
        reflection::record_service(service);
        let fallback = format!("/{}/{{*method}}", service);
        if !app.routes.iter().any(|r| r.path == fallback) {
            let func: HandlerFn = Arc::new(|req: super::request::Request| -> HandlerFuture {
                Box::pin(async move {
                    match accept(&req) {
                        Ok(call) => error_response(
                            &call.web,
                            &Status::unimplemented(format!(
                                "method {} is not implemented",
                                req.path()
                            )),
                        ),
                        Err(r) => r,
                    }
                })
            });
            let meta = OperationMeta {
                operation_id: format!("{}_unimplemented", service.replace('.', "_")),
                hidden: true,
                ..Default::default()
            };
            app = app.route_fn(
                &meta.operation_id.clone(),
                &["POST", "OPTIONS"],
                &fallback,
                with_preflight(func),
                meta,
            );
        }
    }
    app.route_fn_streaming(path, &["POST", "OPTIONS"], path, with_preflight(func), meta)
}

impl App {
    /// Registers a unary RPC (one request, one response). `path` is `/<package>.<Service>/<Method>`.
    /// `codec` encodes and decodes the request and response messages (`ProstCodec` in code generated by `veil-build`).
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
                let web = call.web.clone();
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
                            web,
                        )
                    }
                    Err(s) => error_response(&web, &s),
                }
            })
        });
        grpc_route(self, path, MethodKind::Unary, func)
    }

    /// Registers a server-streaming RPC (one request, a stream of responses).
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
                let web = call.web.clone();
                let body: Streaming<Req> = request_parts(&req, &call, &decode);
                let deadline = call.deadline;
                let r = with_deadline(deadline, async move {
                    let m = read_single(body).await?;
                    let (req, _) = make_request(call, m);
                    f(req).await
                })
                .await;
                match r {
                    Ok(resp) => stream_response(boxed(resp), deadline, codec, accepts, web),
                    Err(s) => error_response(&web, &s),
                }
            })
        });
        grpc_route(self, path, MethodKind::ServerStreaming, func)
    }

    /// Registers a client-streaming RPC (a stream of requests, one response).
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
                let web = call.web.clone();
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
                            web,
                        )
                    }
                    Err(s) => error_response(&web, &s),
                }
            })
        });
        grpc_route(self, path, MethodKind::ClientStreaming, func)
    }

    // F-209: HTTP/2・HTTP/3 とも全二重。
    /// Registers a bidirectional-streaming RPC (a stream of requests, a stream of responses).
    /// Requests and responses flow full duplex over both HTTP/2 and HTTP/3.
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
                let web = call.web.clone();
                let body: Streaming<Req> = request_parts(&req, &call, &decode);
                let (req, deadline) = make_request(call, body);
                match with_deadline(deadline, f(req)).await {
                    Ok(resp) => stream_response(boxed(resp), deadline, codec, accepts, web),
                    Err(s) => error_response(&web, &s),
                }
            })
        });
        grpc_route(self, path, MethodKind::BidiStreaming, func)
    }
}

/// Makes a response stream `Unpin` (a user's stream is not necessarily `Unpin`).
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
