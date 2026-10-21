// F-197
//! Streaming bodies and trailers on the handler path. Usable beyond gRPC (large responses, SSE and so on).
//!
//! - Response: `Response::stream(..)` sends the body in chunks (`BodyChunk::Data`) and can finish with
//!   `BodyChunk::Trailers` (trailers on HTTP/2 and HTTP/3; chunked trailers on HTTP/1.1). `BodyChunk::Abort`
//!   aborts the stream midway (RST_STREAM on HTTP/2, a stream reset on HTTP/3, closing the connection on HTTP/1.1).
//! - Request: on a route with streaming reception enabled (`streaming_request`), `Request::take_body_stream()`
//!   yields the body in arrival order (HTTP/2; on HTTP/1.1 and HTTP/3 the whole body is passed as one chunk).
//!
//! The body flow completes within a worker thread (both sender and receiver are futures on the same thread).

use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// One element of a response body.
#[derive(Debug, Clone)]
pub enum BodyChunk {
    /// A chunk of the body.
    Data(Bytes),
    /// Trailers (these end the stream).
    Trailers(Vec<(Bytes, Bytes)>),
    /// Aborts midway (the reason is for logging).
    Abort(String),
}

/// A stream of the response body (`Send` is required because a blocking handler may build it on another thread).
pub type BodyStream = Pin<Box<dyn Stream<Item = BodyChunk> + Send + 'static>>;

/// A stream of the request body (chunks in arrival order).
pub struct RequestBodyStream {
    pub(crate) rx: futures::channel::mpsc::Receiver<Bytes>,
}

impl Stream for RequestBodyStream {
    type Item = Bytes;
    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Bytes>> {
        Pin::new(&mut self.rx).poll_next(cx)
    }
}

impl std::fmt::Debug for RequestBodyStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RequestBodyStream")
    }
}

/// The place where a `Request` keeps its receive-body stream (shared across clones of the `Request`; whoever takes it first uses it).
pub(crate) type BodyStreamSlot = Arc<Mutex<Option<RequestBodyStream>>>;

/// Creates a sender (on the protocol side) and the receiver handed to the handler. The capacity is a number of chunks.
pub(crate) fn request_body_channel(
    capacity: usize,
) -> (futures::channel::mpsc::Sender<Bytes>, BodyStreamSlot) {
    let (tx, rx) = futures::channel::mpsc::channel(capacity);
    (tx, Arc::new(Mutex::new(Some(RequestBodyStream { rx }))))
}

/// Upper limit of body data the sender combines into one send (matches the default HTTP/2 DATA frame size).
pub(crate) const COALESCE_MAX: usize = 16 * 1024;

/// Appends the already available following `Data` chunks to `first`, up to `max` bytes (without waiting).
///
/// Sending small chunks (such as small gRPC messages) one by one multiplies the number of frames and syscalls by
/// the number of chunks, and the peer treats the flood of frames arriving right after the client cancels as an
/// attack (`max_local_error_reset_streams` in h2). Elements that cannot be combined (trailers, an abort, or a chunk
/// exceeding the limit) are returned in `carry`.
pub(crate) fn coalesce(
    first: Bytes,
    stream: &mut BodyStream,
    max: usize,
    carry: &mut Option<BodyChunk>,
) -> Bytes {
    use futures::{FutureExt, StreamExt};
    if first.len() >= max {
        return first;
    }
    let mut buf: Option<bytes::BytesMut> = None;
    let mut len = first.len();
    loop {
        match stream.next().now_or_never() {
            Some(Some(BodyChunk::Data(b))) if b.is_empty() => {}
            Some(Some(BodyChunk::Data(b))) if len + b.len() <= max => {
                let out = buf.get_or_insert_with(|| {
                    let mut m = bytes::BytesMut::with_capacity(max);
                    m.extend_from_slice(&first);
                    m
                });
                out.extend_from_slice(&b);
                len += b.len();
            }
            Some(Some(other)) => {
                *carry = Some(other);
                break;
            }
            // 終端（None）は次の `next()` でもう一度 None が返る
            Some(None) | None => break,
        }
    }
    match buf {
        Some(b) => b.freeze(),
        None => first,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::{SinkExt, StreamExt};

    #[test]
    fn request_body_channel_delivers_in_order_and_ends() {
        let (mut tx, slot) = request_body_channel(2);
        let mut rx = slot.lock().unwrap().take().unwrap();
        assert!(slot.lock().unwrap().is_none(), "二度目は取れない");
        futures::executor::block_on(async move {
            tx.send(Bytes::from_static(b"a")).await.unwrap();
            tx.send(Bytes::from_static(b"b")).await.unwrap();
            drop(tx);
            assert_eq!(rx.next().await.as_deref(), Some(&b"a"[..]));
            assert_eq!(rx.next().await.as_deref(), Some(&b"b"[..]));
            assert!(rx.next().await.is_none());
        });
    }

    #[test]
    fn coalesce_merges_ready_chunks_and_carries_the_rest() {
        let mut s: BodyStream = Box::pin(futures::stream::iter(vec![
            BodyChunk::Data(Bytes::from_static(b"bb")),
            BodyChunk::Data(Bytes::from_static(b"cc")),
            BodyChunk::Data(Bytes::from_static(b"dddddd")),
            BodyChunk::Trailers(vec![]),
        ]));
        let mut carry = None;
        let out = coalesce(Bytes::from_static(b"aa"), &mut s, 6, &mut carry);
        assert_eq!(&out[..], b"aabbcc");
        assert!(matches!(carry.take(), Some(BodyChunk::Data(b)) if &b[..] == b"dddddd"));
        let out = coalesce(Bytes::from_static(b"x"), &mut s, 6, &mut carry);
        assert_eq!(&out[..], b"x");
        assert!(matches!(carry, Some(BodyChunk::Trailers(_))));
    }

    #[test]
    fn take_body_stream_falls_back_to_full_body() {
        let req = crate::api::request::Request::new("POST", "/", Vec::new(), "xyz");
        assert!(!req.is_body_streaming());
        let chunks: Vec<Bytes> = futures::executor::block_on(req.take_body_stream().collect());
        assert_eq!(chunks, vec![Bytes::from_static(b"xyz")]);
    }
}
