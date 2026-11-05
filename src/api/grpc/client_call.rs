// F-291: ランタイムを持たないスレッドからの gRPC の呼び出し（C の veil_grpc_channel_*・Python の veil_kit.grpc）。
// veil-python の grpc_client.rs（F-268・B-203）の仕組みをここへ移して共有する。呼び出しは専用スレッド
// （api::client_thread）で双方向ストリームの形（生のバイト列のコーデック）で動かすので、4 種類のメソッドが 1 本の
// 経路を通る（直列化は呼び出し側）。応答は Mutex + Condvar で同期に待つか、ハンドラ用プールで呼ぶ閉包で受ける。
//! gRPC calls made from threads without a runtime (C's `veil_grpc_channel_*`, Python's `veil_kit.grpc`).
//!
//! Every call runs on the client thread ([`crate::api::client_thread`]) as a bidirectional stream with the
//! raw-bytes codec, so the four kinds of methods share one path; messages are encoded by the caller. Responses
//! are received by waiting ([`BlockingCall::recv`], [`BlockingCall::status`]) or through functions called on the
//! handler pool ([`BlockingCall::recv_then`], [`BlockingCall::status_then`]).

use super::{BytesCodec, Channel, Request, Status};
use crate::runtime::offload::WakeFlag;
use bytes::Bytes;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

/// The final state of a call.
#[derive(Debug, Clone)]
pub struct CallStatus {
    /// The gRPC status code (0 = OK).
    pub code: u8,
    /// The status message.
    pub message: String,
    /// The response headers.
    pub initial: Vec<(Bytes, Bytes)>,
    /// The trailers.
    pub trailers: Vec<(Bytes, Bytes)>,
}

impl CallStatus {
    fn from_status(st: &Status, initial: Vec<(Bytes, Bytes)>) -> CallStatus {
        CallStatus {
            code: st.code() as u8,
            message: st.message().to_string(),
            initial,
            trailers: st.metadata().to_vec(),
        }
    }
}

type RecvFn = Box<dyn FnOnce(Option<Bytes>) + Send>;
type StatusFn = Box<dyn FnOnce(CallStatus) + Send>;

#[derive(Default)]
struct RespState {
    msgs: VecDeque<Bytes>,
    initial: Vec<(Bytes, Bytes)>,
    done: Option<CallStatus>,
    // 非同期の待ち手（次のメッセージ・最終の状態）
    recv_cb: Option<RecvFn>,
    status_cb: Option<StatusFn>,
}

struct CallShared {
    // 要求のメッセージ（None = 送り終わり）
    reqs: Mutex<VecDeque<Option<Bytes>>>,
    // クライアントのスレッドで作る旗（要求の起床・取消しの起床）。作る前は None
    flag: Mutex<Option<(Arc<WakeFlag>, Arc<WakeFlag>)>>,
    cancelled: AtomicBool,
    // B-203: 応答側（UpStream）を捨てた後。取消しでは、これが立つまで要求のストリームを終えない
    // （先に終えると END_STREAM ＝正常な半クローズになり、途中までの入力を完全な要求として処理される）
    resp_gone: AtomicBool,
    sending_done: AtomicBool,
    resp: Mutex<RespState>,
    cond: Condvar,
}

impl CallShared {
    fn wake_task(&self) {
        let f = self.flag.lock().unwrap().clone();
        if let Some((reqs, cancel)) = f {
            reqs.notify();
            cancel.notify();
        }
    }

    fn push_msg(&self, b: Bytes) {
        let cb = {
            let mut st = self.resp.lock().unwrap();
            match st.recv_cb.take() {
                Some(cb) => Some((cb, b)),
                None => {
                    st.msgs.push_back(b);
                    None
                }
            }
        };
        self.cond.notify_all();
        if let Some((cb, b)) = cb {
            crate::runtime::offload::spawn_handler(move || cb(Some(b)));
        }
    }

    fn finish(&self, f: CallStatus) {
        let (rcb, scb) = {
            let mut st = self.resp.lock().unwrap();
            if st.done.is_some() {
                return;
            }
            st.done = Some(f.clone());
            (st.recv_cb.take(), st.status_cb.take())
        };
        self.cond.notify_all();
        if let Some(cb) = rcb {
            crate::runtime::offload::spawn_handler(move || cb(None));
        }
        if let Some(cb) = scb {
            crate::runtime::offload::spawn_handler(move || cb(f));
        }
    }
}

/// One call started by [`BlockingCall::start`]. Dropping it before the call finished cancels the call.
pub struct BlockingCall {
    shared: Arc<CallShared>,
}

impl BlockingCall {
    /// Starts a call of `path` (`/package.Service/Method`) on `channel` with `metadata` (lower-case names) and an
    /// optional deadline. Request messages are sent with [`BlockingCall::send`].
    pub fn start(
        channel: &Channel,
        path: &str,
        metadata: Vec<(Bytes, Bytes)>,
        timeout: Option<Duration>,
    ) -> Result<BlockingCall, String> {
        let shared = Arc::new(CallShared {
            reqs: Mutex::new(VecDeque::new()),
            flag: Mutex::new(None),
            cancelled: AtomicBool::new(false),
            resp_gone: AtomicBool::new(false),
            sending_done: AtomicBool::new(false),
            resp: Mutex::new(RespState::default()),
            cond: Condvar::new(),
        });
        let (channel, path, call) = (channel.clone(), path.to_string(), Arc::clone(&shared));
        crate::api::client_thread::spawn(move || run_call(channel, path, metadata, timeout, call))?;
        Ok(BlockingCall { shared })
    }

    /// Sends one request message; `false` once the call has finished or sending was closed.
    pub fn send(&self, message: Bytes) -> bool {
        let s = &self.shared;
        if s.sending_done.load(Ordering::Acquire) || s.resp.lock().unwrap().done.is_some() {
            return false;
        }
        s.reqs.lock().unwrap().push_back(Some(message));
        s.wake_task();
        true
    }

    /// Finishes sending (half-close).
    pub fn done_sending(&self) {
        let s = &self.shared;
        if !s.sending_done.swap(true, Ordering::AcqRel) {
            s.reqs.lock().unwrap().push_back(None);
            s.wake_task();
        }
    }

    /// The next response message, or `None` at the end; waits.
    pub fn recv(&self) -> Option<Bytes> {
        let s = &self.shared;
        let mut st = s.resp.lock().unwrap();
        loop {
            if let Some(b) = st.msgs.pop_front() {
                return Some(b);
            }
            if st.done.is_some() {
                return None;
            }
            st = s.cond.wait(st).unwrap();
        }
    }

    /// Calls `f` with the next response message (`None` at the end) on the handler pool, or right away on this
    /// thread when one is already there.
    pub fn recv_then(&self, f: impl FnOnce(Option<Bytes>) + Send + 'static) {
        let mut st = self.shared.resp.lock().unwrap();
        if let Some(b) = st.msgs.pop_front() {
            drop(st);
            f(Some(b));
        } else if st.done.is_some() {
            drop(st);
            f(None);
        } else {
            st.recv_cb = Some(Box::new(f));
        }
    }

    /// The final state; waits for the end of the call.
    pub fn status(&self) -> CallStatus {
        let s = &self.shared;
        let mut st = s.resp.lock().unwrap();
        loop {
            if let Some(f) = &st.done {
                return f.clone();
            }
            st = s.cond.wait(st).unwrap();
        }
    }

    /// The final state when the call has ended (without waiting).
    pub fn try_status(&self) -> Option<CallStatus> {
        self.shared.resp.lock().unwrap().done.clone()
    }

    /// Calls `f` with the final state at the end of the call (on the handler pool, or right away on this thread
    /// when the call already ended).
    pub fn status_then(&self, f: impl FnOnce(CallStatus) + Send + 'static) {
        let mut st = self.shared.resp.lock().unwrap();
        if let Some(done) = st.done.clone() {
            drop(st);
            f(done);
        } else {
            st.status_cb = Some(Box::new(f));
        }
    }

    /// The response headers (empty until they arrived).
    pub fn initial_metadata(&self) -> Vec<(Bytes, Bytes)> {
        self.shared.resp.lock().unwrap().initial.clone()
    }

    /// Cancels the call (the status becomes CANCELLED unless it already finished).
    pub fn cancel(&self) {
        self.shared.cancelled.store(true, Ordering::Release);
        self.shared.wake_task();
    }
}

// 終わる前に捨てられたら取り消す（応答側を捨てて相手に RST_STREAM(CANCEL) を送り、task を終わらせる。
// 接続・応答ヘッダの待ちの間でも効く。B-203）
impl Drop for BlockingCall {
    fn drop(&mut self) {
        if self.shared.resp.lock().unwrap().done.is_none() {
            self.cancel();
        }
    }
}

// B-203: 呼び出し全体（接続・応答ヘッダの待ちを含む）を取消しと競わせる。取り消したら応答側を捨てて
// RST_STREAM(CANCEL) を送らせ、その後で要求のストリームを終える（END_STREAM を先に送らない）。
async fn run_call(
    channel: Channel,
    path: String,
    metadata: Vec<(Bytes, Bytes)>,
    timeout: Option<Duration>,
    call: Arc<CallShared>,
) {
    // 待ち手ごとに旗を分ける（1 つの旗を 2 か所で reset すると起床を取りこぼす）
    let flag = Arc::new(WakeFlag::new());
    let cancel_flag = Arc::new(WakeFlag::new());
    *call.flag.lock().unwrap() = Some((Arc::clone(&flag), Arc::clone(&cancel_flag)));
    let outcome = {
        let work = call_body(channel, path, metadata, timeout, &call, &flag);
        futures::pin_mut!(work);
        let cancel = async {
            loop {
                cancel_flag.reset();
                if call.cancelled.load(Ordering::Acquire) {
                    return;
                }
                cancel_flag.wait().await;
            }
        };
        futures::pin_mut!(cancel);
        match futures::future::select(work, cancel).await {
            futures::future::Either::Left((f, _)) => Some(f),
            futures::future::Either::Right(_) => None,
        }
        // ここで `work`（応答のストリーム・UpStream）を捨てる → アクターが RST_STREAM(CANCEL) を送る
    };
    // 要求のストリームはここから終えてよい（取消しのときも、応答側を捨てた後）
    call.resp_gone.store(true, Ordering::Release);
    flag.notify();
    let f = outcome.unwrap_or_else(|| CallStatus {
        code: 1,
        message: "cancelled".into(),
        initial: call.resp.lock().unwrap().initial.clone(),
        trailers: Vec::new(),
    });
    call.finish(f);
}

/// The call itself: sends the request stream, delivers the responses and returns the final state.
async fn call_body(
    channel: Channel,
    path: String,
    metadata: Vec<(Bytes, Bytes)>,
    timeout: Option<Duration>,
    call: &Arc<CallShared>,
    flag: &Arc<WakeFlag>,
) -> CallStatus {
    // 要求のストリーム: キューから取り出す（送り終わり・呼び出しの終わりで終わる）
    let src = Arc::clone(call);
    let reqs = Box::pin(futures::stream::unfold(
        (src, Arc::clone(flag)),
        |(s, f)| async move {
            loop {
                f.reset();
                if s.resp_gone.load(Ordering::Acquire) {
                    return None;
                }
                if s.cancelled.load(Ordering::Acquire) {
                    // 取消し: 応答側を捨てる（RST_STREAM）まで終端を送らない
                    f.wait().await;
                    continue;
                }
                let item = s.reqs.lock().unwrap().pop_front();
                match item {
                    Some(Some(b)) => return Some((b, (s, f))),
                    Some(None) => return None,
                    None => f.wait().await,
                }
            }
        },
    ));
    let mut req = Request::new(reqs);
    for (n, v) in metadata {
        req = req.with_metadata(n, v);
    }
    if let Some(t) = timeout {
        req = req.with_timeout(t);
    }
    let resp = match channel
        .bidi::<BytesCodec, Bytes, Bytes, _>(&path, BytesCodec, req)
        .await
    {
        Ok(r) => r,
        Err(st) => return CallStatus::from_status(&st, Vec::new()),
    };
    let initial = resp.metadata().to_vec();
    call.resp.lock().unwrap().initial = initial.clone();
    let mut stream = resp.into_inner();
    loop {
        match stream.message().await {
            Ok(Some(b)) => call.push_msg(b),
            Ok(None) => {
                let trailers = stream.trailers().to_vec();
                return CallStatus {
                    code: 0,
                    message: String::new(),
                    initial,
                    trailers,
                };
            }
            Err(st) => return CallStatus::from_status(&st, initial),
        }
    }
}
