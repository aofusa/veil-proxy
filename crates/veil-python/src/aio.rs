// F-263: Python の常駐の asyncio イベントループ。設計は docs/backlog/features/F-263-python-async-runtime-websocket.md。
//! Persistent asyncio event loops for `async def` handlers and WebSocket sessions.
//!
//! The network workers never take the GIL. A worker pushes a job onto the bounded queue of a loop and, when the
//! loop is not already signalled, writes one byte to the loop's socket pair (`loop.add_reader` wakes the loop,
//! which takes the jobs with `aio_take`). Results come back through a per-call queue and the worker's
//! notification fd ([`WakeFlag`]); no waker is called across threads.

use crate::native::{self, NativeRequest};
use crate::PyRoute;
use bytes::Bytes;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::pybacked::PyBackedStr;
use pyo3::types::{PyBytes, PyString, PyTuple};
use std::collections::VecDeque;
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use veil::api::ws::{
    CloseFrame, Message, Utf8Bytes, WebSocket, WebSocketUpgrade, WsError, WsSender,
};
use veil::api::{BodyChunk, FromRequest, Request, Response};
use veil::runtime::offload::WakeFlag;

// ジョブの種類（python/veil/_aio.py の JOB_* と同じ値）
const JOB_CALL: u8 = 0;
const JOB_CANCEL: u8 = 1;
const JOB_WS: u8 = 2;
const JOB_WAKE: u8 = 3;

// 1 回の呼び出しの応答の送り返し（ループ → ワーカー）で溜められる断片の数。
const OUT_CAP: usize = 16;
// WebSocket の受信（ワーカー → ループ）で溜められるメッセージの数（超えたら読み取りを止める）。
const WS_IN_CAP: usize = 256;

enum Job {
    Call {
        id: u64,
        handler_id: u64,
        route: Arc<PyRoute>,
        req: Request,
        out: Arc<Out>,
    },
    Cancel(u64),
    Ws {
        id: u64,
        handler_id: u64,
        req: Request,
        ws: Arc<WsShared>,
    },
    Wake(u64),
}

// ===================== ループの投入キュー =====================

// ループのソケットの組の書き込み側（Python が作り、`aio_setup` で受け取る。所有権は Python 側）。
#[cfg(unix)]
type RawSock = std::os::fd::RawFd;
#[cfg(windows)]
type RawSock = std::os::windows::io::RawSocket;

struct LoopQueue {
    q: Mutex<VecDeque<Job>>,
    cap: usize,
    // ループに合図済み（ループが `aio_take` で下ろす）。合図の書き込みをまとめる。
    armed: AtomicBool,
    wsock: RawSock,
}

impl LoopQueue {
    // 積む（`force` でなければ満杯のとき `false`）。
    fn push(&self, job: Job, force: bool) -> bool {
        {
            let mut q = self.q.lock().unwrap();
            if !force && q.len() >= self.cap {
                return false;
            }
            q.push_back(job);
        }
        if !self.armed.swap(true, Ordering::AcqRel) {
            self.signal();
        }
        true
    }

    // ソケットに 1 バイト書いてループを起こす（ノンブロッキング。満杯なら既に起きる予定）。
    fn signal(&self) {
        #[cfg(unix)]
        {
            use std::os::fd::FromRawFd;
            // SAFETY: wsock は Python の socketpair の書き込み側で、ループが止まるまで閉じない（`aio_setup` の契約）。
            // ManuallyDrop で包むので閉じない。
            let s = std::mem::ManuallyDrop::new(unsafe {
                std::os::unix::net::UnixStream::from_raw_fd(self.wsock)
            });
            let _ = (&*s).write(b"x");
        }
        #[cfg(windows)]
        {
            use std::os::windows::io::FromRawSocket;
            // SAFETY: 上と同じ（Windows の socketpair は TCP のソケットの組）。
            let s = std::mem::ManuallyDrop::new(unsafe {
                std::net::TcpStream::from_raw_socket(self.wsock)
            });
            let _ = (&*s).write(b"x");
        }
    }
}

static LOOPS: OnceLock<Vec<LoopQueue>> = OnceLock::new();
static NEXT_LOOP: AtomicUsize = AtomicUsize::new(0);
static NEXT_ID: AtomicU64 = AtomicU64::new(1);

fn loops() -> Option<&'static [LoopQueue]> {
    LOOPS.get().map(|v| v.as_slice())
}

// 要求ごとにループを選ぶ（順番に。同じ WebSocket の起床は最初に選んだループへ）。
fn pick() -> Option<(usize, &'static LoopQueue)> {
    let ls = loops()?;
    let i = NEXT_LOOP.fetch_add(1, Ordering::Relaxed) % ls.len();
    Some((i, &ls[i]))
}

/// Register the event loops: `fds` are the write ends of their socket pairs, `queue` the length of each job queue.
#[pyfunction]
pub fn aio_setup(fds: Vec<i64>, queue: usize) -> PyResult<()> {
    let v: Vec<LoopQueue> = fds
        .into_iter()
        .map(|fd| LoopQueue {
            q: Mutex::new(VecDeque::new()),
            cap: queue.max(1),
            armed: AtomicBool::new(false),
            wsock: fd as RawSock,
        })
        .collect();
    if v.is_empty() {
        return Err(PyRuntimeError::new_err("no event loop"));
    }
    LOOPS
        .set(v)
        .map_err(|_| PyRuntimeError::new_err("the event loops are already set up"))
}

/// Take the jobs queued for loop `index` (called by the loop with the GIL held).
#[pyfunction]
pub fn aio_take(py: Python<'_>, index: usize) -> PyResult<Vec<Py<AioJob>>> {
    let Some(lq) = loops().and_then(|l| l.get(index)) else {
        return Ok(Vec::new());
    };
    // 先に合図を下ろす（この後に積まれた分は改めて合図される）
    lq.armed.store(false, Ordering::Release);
    let jobs: Vec<Job> = lq.q.lock().unwrap().drain(..).collect();
    let mut out = Vec::with_capacity(jobs.len());
    for j in jobs {
        let job = match j {
            Job::Call {
                id,
                handler_id,
                route,
                req,
                out,
            } => AioJob {
                kind: JOB_CALL,
                id,
                handler_id,
                route: Some(route),
                request: Some(Py::new(py, NativeRequest::new(req))?),
                out: Some(out),
                ws: None,
            },
            Job::Cancel(id) => AioJob::bare(JOB_CANCEL, id),
            Job::Wake(id) => AioJob::bare(JOB_WAKE, id),
            Job::Ws {
                id,
                handler_id,
                req,
                ws,
            } => AioJob {
                kind: JOB_WS,
                id,
                handler_id,
                route: None,
                request: Some(Py::new(py, NativeRequest::new(req))?),
                out: None,
                ws: Some(Py::new(py, NativeWebSocket { s: ws })?),
            },
        };
        out.push(Py::new(py, job)?);
    }
    Ok(out)
}

// ===================== 応答の送り返し（ループ → ワーカー） =====================

enum Ev {
    Full(Response),
    Head(Response),
    Chunk(Bytes),
    Trailers(Vec<(Bytes, Bytes)>),
    End,
}

// 1 回の呼び出しの応答のキュー（ループが積み、ワーカーが `WakeFlag` で起きて取る）。
struct Out {
    q: Mutex<VecDeque<Ev>>,
    wake: WakeFlag,
    // ループ側が終わった（応答を全部積んだ・ジョブが捨てられた）。
    tx_closed: AtomicBool,
    // ワーカー側が終わった（クライアントが切断した等）。
    rx_closed: AtomicBool,
}

impl Out {
    fn new() -> Out {
        Out {
            q: Mutex::new(VecDeque::new()),
            wake: WakeFlag::new(),
            tx_closed: AtomicBool::new(false),
            rx_closed: AtomicBool::new(false),
        }
    }

    fn send(&self, ev: Ev) {
        self.q.lock().unwrap().push_back(ev);
        self.wake.notify();
    }

    fn close_tx(&self) {
        if !self.tx_closed.swap(true, Ordering::AcqRel) {
            self.wake.notify();
        }
    }

    async fn recv(&self) -> Option<Ev> {
        loop {
            self.wake.reset();
            if let Some(ev) = self.q.lock().unwrap().pop_front() {
                return Some(ev);
            }
            if self.tx_closed.load(Ordering::Acquire) {
                return None;
            }
            self.wake.wait().await;
        }
    }
}

// ワーカー側の後始末: 応答が終わる前に捨てられたら（切断）ループのタスクを cancel する。
struct CallGuard {
    lq: &'static LoopQueue,
    id: u64,
    out: Arc<Out>,
    done: bool,
}

impl Drop for CallGuard {
    fn drop(&mut self) {
        self.out.rx_closed.store(true, Ordering::Release);
        if !self.done && !self.out.tx_closed.load(Ordering::Acquire) {
            self.lq.push(Job::Cancel(self.id), true);
        }
    }
}

fn overloaded() -> Response {
    Response::error(503, "Service Unavailable").with_header("retry-after", "1")
}

/// Run an `async def` route on a persistent event loop and wait for its response (on the network worker).
pub async fn call_on_loop(route: Arc<PyRoute>, handler_id: u64, req: Request) -> Response {
    let Some((_, lq)) = pick() else {
        return Response::error(500, "Internal Server Error");
    };
    let out = Arc::new(Out::new());
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let job = Job::Call {
        id,
        handler_id,
        route,
        req,
        out: out.clone(),
    };
    if !lq.push(job, false) {
        return overloaded();
    }
    let mut guard = CallGuard {
        lq,
        id,
        out: out.clone(),
        done: false,
    };
    match out.recv().await {
        Some(Ev::Full(r)) => {
            guard.done = true;
            r
        }
        Some(Ev::Head(head)) => {
            let status = head.status_code();
            let body = futures::stream::unfold(Some(guard), |g| async move {
                let mut g = g?;
                loop {
                    match g.out.recv().await {
                        Some(Ev::Chunk(b)) => return Some((BodyChunk::Data(b), Some(g))),
                        Some(Ev::Trailers(t)) => {
                            g.done = true;
                            return Some((BodyChunk::Trailers(t), None));
                        }
                        Some(Ev::End) => {
                            g.done = true;
                            return None;
                        }
                        Some(_) => continue,
                        None => {
                            g.done = true;
                            return Some((
                                BodyChunk::Abort("the handler failed while streaming".into()),
                                None,
                            ));
                        }
                    }
                }
            });
            let mut r = Response::stream(status, body);
            for (n, v) in head.headers() {
                r = r.with_header(n.clone(), v.clone());
            }
            r
        }
        _ => {
            guard.done = true;
            Response::error(500, "Internal Server Error")
        }
    }
}

/// One job handed to a loop.
#[pyclass(name = "AioJob", module = "veil._veil")]
pub struct AioJob {
    #[pyo3(get)]
    kind: u8,
    #[pyo3(get)]
    id: u64,
    #[pyo3(get)]
    handler_id: u64,
    route: Option<Arc<PyRoute>>,
    request: Option<Py<NativeRequest>>,
    out: Option<Arc<Out>>,
    ws: Option<Py<NativeWebSocket>>,
}

impl AioJob {
    fn bare(kind: u8, id: u64) -> AioJob {
        AioJob {
            kind,
            id,
            handler_id: 0,
            route: None,
            request: None,
            out: None,
            ws: None,
        }
    }

    fn finish_with(&self, ev: Ev) {
        if let Some(o) = &self.out {
            if !o.tx_closed.load(Ordering::Acquire) {
                o.send(ev);
                o.close_tx();
            }
        }
    }
}

impl Drop for AioJob {
    fn drop(&mut self) {
        // 応答を積まずに捨てられた（ワーカーは 500 にする）
        if let Some(o) = &self.out {
            o.close_tx();
        }
    }
}

#[pymethods]
impl AioJob {
    /// The request (`veil.Request`-compatible).
    #[getter]
    fn request(&self, py: Python<'_>) -> Option<Py<NativeRequest>> {
        self.request.as_ref().map(|r| r.clone_ref(py))
    }

    /// The WebSocket of a `JOB_WS` job.
    #[getter]
    fn websocket(&self, py: Python<'_>) -> Option<Py<NativeWebSocket>> {
        self.ws.as_ref().map(|w| w.clone_ref(py))
    }

    /// Hand the handler's return value to the worker (converted like a pool handler's).
    fn complete(&self, py: Python<'_>, result: Bound<'_, PyAny>) {
        let Some(route) = &self.route else { return };
        let resp = match native::to_response(py, result, route.status, &route.fallback, None) {
            Ok(r) => r,
            Err(e) => {
                e.print(py);
                crate::internal_error()
            }
        };
        self.finish_with(Ev::Full(resp));
    }

    /// The handler raised (500).
    fn fail(&self) {
        self.finish_with(Ev::Full(crate::internal_error()));
    }

    /// The task was cancelled (the client left).
    fn cancelled(&self) {
        if let Some(o) = &self.out {
            o.close_tx();
        }
    }

    /// Start a streaming response (`veil.StreamingResponse`): sends the status and headers.
    fn start_stream(&self, sr: Bound<'_, PyAny>) -> PyResult<AioStream> {
        let Some(o) = &self.out else {
            return Err(PyRuntimeError::new_err("not a call job"));
        };
        let head = native::response_object_head(&sr)?;
        o.send(Ev::Head(head));
        Ok(AioStream { out: o.clone() })
    }
}

/// The body of a streaming response sent from the loop.
#[pyclass(name = "AioStream", module = "veil._veil")]
pub struct AioStream {
    out: Arc<Out>,
}

#[pymethods]
impl AioStream {
    /// Queue a chunk (`bytes` or `str`); `False` when the queue is full (try again later) or the client left.
    fn try_write(&self, item: Bound<'_, PyAny>) -> PyResult<bool> {
        if self.out.rx_closed.load(Ordering::Acquire) {
            return Ok(false);
        }
        let b = native::to_chunk(item)?;
        {
            let mut q = self.out.q.lock().unwrap();
            if q.len() >= OUT_CAP {
                return Ok(false);
            }
            q.push_back(Ev::Chunk(b));
        }
        self.out.wake.notify();
        Ok(true)
    }

    /// Whether the client has left.
    #[getter]
    fn closed(&self) -> bool {
        self.out.rx_closed.load(Ordering::Acquire)
    }

    /// End the body (with optional `(name, value)` trailers).
    #[pyo3(signature = (trailers=None))]
    fn finish(&self, trailers: Option<Bound<'_, PyAny>>) -> PyResult<()> {
        let mut t = Vec::new();
        if let Some(tr) = trailers.filter(|t| !t.is_none()) {
            for item in tr.try_iter()? {
                let item = item?;
                let tup = item.cast::<PyTuple>()?;
                let n: PyBackedStr = tup.get_item(0)?.extract()?;
                let v: PyBackedStr = tup.get_item(1)?.extract()?;
                t.push((
                    Bytes::copy_from_slice(n.to_ascii_lowercase().as_bytes()),
                    Bytes::copy_from_slice(v.as_bytes()),
                ));
            }
        }
        self.out.send(if t.is_empty() {
            Ev::End
        } else {
            Ev::Trailers(t)
        });
        self.out.close_tx();
        Ok(())
    }
}

// ===================== WebSocket（ループで動く `@app.websocket`） =====================

enum Ctl {
    Accept(Option<String>, Vec<(String, String)>),
    Reject(u16),
}

enum Inbound {
    Msg(Message),
    Close(u16),
}

#[derive(Default)]
struct SendState {
    tx: Option<WsSender>,
    // `accept` の後、接続が始まる前に送られた分
    pending: Vec<Message>,
    closed: bool,
}

// ワーカーとループが共有する 1 本の WebSocket の状態。
struct WsShared {
    id: u64,
    lq: &'static LoopQueue,
    ctl: Mutex<Option<Ctl>>,
    ctl_wake: WakeFlag,
    inbound: Mutex<VecDeque<Inbound>>,
    // ループへ起床を積み済み（ループの `take` が下ろす）
    wake_scheduled: AtomicBool,
    // 受信の空きができた（読み取りを再開する）
    space: WakeFlag,
    send: Mutex<SendState>,
    // Python のハンドラが終わった
    py_done: AtomicBool,
    // Python に終わり（Close）を渡した
    in_closed: AtomicBool,
}

impl WsShared {
    fn push_in(&self, m: Inbound) {
        if self.in_closed.load(Ordering::Acquire) {
            return;
        }
        if matches!(m, Inbound::Close(_)) {
            self.in_closed.store(true, Ordering::Release);
        }
        if self.py_done.load(Ordering::Acquire) {
            return;
        }
        self.inbound.lock().unwrap().push_back(m);
        if !self.wake_scheduled.swap(true, Ordering::AcqRel) {
            self.lq.push(Job::Wake(self.id), true);
        }
    }

    fn send(&self, m: Message) -> u8 {
        let mut s = self.send.lock().unwrap();
        if s.closed {
            return 2;
        }
        if matches!(m, Message::Close(_)) {
            s.closed = true;
        }
        match &s.tx {
            Some(tx) => match tx.send(m) {
                Ok(()) => 0,
                Err(WsError::Full) => 1,
                Err(_) => 2,
            },
            None => {
                s.pending.push(m);
                0
            }
        }
    }

    fn decide(&self, c: Ctl) -> bool {
        let mut g = self.ctl.lock().unwrap();
        if g.is_some() {
            return false;
        }
        *g = Some(c);
        drop(g);
        self.ctl_wake.notify();
        true
    }
}

// 接続が始まらずに捨てられた・終わったら、Python に Close（1006）を渡す。
struct SessionGuard(Arc<WsShared>);

impl Drop for SessionGuard {
    fn drop(&mut self) {
        self.0.push_in(Inbound::Close(1006));
        self.0.send.lock().unwrap().closed = true;
    }
}

/// Serve an `@app.websocket` route whose handler runs on a persistent event loop (on the network worker).
pub async fn websocket_on_loop(handler_id: u64, req: Request) -> Response {
    let up = match WebSocketUpgrade::from_request(&req) {
        Ok(u) => u,
        Err(r) => return r,
    };
    let Some((_, lq)) = pick() else {
        return Response::error(500, "Internal Server Error");
    };
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let shared = Arc::new(WsShared {
        id,
        lq,
        ctl: Mutex::new(None),
        ctl_wake: WakeFlag::new(),
        inbound: Mutex::new(VecDeque::new()),
        wake_scheduled: AtomicBool::new(false),
        space: WakeFlag::new(),
        send: Mutex::new(SendState::default()),
        py_done: AtomicBool::new(false),
        in_closed: AtomicBool::new(false),
    });
    let job = Job::Ws {
        id,
        handler_id,
        req,
        ws: shared.clone(),
    };
    if !lq.push(job, false) {
        return overloaded();
    }
    // 承認（accept）を待つ間に切断されたら、ガードの drop で Python に Close を渡す
    let guard = SessionGuard(shared.clone());
    let ctl = loop {
        shared.ctl_wake.reset();
        if let Some(c) = shared.ctl.lock().unwrap().take() {
            break c;
        }
        shared.ctl_wake.wait().await;
    };
    match ctl {
        Ctl::Reject(status) => {
            let msg = if status == 403 {
                "Forbidden"
            } else if status == 422 {
                "Unprocessable Entity"
            } else {
                "Internal Server Error"
            };
            Response::error(status, msg)
        }
        Ctl::Accept(sub, headers) => {
            let up = match sub {
                Some(p) => up.protocols([p]),
                None => up,
            };
            let mut resp = up.on_upgrade(move |ws: WebSocket| ws_session(ws, guard));
            for (n, v) in headers {
                resp = resp.with_header(n.to_ascii_lowercase(), v);
            }
            resp
        }
    }
}

// 接続の一生（ワーカーで）: 受けたメッセージを Python へ渡し、Python の送信は `WsSender` のキューから書かれる。
async fn ws_session(ws: WebSocket, guard: SessionGuard) {
    let shared = guard.0.clone();
    let (tx, mut rx) = ws.split();
    {
        let mut s = shared.send.lock().unwrap();
        for m in s.pending.drain(..) {
            let _ = tx.send(m);
        }
        s.tx = Some(tx);
    }
    loop {
        // 受信の背圧: Python が受け取るまで読み取りを止める（その間も送信は書き出す）
        loop {
            shared.space.reset();
            if shared.py_done.load(Ordering::Acquire)
                || shared.inbound.lock().unwrap().len() < WS_IN_CAP
            {
                break;
            }
            if veil::runtime::time::timeout(
                std::time::Duration::from_millis(5),
                shared.space.wait(),
            )
            .await
            .is_err()
                && rx.flush().await.is_err()
            {
                break;
            }
        }
        match rx.recv().await {
            Some(Ok(m @ (Message::Text(_) | Message::Binary(_)))) => {
                shared.push_in(Inbound::Msg(m))
            }
            Some(Ok(Message::Close(c))) => {
                shared.push_in(Inbound::Close(c.map(|c| c.code).unwrap_or(1005)));
                break;
            }
            Some(Ok(_)) => {}
            Some(Err(WsError::Protocol(c))) => {
                shared.push_in(Inbound::Close(c));
                break;
            }
            Some(Err(_)) | None => {
                shared.push_in(Inbound::Close(1006));
                break;
            }
        }
    }
    drop(guard);
}

fn message_from(py: Python<'_>, opcode: u8, data: &Bound<'_, PyAny>) -> PyResult<Message> {
    let _ = py;
    if opcode == 1 {
        let s: String = data.extract()?;
        Ok(Message::Text(Utf8Bytes::from(s)))
    } else {
        let b: Vec<u8> = data.extract()?;
        Ok(Message::Binary(Bytes::from(b)))
    }
}

fn close_frame(code: u16, reason: &str) -> Message {
    Message::Close(Some(CloseFrame {
        code,
        reason: Utf8Bytes::from(reason.to_string()),
    }))
}

/// The native side of a `veil.WebSocket` (used on the event loop).
#[pyclass(name = "NativeWebSocket", module = "veil._veil")]
pub struct NativeWebSocket {
    s: Arc<WsShared>,
}

#[pymethods]
impl NativeWebSocket {
    /// Accept the handshake (101).
    #[pyo3(signature = (subprotocol=None, headers=Vec::new()))]
    fn accept(&self, subprotocol: Option<String>, headers: Vec<(String, String)>) -> PyResult<()> {
        if !self.s.decide(Ctl::Accept(subprotocol, headers)) {
            return Err(PyRuntimeError::new_err("the handshake is already answered"));
        }
        Ok(())
    }

    /// Reject the handshake with an HTTP status (before `accept`).
    fn reject(&self, status: u16) {
        self.s.decide(Ctl::Reject(status));
    }

    /// The next received message without waiting: `("text", str)`, `("bytes", bytes)`, `("close", code)` or `None`.
    fn take(&self, py: Python<'_>) -> Option<(&'static str, Py<PyAny>)> {
        self.s.wake_scheduled.store(false, Ordering::Release);
        let (m, was_full) = {
            let mut q = self.s.inbound.lock().unwrap();
            let full = q.len() >= WS_IN_CAP;
            (q.pop_front()?, full)
        };
        if was_full {
            self.s.space.notify();
        }
        Some(match m {
            Inbound::Msg(Message::Text(t)) => {
                ("text", PyString::new(py, t.as_str()).into_any().unbind())
            }
            Inbound::Msg(Message::Binary(b)) => ("bytes", PyBytes::new(py, &b).into_any().unbind()),
            Inbound::Msg(_) => return None,
            Inbound::Close(code) => ("close", code.into_pyobject(py).ok()?.into_any().unbind()),
        })
    }

    /// Queue a message (`opcode` 1 = text, 2 = binary): 0 = queued, 1 = queue full, 2 = closed.
    fn send(&self, py: Python<'_>, opcode: u8, data: Bound<'_, PyAny>) -> PyResult<u8> {
        Ok(self.s.send(message_from(py, opcode, &data)?))
    }

    /// Start the closing handshake.
    fn close(&self, code: u16, reason: &str) {
        self.s.send(close_frame(code, reason));
    }

    /// The handler returned: close normally (1000) unless already closed.
    fn finish(&self) {
        self.s.decide(Ctl::Reject(403));
        self.s.send(close_frame(1000, ""));
        self.s.py_done.store(true, Ordering::Release);
        self.s.inbound.lock().unwrap().clear();
        self.s.space.notify();
    }
}

// ===================== WebSocketEndpoint（ハンドラ用プールで動くクラス） =====================

/// The sender handed to `WebSocketEndpoint` callbacks (sending never waits).
#[pyclass(name = "NativeWsSender", module = "veil._veil")]
pub struct NativeWsSender {
    tx: WsSender,
}

#[pymethods]
impl NativeWsSender {
    /// Queue a message (`opcode` 1 = text, 2 = binary): 0 = queued, 1 = queue full, 2 = closed.
    fn send(&self, py: Python<'_>, opcode: u8, data: Bound<'_, PyAny>) -> PyResult<u8> {
        Ok(match self.tx.send(message_from(py, opcode, &data)?) {
            Ok(()) => 0,
            Err(WsError::Full) => 1,
            Err(_) => 2,
        })
    }

    /// Start the closing handshake.
    fn close(&self, code: u16, reason: &str) {
        let _ = self.tx.send(close_frame(code, reason));
    }
}

// `WebSocketEndpoint` の 1 接続（コールバックはハンドラ用プールで 1 つずつ順に）。
struct EndpointSession {
    session: Py<PyAny>,
    tx: WsSender,
}

impl veil::api::ws::WsSession for EndpointSession {
    fn on_message(&mut self, msg: Message) {
        Python::attach(|py| {
            let data = match &msg {
                Message::Text(t) => PyString::new(py, t.as_str()).into_any(),
                Message::Binary(b) => PyBytes::new(py, b).into_any(),
                _ => return,
            };
            if let Err(e) = self.session.call_method1(py, "on_message", (data,)) {
                e.print(py);
                let _ = self.tx.send(close_frame(1011, ""));
            }
        })
    }

    fn on_close(&mut self, code: u16) {
        Python::attach(|py| {
            if let Err(e) = self.session.call_method1(py, "on_close", (code,)) {
                e.print(py);
            }
        })
    }
}

/// Serve an `@app.websocket` route whose handler is a `WebSocketEndpoint` class (callbacks on the handler pool).
pub fn websocket_endpoint(route: Arc<Py<PyAny>>, req: Request) -> Response {
    let up = match WebSocketUpgrade::from_request(&req) {
        Ok(u) => u,
        Err(r) => return r,
    };
    up.on_upgrade_blocking(move |tx: WsSender| {
        Python::attach(|py| {
            let r = (|| -> PyResult<EndpointSession> {
                let sender = Py::new(py, NativeWsSender { tx: tx.clone() })?;
                let nreq = Py::new(py, NativeRequest::new(req))?;
                let session = route.call_method1(py, "_endpoint_session", (sender, nreq))?;
                session.call_method0(py, "on_connect")?;
                Ok(EndpointSession { session, tx })
            })();
            match r {
                Ok(s) => Some(Box::new(s) as Box<dyn veil::api::ws::WsSession>),
                Err(e) => {
                    e.print(py);
                    None
                }
            }
        })
    })
}
