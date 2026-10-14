//! ワーカー（非同期）とハンドラ用プールのスレッド（ブロッキング）の間の有界キュー（F-210）。
//!
//! C・Python のハンドラはプールのスレッドで動き、ワーカーのイベントループはブロックできない。この 2 つの間で
//! ストリーミングの本文・gRPC のメッセージ・L4 のデータをやり取りする。
//!
//! - [`to_worker`]: プール → ワーカー。プール側は満杯なら待ち（背圧）、ワーカー側は届くまで非同期に待つ。
//! - [`to_pool`]: ワーカー → プール。ワーカー側は満杯なら非同期に待ち、プール側は届くまで待つ。
//!
//! プール側の待ちは `Condvar`、ワーカー側の待ちは [`WakeFlag`]（旗 + そのワーカーの通知 fd）。スレッド越しに
//! Waker を呼ばない（AGENTS.md）。どちらかの端が drop されたら、もう一方の送受信は終わる（`Err` / `None`）。
//! 非同期の端は作ったワーカーのスレッドで使うこと（通知 fd がそのスレッドのもの）。

use crate::runtime::offload::WakeFlag;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};

struct Shared<T> {
    q: Mutex<VecDeque<T>>,
    cv: Condvar,
    cap: usize,
    /// 送る側が閉じた
    tx_closed: AtomicBool,
    /// 受ける側が閉じた
    rx_closed: AtomicBool,
    /// ワーカー側を起こす
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

/// プール → ワーカーの有界キューを作る（ワーカーのスレッドで呼ぶ）。
pub fn to_worker<T: Send>(cap: usize) -> (BlockingSender<T>, AsyncReceiver<T>) {
    let s = Shared::new(cap);
    (BlockingSender { s: s.clone() }, AsyncReceiver { s })
}

/// プール側の送り手（ブロックする）。
pub struct BlockingSender<T> {
    s: Arc<Shared<T>>,
}

impl<T> BlockingSender<T> {
    /// 送る。満杯なら空くまで待つ。受け手が閉じていたら値を返す。
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

    /// 受け手が閉じたか（クライアントが切断した等）。
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

/// ワーカー側の受け手（非同期）。
pub struct AsyncReceiver<T> {
    s: Arc<Shared<T>>,
}

impl<T> AsyncReceiver<T> {
    /// 次の値（送り手が閉じて空なら `None`）。
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

    /// 待たずに取り出す。
    pub fn try_recv(&self) -> Option<T> {
        let v = self.s.q.lock().unwrap().pop_front();
        if v.is_some() {
            self.s.cv.notify_one();
        }
        v
    }

    /// `futures::Stream` にする（応答本文等に渡す）。
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

/// ワーカー → プールの有界キューを作る（ワーカーのスレッドで呼ぶ）。
pub fn to_pool<T: Send>(cap: usize) -> (AsyncSender<T>, BlockingReceiver<T>) {
    let s = Shared::new(cap);
    (AsyncSender { s: s.clone() }, BlockingReceiver { s })
}

/// ワーカー側の送り手（非同期）。
pub struct AsyncSender<T> {
    s: Arc<Shared<T>>,
}

impl<T> AsyncSender<T> {
    /// 送る。満杯なら空くまで非同期に待つ。受け手が閉じていたら値を返す。
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

/// プール側の受け手（ブロックする）。
pub struct BlockingReceiver<T> {
    s: Arc<Shared<T>>,
}

impl<T> BlockingReceiver<T> {
    /// 次の値（送り手が閉じて空なら `None`）。届くまで待つ。
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

/// プールのハンドラからワーカーへ届くもの。
enum Event {
    /// 全量の応答（ストリーミングしなかった）
    Full(Response),
    /// ストリーミングの応答の始まり（ステータスとヘッダ）
    Head(Response),
    Chunk(BodyChunk),
}

/// ブロッキングなハンドラが応答をストリーミングにする・要求本文を順に読むための文脈（F-210）。
///
/// ハンドラ用プールのスレッドで使う。[`HandlerContext::start_stream`] で返る [`ResponseWriter`] は
/// ハンドラから戻った後も別のスレッドで使え、drop で本文を終える。
pub struct HandlerContext {
    events: Arc<BlockingSender<Event>>,
    started: bool,
    body: Option<BlockingReceiver<Bytes>>,
}

impl HandlerContext {
    /// 応答をストリーミングにする（`head` のステータスとヘッダを先に送る。本文は `ResponseWriter` で）。
    /// 2 回目は `None`。
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

    /// ストリーミングを始めたか。
    pub fn is_streaming(&self) -> bool {
        self.started
    }

    /// 要求本文の次の断片（ストリーミング受信のルートだけ。終わり・それ以外は `None`）。届くまで待つ。
    pub fn read_body(&mut self) -> Option<Bytes> {
        self.body.as_ref()?.recv()
    }

    /// 要求本文をストリーミングで受けているか。
    pub fn has_body_stream(&self) -> bool {
        self.body.is_some()
    }
}

/// ストリーミングの応答の本文を書く（どのスレッドからでも。drop で終わる）。
pub struct ResponseWriter {
    events: Option<Arc<BlockingSender<Event>>>,
}

impl ResponseWriter {
    /// 本文の断片を送る（満杯なら待つ）。クライアントが切断していたら `false`。
    pub fn write(&self, data: Bytes) -> bool {
        match &self.events {
            Some(e) => e.send(Event::Chunk(BodyChunk::Data(data))).is_ok(),
            None => false,
        }
    }

    /// トレーラを送って本文を終える。
    pub fn finish_with_trailers(mut self, trailers: Vec<(Bytes, Bytes)>) -> bool {
        match self.events.take() {
            Some(e) => e.send(Event::Chunk(BodyChunk::Trailers(trailers))).is_ok(),
            None => false,
        }
    }

    /// 途中で中断する（HTTP/2 は RST_STREAM、HTTP/1.1 は接続を閉じる）。
    pub fn abort(mut self, reason: &str) {
        if let Some(e) = self.events.take() {
            let _ = e.send(Event::Chunk(BodyChunk::Abort(reason.to_string())));
        }
    }

    /// クライアントが切断したか。
    pub fn is_closed(&self) -> bool {
        self.events.as_ref().is_none_or(|e| e.is_closed())
    }
}

/// ブロッキングなハンドラ `f` をハンドラ用プールで実行する（C・Python の既定の経路、F-210）。
///
/// `f` は全量の応答を返すか、[`HandlerContext::start_stream`] で応答をストリーミングにする（その後の戻り値は
/// 使わない）。ストリーミング受信のルートでは要求本文を [`HandlerContext::read_body`] で順に読める。
/// 応答が始まった時点で（`f` が戻るのを待たずに）返るので、本文の送信と受信が同時に進む。
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

/// 次の出来事を待ちながら、要求本文（gRPC は受信メッセージ）の中継も進める。
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

    /// ランタイム（通知 fd のドライバ）の上で `call_blocking` を呼び、応答を最後まで読む。ドライバが無いと
    /// 待ちがポーリングになり、要求本文の中継が進まない。io_uring・epoll が使えない環境ではスキップ（None）。
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
            use futures::SinkExt;
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
