//! ブロッキングな gRPC のハンドラ（C・Python。F-210）。
//!
//! メッセージは符号化されたバイト列のまま受け渡す（`super::BytesCodec`）。直列化は利用者（C の protobuf-c・
//! Python の protobuf 等）が行う。4 種類の RPC はワイヤ上は同じ形なので、どれも双方向ストリーミングとして登録し、
//! 単項・サーバストリーミングなら受信は 1 通、単項・クライアントストリーミングなら送信は 1 通に絞るのは呼び出し側
//! （C・Python の層）の役目。ハンドラはハンドラ用プールで動き、受信と送信は [`crate::api::bridge`] のキューで
//! ワーカーとやり取りする（全二重）。

use super::{BoxStream, BytesCodec, Code, Metadata, Request, Response, Status, Streaming};
use crate::api::app::App;
use crate::api::bridge::{next_event, to_pool, to_worker, BlockingReceiver, BlockingSender};
use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use std::sync::Arc;
use std::time::Instant;

enum Event {
    /// 応答ヘッダ（最初の送信か終わりのとき 1 回）
    Head(Metadata),
    Msg(Bytes),
    /// 最後の状態（OK でもトレーラを載せる）
    Finish(Status),
}

/// ハンドラに渡す 1 回の呼び出し（ハンドラ用プールのスレッドで使う）。
pub struct GrpcCall {
    method: Bytes,
    metadata: Metadata,
    deadline: Option<Instant>,
    incoming: BlockingReceiver<Result<Bytes, Status>>,
    events: Arc<BlockingSender<Event>>,
    headers: Metadata,
    trailers: Metadata,
    head_sent: bool,
    received: usize,
    sent: usize,
}

impl GrpcCall {
    /// 呼ばれたメソッド（`/<package>.<Service>/<Method>`）。
    pub fn method(&self) -> &str {
        std::str::from_utf8(&self.method).unwrap_or("")
    }

    /// 要求メタデータ。
    pub fn metadata(&self) -> &[(Bytes, Bytes)] {
        &self.metadata
    }

    /// 期限（`grpc-timeout`）。
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// 次の受信メッセージ（終わりは `Ok(None)`、壊れた・大きすぎるメッセージは `Err`）。届くまで待つ。
    pub fn recv(&mut self) -> Result<Option<Bytes>, Status> {
        match self.incoming.recv() {
            Some(Ok(b)) => {
                self.received += 1;
                Ok(Some(b))
            }
            Some(Err(s)) => Err(s),
            None => Ok(None),
        }
    }

    /// これまでに受け取った・送ったメッセージの数。
    pub fn counts(&self) -> (usize, usize) {
        (self.received, self.sent)
    }

    /// 応答ヘッダに足す（最初の送信より前だけ。後なら `false`）。
    pub fn add_header(&mut self, name: Bytes, value: Bytes) -> bool {
        if self.head_sent {
            return false;
        }
        self.headers.push((name, value));
        true
    }

    /// 正常終了のトレーラに足す。
    pub fn add_trailer(&mut self, name: Bytes, value: Bytes) {
        self.trailers.push((name, value));
    }

    fn send_head(&mut self) -> bool {
        if self.head_sent {
            return true;
        }
        self.head_sent = true;
        self.events
            .send(Event::Head(std::mem::take(&mut self.headers)))
            .is_ok()
    }

    /// メッセージを送る（満杯なら待つ）。クライアントが切断していたら `false`。
    pub fn send(&mut self, msg: Bytes) -> bool {
        if !self.send_head() {
            return false;
        }
        self.sent += 1;
        self.events.send(Event::Msg(msg)).is_ok()
    }

    /// クライアントが切断したか。
    pub fn is_cancelled(&self) -> bool {
        self.events.is_closed()
    }

    fn finish(mut self, status: Status) {
        // 何も送らずにエラーで終えるなら Trailers-Only（ヘッダを送らない）。正常終了はヘッダを送る
        if status.code() == Code::Ok {
            self.send_head();
        }
        let mut st = status;
        if st.code() == Code::Ok {
            for (n, v) in std::mem::take(&mut self.trailers) {
                st = st.with_metadata(n, v);
            }
        }
        let _ = self.events.send(Event::Finish(st));
    }
}

impl App {
    /// ブロッキングな gRPC のハンドラを登録する（C・Python 用、F-210）。`f` はハンドラ用プールで動き、
    /// [`GrpcCall`] で受信・送信し、最後の状態を返す（`Status::new(Code::Ok, "")` で正常終了）。
    pub fn grpc_blocking<F>(self, path: &str, f: F) -> App
    where
        F: Fn(&mut GrpcCall) -> Status + Send + Sync + 'static,
    {
        let f = Arc::new(f);
        self.grpc_bidi(path, BytesCodec, move |req: Request<Streaming<Bytes>>| {
            let f = f.clone();
            async move { start(req, f).await }
        })
    }
}

async fn start<F>(
    req: Request<Streaming<Bytes>>,
    f: Arc<F>,
) -> Result<Response<BoxStream<Bytes>>, Status>
where
    F: Fn(&mut GrpcCall) -> Status + Send + Sync + 'static,
{
    let method = req.path.clone();
    let metadata = req.metadata.clone();
    let deadline = req.deadline;
    let mut incoming = req.into_inner();
    let (in_tx, in_rx) = to_pool::<Result<Bytes, Status>>(8);
    let (ev_tx, ev_rx) = to_worker::<Event>(16);
    let ev_tx = Arc::new(ev_tx);
    let pump = async move {
        while let Some(m) = incoming.next().await {
            let stop = m.is_err();
            if in_tx.send(m).await.is_err() || stop {
                break;
            }
        }
    }
    .boxed();
    {
        let events = ev_tx.clone();
        crate::runtime::offload::spawn_handler(move || {
            let mut call = GrpcCall {
                method,
                metadata,
                deadline,
                incoming: in_rx,
                events,
                headers: Vec::new(),
                trailers: Vec::new(),
                head_sent: false,
                received: 0,
                sent: 0,
            };
            let status =
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut call))) {
                    Ok(s) => s,
                    Err(_) => Status::internal("handler panicked"),
                };
            call.finish(status);
        });
    }
    drop(ev_tx);
    let mut pump = Some(pump);
    let headers = match next_event(&ev_rx, &mut pump).await {
        Some(Event::Head(h)) => h,
        // 何も送らずにエラーで終えた（Trailers-Only）
        Some(Event::Finish(st)) => return Err(st),
        _ => return Err(Status::internal("handler ended without a response")),
    };
    let body = futures::stream::unfold((ev_rx, pump), |(rx, mut pump)| async move {
        match next_event(&rx, &mut pump).await? {
            Event::Msg(b) => Some((Ok(b), (rx, pump))),
            // OK でも Err で終える（GrpcBody はその状態とメタデータをトレーラにして終わる）
            Event::Finish(st) => Some((Err(st), (rx, pump))),
            Event::Head(_) => Some((Err(Status::internal("duplicate head")), (rx, pump))),
        }
    });
    let mut resp = Response::new(Box::pin(body) as BoxStream<Bytes>);
    for (n, v) in headers {
        resp = resp.with_metadata(n, v);
    }
    Ok(resp)
}
