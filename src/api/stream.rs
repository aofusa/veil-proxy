//! ハンドラ経路のストリーミング本文とトレーラ（F-197）。gRPC 以外（大きな応答・SSE 等）にも使える。
//!
//! - 応答: `Response::stream(..)` で本文を断片（`BodyChunk::Data`）で送り、最後に `BodyChunk::Trailers`
//!   （HTTP/2・HTTP/3 のトレーラ、HTTP/1.1 は chunked のトレーラ）を送れる。`BodyChunk::Abort` は途中で
//!   ストリームを中断する（HTTP/2 は RST_STREAM、HTTP/3 はストリームリセット、HTTP/1.1 は接続を閉じる）。
//! - 要求: ストリーミング受信を有効にしたルート（`streaming_request`）では `Request::take_body_stream()`
//!   で本文を届いた順に受け取れる（HTTP/2。HTTP/1.1・HTTP/3 では全量を 1 断片として渡す）。
//!
//! 本文の流れはワーカースレッドの中で完結する（送り手・受け手とも同じスレッドの Future）。

use bytes::Bytes;
use futures::Stream;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

/// 応答本文の 1 要素。
#[derive(Debug, Clone)]
pub enum BodyChunk {
    /// 本文の断片
    Data(Bytes),
    /// トレーラ（これでストリームを終える）
    Trailers(Vec<(Bytes, Bytes)>),
    /// 途中で中断する（理由はログ用）
    Abort(String),
}

/// 応答本文のストリーム（`Send` を要求するのは、ブロッキングなハンドラが別スレッドで作ることがあるため）。
pub type BodyStream = Pin<Box<dyn Stream<Item = BodyChunk> + Send + 'static>>;

/// 要求本文のストリーム（届いた順の断片）。
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

/// `Request` が持つ受信本文ストリームの置き場（`Request` の複製で共有し、最初に取った者が使う）。
pub(crate) type BodyStreamSlot = Arc<Mutex<Option<RequestBodyStream>>>;

/// 送り手（プロトコル側）と、ハンドラへ渡す受け手を作る。容量は断片の個数。
pub(crate) fn request_body_channel(
    capacity: usize,
) -> (futures::channel::mpsc::Sender<Bytes>, BodyStreamSlot) {
    let (tx, rx) = futures::channel::mpsc::channel(capacity);
    (tx, Arc::new(Mutex::new(Some(RequestBodyStream { rx }))))
}

/// 送出側が 1 回にまとめる本文の上限（HTTP/2 の既定 DATA フレーム長に合わせる）。
pub(crate) const COALESCE_MAX: usize = 16 * 1024;

/// 既に用意できている後続の `Data` を `max` バイトまで `first` に連結する（待たない）。
///
/// 小さな断片（gRPC の小さなメッセージ等）を 1 つずつ送るとフレーム数・syscall が断片の数だけ増え、
/// クライアントが取り消した直後に届く大量のフレームを相手が攻撃と見なす（h2 の
/// `max_local_error_reset_streams`）。連結できない要素（トレーラ・中断・上限を超える断片）は `carry` に返す。
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
