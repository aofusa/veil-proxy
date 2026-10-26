// F-267: gRPC のクライアント。HTTP/2 の多重化接続（`http2::upstream_mux`）の上で unary と 3 種のストリーミングを呼ぶ。
//! A gRPC client channel on veil's runtime (no tokio or tonic).
//!
//! ```no_run
//! # async fn f() -> Result<(), veil::api::grpc::Status> {
//! use veil::api::grpc::{BytesCodec, Channel, Request};
//! let ch = Channel::connect("http://127.0.0.1:50051")?;
//! let res = ch
//!     .unary("/echo.Echo/Say", BytesCodec, Request::new(veil::api::grpc::Bytes::from_static(b"hi")))
//!     .await?;
//! # let _ = res; Ok(()) }
//! ```
//!
//! `veil-build` generates typed clients (`<service>_client::<Service>Client`) on top of this type with
//! `Builder::build_client(true)`. `http://` uses HTTP/2 with prior knowledge (h2c); `https://` uses ALPN `h2`
//! with the client's trust anchors.

use super::{
    encode_message, lower, percent_decode, Code, Codec, Compression, Metadata, Request, Response,
    Status,
};
use crate::api::client::{Client, Http2, Origin};
use crate::grpc::framing::GrpcFrameDecoder;
use crate::http2::upstream_mux::{UpResp, UpStream};
use bytes::{BufMut, Bytes, BytesMut};
use futures::{Stream, StreamExt};
use std::sync::Arc;
use std::time::Duration;

/// A connection target for gRPC calls (cheap to clone; streams to the same server share one HTTP/2 connection
/// per worker thread).
#[derive(Clone)]
pub struct Channel {
    client: Client,
    origin: Arc<Origin>,
    /// Path prefix in front of `/<package>.<Service>/<Method>` (from the URL; usually empty).
    prefix: String,
    timeout: Option<Duration>,
    compression: Compression,
    metadata: Metadata,
    max_message_size: usize,
}

impl std::fmt::Debug for Channel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Channel")
            .field("authority", &self.origin.authority)
            .finish()
    }
}

impl Channel {
    /// A channel with a client of the default settings (HTTP/2 only). `http://unix:/path.sock:` URLs are accepted.
    pub fn connect(url: &str) -> Result<Channel, Status> {
        // B-198: 呼び出し側が書いた URL なので Unix ソケットも受ける（従来どおり）
        let client = Client::builder()
            .http2(Http2::On)
            .unix_sockets(true)
            .build()
            .map_err(|e| Status::unavailable(e.to_string()))?;
        Channel::with_client(&client, url)
    }

    /// A channel that uses `client`'s TLS settings and timeouts (its HTTP/2 mode does not matter: gRPC always uses HTTP/2).
    pub fn with_client(client: &Client, url: &str) -> Result<Channel, Status> {
        let (origin, target) = client
            .resolve(url)
            .map_err(|e| Status::invalid_argument(e.to_string()))?;
        let prefix = target.split('?').next().unwrap_or("").trim_end_matches('/');
        Ok(Channel {
            client: client.clone(),
            origin,
            prefix: prefix.to_string(),
            timeout: None,
            compression: Compression::Identity,
            metadata: Vec::new(),
            max_message_size: super::DEFAULT_MAX_MESSAGE_SIZE,
        })
    }

    /// The deadline of every call (sent as `grpc-timeout`; a call's own [`Request::with_timeout`] takes precedence).
    pub fn timeout(mut self, d: Duration) -> Self {
        self.timeout = Some(d);
        self
    }

    /// Compresses request messages (`grpc-encoding`) and accepts compressed responses.
    pub fn compression(mut self, c: Compression) -> Self {
        self.compression = c;
        self
    }

    /// Metadata sent with every call (for example `authorization`).
    pub fn metadata(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        self.metadata.push((lower(name.into()), value.into()));
        self
    }

    /// Largest response message accepted (default 4 MiB).
    pub fn max_message_size(mut self, n: usize) -> Self {
        self.max_message_size = n;
        self
    }

    fn headers<T>(&self, req: &Request<T>) -> (Vec<(Bytes, Bytes)>, Duration) {
        let mut h: Vec<(Bytes, Bytes)> =
            Vec::with_capacity(5 + self.metadata.len() + req.metadata.len());
        h.push((
            Bytes::from_static(b"content-type"),
            Bytes::from_static(b"application/grpc"),
        ));
        h.push((Bytes::from_static(b"te"), Bytes::from_static(b"trailers")));
        if self.compression != Compression::Identity {
            h.push((
                Bytes::from_static(b"grpc-encoding"),
                Bytes::from_static(self.compression.as_bytes()),
            ));
        }
        h.push((
            Bytes::from_static(b"grpc-accept-encoding"),
            Bytes::from_static(b"gzip, deflate, identity"),
        ));
        let limit = req
            .deadline
            .map(|d| d.saturating_duration_since(std::time::Instant::now()))
            .or(self.timeout)
            .unwrap_or_else(|| self.client.request_timeout());
        if req.deadline.is_some() || self.timeout.is_some() {
            h.push((
                Bytes::from_static(b"grpc-timeout"),
                Bytes::from(crate::grpc::headers::format_grpc_timeout(limit)),
            ));
        }
        h.extend(self.metadata.iter().cloned());
        h.extend(req.metadata.iter().cloned());
        (h, limit)
    }

    fn frame<T, C: Codec<T>>(&self, codec: &C, m: &T) -> Result<Bytes, Status> {
        let f = encode_message(codec, m)?;
        if self.compression == Compression::Identity {
            return Ok(f);
        }
        let c = self
            .compression
            .compress(&f[5..], 6)
            .map_err(|e| Status::internal(format!("failed to compress message: {}", e)))?;
        let mut buf = BytesMut::with_capacity(5 + c.len());
        buf.put_u8(1);
        buf.put_u32(c.len() as u32);
        buf.extend_from_slice(&c);
        Ok(buf.freeze())
    }

    async fn open(
        &self,
        path: &str,
        headers: &[(Bytes, Bytes)],
        end_stream: bool,
    ) -> Result<UpStream, Status> {
        let full = format!("{}{}", self.prefix, path);
        self.client
            .open_h2_stream(&self.origin, b"POST", full.as_bytes(), headers, end_stream)
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    Status::deadline_exceeded(e.to_string())
                } else {
                    Status::unavailable(e.to_string())
                }
            })
    }

    /// Starts the response side: waits for the head and returns the message stream.
    async fn response<T: 'static, C: Codec<T>>(
        &self,
        stream: UpStream,
        codec: C,
        deadline: timing::Deadline,
    ) -> Result<Response<Streaming<T>>, Status> {
        let mut s = Streaming {
            stream: Some(stream),
            decoder: GrpcFrameDecoder::with_max_size(self.max_message_size),
            encoding: Compression::Identity,
            codec: Arc::new(move |b| codec.decode(b)),
            trailers: Vec::new(),
            done: false,
            deadline,
            max: self.max_message_size,
        };
        let msg = s.recv().await?;
        let (status, headers) = match msg {
            Some(UpResp::Head { status, headers }) => (status, headers),
            Some(UpResp::Trailers(t)) => (200, t),
            Some(_) | None => {
                return Err(Status::unavailable(
                    "the stream ended before the response headers",
                ))
            }
        };
        if status != 200 {
            return Err(Status::new(
                Code::from_http_status(status),
                format!("HTTP status {}", status),
            ));
        }
        // trailers-only（ヘッダに grpc-status）
        if let Some(st) = status_from(&headers) {
            s.done = true;
            s.stream = None;
            st?;
            s.trailers = headers.clone();
        }
        if let Some((_, v)) = headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(b"grpc-encoding"))
        {
            s.encoding = Compression::from_bytes(v).ok_or_else(|| {
                Status::internal(format!(
                    "unsupported grpc-encoding '{}'",
                    String::from_utf8_lossy(v)
                ))
            })?;
        }
        let metadata = headers
            .into_iter()
            .filter(|(n, _)| {
                !(n.eq_ignore_ascii_case(b"content-type")
                    || n.eq_ignore_ascii_case(b"grpc-encoding")
                    || n.eq_ignore_ascii_case(b"grpc-accept-encoding"))
            })
            .collect();
        Ok(Response {
            metadata,
            message: s,
            trailers: Vec::new(),
            compression: None,
        })
    }

    /// A unary call.
    pub async fn unary<C, Req, Resp>(
        &self,
        path: &str,
        codec: C,
        req: Request<Req>,
    ) -> Result<Response<Resp>, Status>
    where
        C: Codec<Req> + Codec<Resp>,
        Resp: 'static,
    {
        let (headers, limit) = self.headers(&req);
        let frame = self.frame(&codec, &req.message)?;
        let deadline = timing::Deadline::after(limit);
        let mut stream = self.open(path, &headers, false).await?;
        if let Some(tx) = &stream.req_tx {
            let _ = tx.send(frame).await;
        }
        stream.finish_request();
        let r = self.response::<Resp, C>(stream, codec, deadline).await?;
        let (metadata, mut s) = (r.metadata, r.message);
        let m = s
            .message()
            .await?
            .ok_or_else(|| Status::internal("the response has no message"))?;
        // 残り（トレーラ）を読む
        if s.message().await?.is_some() {
            return Err(Status::internal(
                "a unary response has more than one message",
            ));
        }
        Ok(Response {
            metadata,
            message: m,
            trailers: std::mem::take(&mut s.trailers),
            compression: None,
        })
    }

    /// A server-streaming call: one request, a stream of responses.
    pub async fn server_streaming<C, Req, Resp>(
        &self,
        path: &str,
        codec: C,
        req: Request<Req>,
    ) -> Result<Response<Streaming<Resp>>, Status>
    where
        C: Codec<Req> + Codec<Resp>,
        Resp: 'static,
    {
        let (headers, limit) = self.headers(&req);
        let frame = self.frame(&codec, &req.message)?;
        let deadline = timing::Deadline::after(limit);
        let mut stream = self.open(path, &headers, false).await?;
        if let Some(tx) = &stream.req_tx {
            let _ = tx.send(frame).await;
        }
        stream.finish_request();
        self.response::<Resp, C>(stream, codec, deadline).await
    }

    /// A client-streaming call: a stream of requests, one response.
    pub async fn client_streaming<C, Req, Resp, S>(
        &self,
        path: &str,
        codec: C,
        req: Request<S>,
    ) -> Result<Response<Resp>, Status>
    where
        C: Codec<Req> + Codec<Resp>,
        S: Stream<Item = Req> + Unpin + 'static,
        Req: 'static,
        Resp: 'static,
    {
        let r = self.bidi::<C, Req, Resp, S>(path, codec, req).await?;
        let (metadata, mut s) = (r.metadata, r.message);
        let m = s
            .message()
            .await?
            .ok_or_else(|| Status::internal("the response has no message"))?;
        if s.message().await?.is_some() {
            return Err(Status::internal(
                "a client-streaming response has more than one message",
            ));
        }
        Ok(Response {
            metadata,
            message: m,
            trailers: std::mem::take(&mut s.trailers),
            compression: None,
        })
    }

    /// A bidirectional-streaming call. The request stream is sent by a task on the current worker while the
    /// responses are read.
    pub async fn bidi<C, Req, Resp, S>(
        &self,
        path: &str,
        codec: C,
        req: Request<S>,
    ) -> Result<Response<Streaming<Resp>>, Status>
    where
        C: Codec<Req> + Codec<Resp>,
        S: Stream<Item = Req> + Unpin + 'static,
        Req: 'static,
        Resp: 'static,
    {
        let (headers, limit) = self.headers(&req);
        let deadline = timing::Deadline::after(limit);
        let mut stream = self.open(path, &headers, false).await?;
        let tx = stream.req_tx.take();
        let mut msgs = req.message;
        let me = self.clone();
        let enc = codec.clone();
        // 要求の送り手（送信端を drop すると END_STREAM）
        crate::runtime::spawn(async move {
            let Some(tx) = tx else { return };
            while let Some(m) = msgs.next().await {
                let f = match me.frame(&enc, &m) {
                    Ok(f) => f,
                    Err(_) => break,
                };
                if tx.send(f).await.is_err() {
                    break;
                }
            }
            drop(tx);
        });
        stream.finish_request();
        self.response::<Resp, C>(stream, codec, deadline).await
    }
}

/// `Some(Ok)` for `grpc-status: 0`, `Some(Err)` for an error status, `None` without `grpc-status`.
fn status_from(h: &[(Bytes, Bytes)]) -> Option<Result<(), Status>> {
    let v = h
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(b"grpc-status"))?;
    let code = crate::grpc::status::GrpcStatusCode::from_bytes(&v.1).unwrap_or(Code::Unknown);
    if code == Code::Ok {
        return Some(Ok(()));
    }
    let msg = h
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(b"grpc-message"))
        .map(|(_, v)| percent_decode(v))
        .unwrap_or_default();
    let mut st = Status::new(code, msg);
    for (n, v) in h {
        if !(n.eq_ignore_ascii_case(b"grpc-status") || n.eq_ignore_ascii_case(b"grpc-message")) {
            st = st.with_metadata(n.clone(), v.clone());
        }
    }
    Some(Err(st))
}

type DecodeFn<T> = Arc<dyn Fn(Bytes) -> Result<T, Status> + Send + Sync>;

/// Response messages of a streaming call. Read with [`Streaming::message`] (or [`Streaming::into_stream`]); the
/// trailers are available after the last message.
pub struct Streaming<T> {
    stream: Option<UpStream>,
    decoder: GrpcFrameDecoder,
    encoding: Compression,
    codec: DecodeFn<T>,
    trailers: Metadata,
    done: bool,
    deadline: timing::Deadline,
    max: usize,
}

impl<T> std::fmt::Debug for Streaming<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("client::Streaming")
            .field("done", &self.done)
            .finish()
    }
}

impl<T: 'static> Streaming<T> {
    async fn recv(&mut self) -> Result<Option<UpResp>, Status> {
        let Some(s) = &self.stream else {
            return Ok(None);
        };
        match crate::runtime::time::timeout(self.deadline.remaining(), s.resp_rx.recv()).await {
            Ok(m) => Ok(m),
            Err(_) => {
                self.stream = None;
                self.done = true;
                Err(Status::deadline_exceeded("deadline exceeded"))
            }
        }
    }

    /// The next message (`Ok(None)` after the last one, when the status was OK).
    pub async fn message(&mut self) -> Result<Option<T>, Status> {
        loop {
            match self.decoder.decode_next() {
                Ok(Some(frame)) => {
                    let data = if frame.compressed {
                        if self.encoding == Compression::Identity {
                            return Err(Status::internal(
                                "compressed message without grpc-encoding",
                            ));
                        }
                        self.encoding
                            .decompress_limited(&frame.data, self.max)
                            .map_err(|e| Status::resource_exhausted(e.to_string()))?
                    } else {
                        frame.data
                    };
                    return (self.codec)(Bytes::from(data)).map(Some);
                }
                Ok(None) => {}
                Err(e) => return Err(Status::resource_exhausted(e.to_string())),
            }
            if self.done {
                if self.decoder.has_pending() {
                    return Err(Status::internal(
                        "the response ended in the middle of a message",
                    ));
                }
                return Ok(None);
            }
            match self.recv().await? {
                Some(UpResp::Data(b)) => self.decoder.push(&b),
                Some(UpResp::Trailers(t)) => {
                    self.done = true;
                    self.stream = None;
                    match status_from(&t) {
                        Some(Ok(())) => self.trailers = t,
                        Some(Err(st)) => return Err(st),
                        None => return Err(Status::internal("the response has no grpc-status")),
                    }
                }
                Some(UpResp::Head { .. }) => {}
                Some(UpResp::Reset { .. }) => {
                    self.done = true;
                    self.stream = None;
                    return Err(Status::unavailable("the stream was reset"));
                }
                None => {
                    self.done = true;
                    self.stream = None;
                    if self.trailers.is_empty() {
                        return Err(Status::internal("the response ended without trailers"));
                    }
                }
            }
        }
    }

    /// The trailers (after the last message).
    pub fn trailers(&self) -> &[(Bytes, Bytes)] {
        &self.trailers
    }

    /// The messages as a `Stream` (for `StreamExt` combinators; it runs on the current worker).
    pub fn into_stream(self) -> futures::stream::LocalBoxStream<'static, Result<T, Status>> {
        futures::stream::unfold(Some(self), |s| async move {
            let mut s = s?;
            match s.message().await {
                Ok(Some(m)) => Some((Ok(m), Some(s))),
                Ok(None) => None,
                Err(e) => Some((Err(e), None)),
            }
        })
        .boxed_local()
    }
}

impl<T> Response<T> {
    /// The trailers received after the message (client calls).
    pub fn trailers(&self) -> &[(Bytes, Bytes)] {
        &self.trailers
    }
}

impl<T> Request<T> {
    /// Adds request metadata (client calls; the name is lowercased).
    pub fn with_metadata(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        self.metadata.push((lower(name.into()), value.into()));
        self
    }

    /// The deadline of this call (client calls; sent as `grpc-timeout`).
    pub fn with_timeout(mut self, d: Duration) -> Self {
        self.deadline = Some(std::time::Instant::now() + d);
        self
    }
}

/// A request message or a [`Request`] (so generated clients accept both).
pub trait IntoRequest<T> {
    /// Wraps the message (or returns the request as it is).
    fn into_request(self) -> Request<T>;
}

impl<T> IntoRequest<T> for T {
    fn into_request(self) -> Request<T> {
        Request::new(self)
    }
}

impl<T> IntoRequest<T> for Request<T> {
    fn into_request(self) -> Request<T> {
        self
    }
}

/// A stream of request messages or a [`Request`] holding one (for the streaming methods of generated clients).
pub trait IntoStreamingRequest {
    /// The stream type.
    type Stream: Stream<Item = Self::Message> + Unpin + 'static;
    /// The message type.
    type Message;
    /// Wraps the stream (or returns the request as it is).
    fn into_streaming_request(self) -> Request<Self::Stream>;
}

impl<S: Stream + Unpin + 'static> IntoStreamingRequest for S {
    type Stream = S;
    type Message = S::Item;
    fn into_streaming_request(self) -> Request<S> {
        Request::new(self)
    }
}

impl<S: Stream + Unpin + 'static> IntoStreamingRequest for Request<S> {
    type Stream = S;
    type Message = S::Item;
    fn into_streaming_request(self) -> Request<S> {
        self
    }
}

mod timing {
    use std::time::{Duration, Instant};

    /// The point in time a call gives up.
    #[derive(Clone, Copy)]
    pub(super) struct Deadline(Instant);

    impl Deadline {
        pub(super) fn after(d: Duration) -> Deadline {
            Deadline(Instant::now() + d)
        }
        pub(super) fn remaining(self) -> Duration {
            self.0.saturating_duration_since(Instant::now())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_is_read_from_trailers() {
        let ok = vec![(Bytes::from_static(b"grpc-status"), Bytes::from_static(b"0"))];
        assert!(matches!(status_from(&ok), Some(Ok(()))));
        let err = vec![
            (Bytes::from_static(b"grpc-status"), Bytes::from_static(b"5")),
            (
                Bytes::from_static(b"grpc-message"),
                Bytes::from_static(b"no%20such"),
            ),
        ];
        let st = status_from(&err).unwrap().unwrap_err();
        assert_eq!(st.code(), Code::NotFound);
        assert_eq!(st.message(), "no such");
        assert!(status_from(&[]).is_none());
    }

    #[test]
    fn channel_keeps_the_url_prefix() {
        let ch = Channel::connect("http://127.0.0.1:50051/base/").unwrap();
        assert_eq!(ch.prefix, "/base");
        let ch = Channel::connect("http://127.0.0.1:50051").unwrap();
        assert_eq!(ch.prefix, "");
    }
}
