// F-210
//! Blocking gRPC handlers (C and Python).
//!
//! Messages are passed as encoded bytes (`super::BytesCodec`); serialization is done by the user (protobuf-c in
//! C, protobuf in Python and so on). The four kinds of RPC have the same shape on the wire, so all are registered
//! as bidirectional streaming; narrowing the receive side to one message for unary and server-streaming calls,
//! and the send side to one message for unary and client-streaming calls, is the job of the caller (the C and
//! Python layers). The handler runs on the handler pool and exchanges received and sent messages with the worker
//! through the queues of [`crate::api::bridge`] (full duplex).

use super::{BoxStream, BytesCodec, Code, Metadata, Request, Response, Status, Streaming};
use crate::api::app::App;
use crate::api::bridge::{next_event, to_pool, to_worker, BlockingReceiver, BlockingSender};
use bytes::Bytes;
use futures::{FutureExt, StreamExt};
use std::sync::Arc;
use std::time::Instant;

enum Event {
    /// The response headers (once, at the first send or at the end).
    Head(Metadata),
    Msg(Bytes),
    /// The final status (trailers are sent even for OK).
    Finish(Status),
}

/// One call passed to the handler (used on a handler-pool thread).
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
    /// The method called (`/<package>.<Service>/<Method>`).
    pub fn method(&self) -> &str {
        std::str::from_utf8(&self.method).unwrap_or("")
    }

    /// The request metadata.
    pub fn metadata(&self) -> &[(Bytes, Bytes)] {
        &self.metadata
    }

    /// The deadline (`grpc-timeout`).
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// The next received message (`Ok(None)` at the end; a malformed or oversized message gives `Err`). Waits until one arrives.
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

    /// The number of messages received and sent so far.
    pub fn counts(&self) -> (usize, usize) {
        (self.received, self.sent)
    }

    /// Adds a response header (only before the first send; `false` afterwards).
    pub fn add_header(&mut self, name: Bytes, value: Bytes) -> bool {
        if self.head_sent {
            return false;
        }
        self.headers.push((name, value));
        true
    }

    /// Adds a trailer for a normal completion.
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

    /// Sends a message (waits if the queue is full). Returns `false` if the client has disconnected.
    pub fn send(&mut self, msg: Bytes) -> bool {
        if !self.send_head() {
            return false;
        }
        self.sent += 1;
        self.events.send(Event::Msg(msg)).is_ok()
    }

    /// Whether the client has disconnected.
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
    // F-210: C・Python 用。
    /// Registers a blocking gRPC handler. `f` runs on the handler pool, receives and sends through the [`GrpcCall`],
    /// and returns the final status (`Status::new(Code::Ok, "")` for a normal completion).
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
