// F-210
//! Bounded queues between the workers (asynchronous) and the handler-pool threads (blocking).
//!
//! C and Python handlers run on pool threads, and a worker's event loop must not block. Streaming bodies, gRPC
//! messages and L4 data are exchanged between the two through these queues.
//!
//! - `to_worker`: pool to worker. The pool side waits when the queue is full (backpressure); the worker side
//!   waits asynchronously until a value arrives.
//! - `to_pool`: worker to pool. The worker side waits asynchronously when the queue is full; the pool side waits
//!   until a value arrives.
//!
//! The pool side waits on a `Condvar`, and the worker side on a `WakeFlag` (a flag plus that worker's
//! notification fd); no waker is called across threads. When either end is dropped, sending and receiving on the
//! other end finish (`Err` / `None`). Use the asynchronous end on the thread of the worker that created it
//! (the notification fd belongs to that thread).

use crate::runtime::offload::WakeFlag;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

struct Shared<T> {
    q: Mutex<VecDeque<T>>,
    cv: Condvar,
    cap: usize,
    /// The sending side is closed.
    tx_closed: AtomicBool,
    /// The receiving side is closed.
    rx_closed: AtomicBool,
    /// Wakes the worker side.
    wake: WakeFlag,
}

impl<T> Shared<T> {
    fn new(cap: usize) -> Arc<Shared<T>> {
        Arc::new(Shared {
            q: Mutex::new(VecDeque::with_capacity(cap.min(64))),
            cv: Condvar::new(),
            cap: cap.max(1),
            tx_closed: AtomicBool::new(false),
            rx_closed: AtomicBool::new(false),
            wake: WakeFlag::new(),
        })
    }
}

// ===================== プール → ワーカー =====================

/// Creates a bounded pool-to-worker queue (call it on the worker's thread).
pub fn to_worker<T: Send>(cap: usize) -> (BlockingSender<T>, AsyncReceiver<T>) {
    let s = Shared::new(cap);
    (BlockingSender { s: s.clone() }, AsyncReceiver { s })
}

/// The pool-side sender (blocking).
pub struct BlockingSender<T> {
    s: Arc<Shared<T>>,
}

impl<T> BlockingSender<T> {
    /// Sends, waiting for space if the queue is full. If the receiver is closed, the value is handed back.
    pub fn send(&self, v: T) -> Result<(), T> {
        let mut q = self.s.q.lock().unwrap();
        while q.len() >= self.s.cap && !self.s.rx_closed.load(Ordering::Acquire) {
            q = self.s.cv.wait(q).unwrap();
        }
        if self.s.rx_closed.load(Ordering::Acquire) {
            return Err(v);
        }
        q.push_back(v);
        drop(q);
        self.s.wake.notify();
        Ok(())
    }

    /// Whether the receiver is closed (for example because the client disconnected).
    pub fn is_closed(&self) -> bool {
        self.s.rx_closed.load(Ordering::Acquire)
    }
}

impl<T> Drop for BlockingSender<T> {
    fn drop(&mut self) {
        self.s.tx_closed.store(true, Ordering::Release);
        self.s.wake.notify();
    }
}

/// The worker-side receiver (asynchronous).
pub struct AsyncReceiver<T> {
    s: Arc<Shared<T>>,
}

impl<T> AsyncReceiver<T> {
    /// The next value (`None` once the sender is closed and the queue is empty).
    pub async fn recv(&self) -> Option<T> {
        loop {
            self.s.wake.reset();
            {
                let mut q = self.s.q.lock().unwrap();
                if let Some(v) = q.pop_front() {
                    drop(q);
                    self.s.cv.notify_one();
                    return Some(v);
                }
                if self.s.tx_closed.load(Ordering::Acquire) {
                    return None;
                }
            }
            self.s.wake.wait().await;
        }
    }

    /// Takes a value without waiting.
    pub fn try_recv(&self) -> Option<T> {
        let v = self.s.q.lock().unwrap().pop_front();
        if v.is_some() {
            self.s.cv.notify_one();
        }
        v
    }

    /// Turns this into a `futures::Stream` (to hand to a response body and the like).
    pub fn into_stream(self) -> impl futures::Stream<Item = T> + Send + 'static
    where
        T: Send + 'static,
    {
        futures::stream::unfold(self, |rx| async move {
            let v = rx.recv().await?;
            Some((v, rx))
        })
    }
}

impl<T> Drop for AsyncReceiver<T> {
    fn drop(&mut self) {
        self.s.rx_closed.store(true, Ordering::Release);
        self.s.cv.notify_all();
    }
}

// ===================== ワーカー → プール =====================

/// Creates a bounded worker-to-pool queue (call it on the worker's thread).
pub fn to_pool<T: Send>(cap: usize) -> (AsyncSender<T>, BlockingReceiver<T>) {
    let s = Shared::new(cap);
    (AsyncSender { s: s.clone() }, BlockingReceiver { s })
}

/// The worker-side sender (asynchronous).
pub struct AsyncSender<T> {
    s: Arc<Shared<T>>,
}

impl<T> AsyncSender<T> {
    /// Sends, waiting asynchronously for space if the queue is full. If the receiver is closed, the value is handed back.
    pub async fn send(&self, v: T) -> Result<(), T> {
        let mut v = Some(v);
        loop {
            self.s.wake.reset();
            {
                let mut q = self.s.q.lock().unwrap();
                if self.s.rx_closed.load(Ordering::Acquire) {
                    return Err(v.take().expect("value"));
                }
                if q.len() < self.s.cap {
                    q.push_back(v.take().expect("value"));
                    drop(q);
                    self.s.cv.notify_one();
                    return Ok(());
                }
            }
            self.s.wake.wait().await;
        }
    }
}

impl<T> Drop for AsyncSender<T> {
    fn drop(&mut self) {
        self.s.tx_closed.store(true, Ordering::Release);
        self.s.cv.notify_all();
    }
}

/// The pool-side receiver (blocking).
pub struct BlockingReceiver<T> {
    s: Arc<Shared<T>>,
}

impl<T> BlockingReceiver<T> {
    /// The next value (`None` once the sender is closed and the queue is empty). Waits until a value arrives.
    pub fn recv(&self) -> Option<T> {
        let mut q = self.s.q.lock().unwrap();
        loop {
            if let Some(v) = q.pop_front() {
                let was_full = q.len() + 1 >= self.s.cap;
                drop(q);
                if was_full {
                    // 満杯で待っていたかもしれない送り手（ワーカー）を起こす
                    self.s.wake.notify();
                }
                return Some(v);
            }
            if self.s.tx_closed.load(Ordering::Acquire) {
                return None;
            }
            q = self.s.cv.wait(q).unwrap();
        }
    }
}

impl<T> Drop for BlockingReceiver<T> {
    fn drop(&mut self) {
        self.s.rx_closed.store(true, Ordering::Release);
        self.s.wake.notify();
    }
}

// ===================== ブロッキングなハンドラの実行（C・Python） =====================

use super::request::Request;
use super::response::Response;
use super::stream::BodyChunk;
use bytes::Bytes;

/// What reaches the worker from a pool handler.
enum Event {
    /// A complete response (not streamed).
    Full(Response),
    /// The start of a streaming response (status and headers).
    Head(Response),
    Chunk(BodyChunk),
}

// F-210
/// The context through which a blocking handler streams its response and reads the request body in order.
///
/// It is used on a handler-pool thread. The [`ResponseWriter`] returned by [`HandlerContext::start_stream`] can
/// be used on another thread even after the handler returns, and finishes the body when dropped.
pub struct HandlerContext {
    events: Arc<BlockingSender<Event>>,
    started: bool,
    body: Option<BlockingReceiver<Bytes>>,
}

impl HandlerContext {
    /// Turns the response into a stream (the status and headers of `head` are sent first; the body goes through
    /// the [`ResponseWriter`]). The second call returns `None`.
    pub fn start_stream(&mut self, head: Response) -> Option<ResponseWriter> {
        if self.started {
            return None;
        }
        self.started = true;
        self.events.send(Event::Head(head)).ok()?;
        Some(ResponseWriter {
            events: Some(self.events.clone()),
        })
    }

    /// Whether streaming has started.
    pub fn is_streaming(&self) -> bool {
        self.started
    }

    /// The next chunk of the request body (only for streaming-reception routes; `None` at the end or otherwise). Waits until it arrives.
    pub fn read_body(&mut self) -> Option<Bytes> {
        self.body.as_ref()?.recv()
    }

    /// Whether the request body is received as a stream.
    pub fn has_body_stream(&self) -> bool {
        self.body.is_some()
    }
}

/// Writes the body of a streaming response (from any thread; dropping it finishes the body).
pub struct ResponseWriter {
    events: Option<Arc<BlockingSender<Event>>>,
}

impl ResponseWriter {
    /// Sends a chunk of the body (waits if the queue is full). Returns `false` if the client has disconnected.
    pub fn write(&self, data: Bytes) -> bool {
        match &self.events {
            Some(e) => e.send(Event::Chunk(BodyChunk::Data(data))).is_ok(),
            None => false,
        }
    }

    /// Sends the trailers and finishes the body.
    pub fn finish_with_trailers(mut self, trailers: Vec<(Bytes, Bytes)>) -> bool {
        match self.events.take() {
            Some(e) => e.send(Event::Chunk(BodyChunk::Trailers(trailers))).is_ok(),
            None => false,
        }
    }

    /// Aborts midway (RST_STREAM on HTTP/2, closing the connection on HTTP/1.1).
    pub fn abort(mut self, reason: &str) {
        if let Some(e) = self.events.take() {
            let _ = e.send(Event::Chunk(BodyChunk::Abort(reason.to_string())));
        }
    }

    /// Whether the client has disconnected.
    pub fn is_closed(&self) -> bool {
        self.events.as_ref().is_none_or(|e| e.is_closed())
    }
}

// F-210: C・Python の既定の経路。
/// Runs a blocking handler `f` on the handler pool.
///
/// `f` either returns a complete response or turns the response into a stream with [`HandlerContext::start_stream`]
/// (its return value is then ignored). On a streaming-reception route, the request body can be read in order with
/// [`HandlerContext::read_body`]. It returns as soon as the response starts (without waiting for `f` to return),
/// so sending and receiving the body proceed concurrently.
pub async fn call_blocking<F>(req: Request, f: F) -> Response
where
    F: FnOnce(Request, &mut HandlerContext) -> Response + Send + 'static,
{
    use futures::FutureExt;
    let (ev_tx, ev_rx) = to_worker::<Event>(16);
    let ev_tx = Arc::new(ev_tx);
    // 要求本文の中継（プール側は read_body で受ける）
    let (pump, body_rx) = if req.is_body_streaming() {
        let stream = req.take_body_stream();
        let (btx, brx) = to_pool::<Bytes>(8);
        let pump = async move {
            use futures::StreamExt;
            let mut stream = stream;
            while let Some(chunk) = stream.next().await {
                if btx.send(chunk).await.is_err() {
                    break; // ハンドラが本文を読まなくなった
                }
            }
        };
        (Some(pump.boxed()), Some(brx))
    } else {
        (None, None)
    };
    {
        let ev_tx = ev_tx.clone();
        crate::runtime::offload::spawn_handler(move || {
            let mut ctx = HandlerContext {
                events: ev_tx.clone(),
                started: false,
                body: body_rx,
            };
            let resp = f(req, &mut ctx);
            if !ctx.started {
                let _ = ev_tx.send(Event::Full(resp));
            }
        });
    }
    drop(ev_tx);
    let mut pump = pump;
    let first = next_event(&ev_rx, &mut pump).await;
    match first {
        Some(Event::Full(resp)) => resp,
        Some(Event::Head(mut head)) => {
            let status = head.status;
            let headers = std::mem::take(&mut head.headers);
            let body = futures::stream::unfold((ev_rx, pump), |(rx, mut pump)| async move {
                loop {
                    match next_event(&rx, &mut pump).await? {
                        Event::Chunk(c) => return Some((c, (rx, pump))),
                        // Head・Full は最初の 1 回だけ
                        _ => continue,
                    }
                }
            });
            let mut r = Response::stream(status, body);
            r.headers = headers;
            r
        }
        // ハンドラが panic した等で何も届かなかった
        _ => Response::error(500, "Internal Server Error"),
    }
}

/// Waits for the next event while also relaying the request body (received messages, for gRPC).
pub(crate) async fn next_event<T>(
    rx: &AsyncReceiver<T>,
    pump: &mut Option<futures::future::BoxFuture<'static, ()>>,
) -> Option<T> {
    loop {
        let Some(p) = pump.as_mut() else {
            return rx.recv().await;
        };
        // 本文の中継とハンドラからの出来事はどちらもキューの待ちだけで、起こされていない側を poll しても
        // syscall は出ない
        match futures::future::select(std::pin::pin!(rx.recv()), p).await {
            futures::future::Either::Left((ev, _)) => return ev,
            futures::future::Either::Right(((), _)) => *pump = None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_worker_backpressure_order_and_close() {
        // ドライバ無し（ポーリングで待つ）でも順序・背圧・終わりが正しい
        let (tx, rx) = to_worker::<u32>(2);
        let h = std::thread::spawn(move || {
            for i in 0..100 {
                tx.send(i).unwrap();
            }
        });
        let got: Vec<u32> = futures::executor::block_on(async {
            let mut v = Vec::new();
            while let Some(x) = rx.recv().await {
                v.push(x);
            }
            v
        });
        h.join().unwrap();
        assert_eq!(got, (0..100).collect::<Vec<_>>());
    }

    #[test]
    // 理由付き allow: 試験のスレッドで送り手が満杯で待つまでの間を置く（イベントループの外）
    #[allow(clippy::disallowed_methods)]
    fn to_worker_receiver_drop_unblocks_sender() {
        let (tx, rx) = to_worker::<u32>(1);
        tx.send(1).unwrap();
        let h = std::thread::spawn(move || tx.send(2));
        std::thread::sleep(std::time::Duration::from_millis(20));
        drop(rx);
        assert_eq!(h.join().unwrap(), Err(2));
    }

    #[test]
    fn to_pool_order_and_close() {
        let (tx, rx) = to_pool::<u32>(2);
        let h = std::thread::spawn(move || {
            let mut v = Vec::new();
            while let Some(x) = rx.recv() {
                v.push(x);
            }
            v
        });
        futures::executor::block_on(async {
            for i in 0..50 {
                tx.send(i).await.unwrap();
            }
        });
        drop(tx);
        assert_eq!(h.join().unwrap(), (0..50).collect::<Vec<_>>());
    }

    /// Calls `call_blocking` on top of the runtime (the notification-fd driver) and reads the response to the end.
    /// Without the driver, waiting degrades to polling and relaying the request body does not progress. Skipped (None)
    /// where neither io_uring nor epoll is available.
    fn run<F>(req: Request, f: F) -> Option<(u16, Vec<u8>, Vec<(Bytes, Bytes)>)>
    where
        F: FnOnce(Request, &mut HandlerContext) -> Response + Send + 'static,
    {
        #[cfg(veil_rt_uring)]
        if crate::runtime::ring::IoUring::new(8, 0).is_err() {
            return None;
        }
        Some(crate::runtime::block_on(async move {
            use futures::StreamExt;
            let mut resp = call_blocking(req, f).await;
            let mut data = resp.body().to_vec();
            let mut trailers = Vec::new();
            if let Some(mut s) = resp.take_stream() {
                while let Some(c) = s.next().await {
                    match c {
                        BodyChunk::Data(b) => data.extend_from_slice(&b),
                        BodyChunk::Trailers(t) => trailers = t,
                        BodyChunk::Abort(r) => panic!("abort {}", r),
                    }
                }
            }
            (resp.status_code(), data, trailers)
        }))
    }

    #[test]
    fn call_blocking_full_and_streaming() {
        let req = Request::new("GET", "/", Vec::new(), "");
        let Some(r) = run(req, |_r, _ctx| Response::text("full")) else {
            return;
        };
        assert_eq!(r, (200, b"full".to_vec(), vec![]));

        let req = Request::new("GET", "/", Vec::new(), "");
        let (status, data, trailers) = run(req, |_r, ctx| {
            let w = ctx.start_stream(Response::new(201)).unwrap();
            // 戻った後も別のスレッドで書ける
            std::thread::spawn(move || {
                for i in 0..50 {
                    assert!(w.write(Bytes::from(format!("{},", i))));
                }
                w.finish_with_trailers(vec![(
                    Bytes::from_static(b"x-n"),
                    Bytes::from_static(b"50"),
                )]);
            });
            Response::text("ignored")
        })
        .unwrap();
        assert_eq!(status, 201);
        let want: String = (0..50).map(|i| format!("{},", i)).collect();
        assert_eq!(String::from_utf8(data).unwrap(), want);
        assert_eq!(trailers[0].1, Bytes::from_static(b"50"));
    }

    #[test]
    fn call_blocking_streaming_request_body() {
        #[cfg(veil_rt_uring)]
        if crate::runtime::ring::IoUring::new(8, 0).is_err() {
            return;
        }
        let got = crate::runtime::block_on(async {
            // 本文はプロトコル側と同じくワーカーのタスクから流す（別スレッドから Waker を呼ばない）
            let (mut tx, slot) = crate::api::stream::request_body_channel(1);
            let mut req = Request::new("POST", "/", Vec::new(), "");
            req.body_stream = Some(slot);
            crate::runtime::spawn(async move {
                for i in 0..20 {
                    if tx
                        .send(Bytes::from(vec![b'a' + (i % 26) as u8; 1000]))
                        .await
                        .is_err()
                    {
                        return;
                    }
                }
            });
            let resp = call_blocking(req, |_r, ctx| {
                assert!(ctx.has_body_stream());
                let mut n = 0;
                while let Some(b) = ctx.read_body() {
                    n += b.len();
                }
                Response::text(n.to_string())
            })
            .await;
            resp.body().clone()
        });
        assert_eq!(&got[..], b"20000");
    }

    #[test]
    fn call_blocking_panic_is_500() {
        let req = Request::new("GET", "/", Vec::new(), "");
        if let Some(r) = run(req, |_r, _ctx| panic!("boom")) {
            assert_eq!(r.0, 500);
        }
    }

    #[test]
    fn to_pool_receiver_drop_fails_send() {
        let (tx, rx) = to_pool::<u32>(1);
        drop(rx);
        assert_eq!(futures::executor::block_on(tx.send(1)), Err(1));
    }
}
