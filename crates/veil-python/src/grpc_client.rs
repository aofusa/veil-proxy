// F-268: Python の gRPC クライアント（veil_kit.grpc.Channel の土台）。
//! gRPC client calls for Python (`veil_kit.grpc`), on veil's own HTTP/2 client.
//!
//! Calls run on one client thread with veil's runtime (`runtime::block_on`), started on the first channel. Python
//! threads hand requests over through a queue and a [`WakeFlag`] (no `Waker` is called across threads); responses
//! come back through a mutex + condition variable (blocking `recv`, with the GIL released) or a callback that the
//! handler pool calls with the GIL (asyncio: `loop.call_soon_threadsafe`). Every call uses the bidirectional
//! primitive with the raw-bytes codec, so the four kinds of methods share one path (protobuf encoding stays in
//! Python).

use bytes::Bytes;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::Duration;
use veil::api::grpc::{BytesCodec, Channel, Compression, Request, Status};
use veil::runtime::offload::WakeFlag;

/// A command for the client thread.
enum Cmd {
    Start {
        channel: Channel,
        path: String,
        metadata: Vec<(Bytes, Bytes)>,
        timeout: Option<Duration>,
        call: Arc<CallShared>,
    },
}

struct ClientRt {
    queue: Mutex<VecDeque<Cmd>>,
    wake: Arc<WakeFlag>,
}

static RT: OnceLock<Result<ClientRt, String>> = OnceLock::new();

fn rt() -> PyResult<&'static ClientRt> {
    RT.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::channel::<Arc<WakeFlag>>();
        std::thread::Builder::new()
            .name("veil-grpc-client".into())
            .spawn(move || {
                veil::runtime::block_on(async move {
                    // 旗はこのスレッドで作る（このスレッドの通知 fd を覚える）
                    let wake = Arc::new(WakeFlag::new());
                    let _ = tx.send(Arc::clone(&wake));
                    let rt = loop {
                        if let Some(Ok(rt)) = RT.get() {
                            break rt;
                        }
                        veil::runtime::sleep(Duration::from_millis(1)).await;
                    };
                    loop {
                        wake.reset();
                        let cmds: Vec<Cmd> = rt.queue.lock().unwrap().drain(..).collect();
                        if cmds.is_empty() {
                            wake.wait().await;
                            continue;
                        }
                        for cmd in cmds {
                            match cmd {
                                Cmd::Start {
                                    channel,
                                    path,
                                    metadata,
                                    timeout,
                                    call,
                                } => {
                                    veil::runtime::spawn(run_call(
                                        channel, path, metadata, timeout, call,
                                    ));
                                }
                            }
                        }
                    }
                })
            })
            .map_err(|e| e.to_string())?;
        let wake = rx
            .recv_timeout(Duration::from_secs(10))
            .map_err(|_| "the gRPC client thread did not start".to_string())?;
        Ok(ClientRt {
            queue: Mutex::new(VecDeque::new()),
            wake,
        })
    })
    .as_ref()
    .map_err(|e| PyRuntimeError::new_err(e.clone()))
}

/// Start the client thread now (otherwise it starts with the first call).
#[pyfunction]
pub fn grpc_client_setup() -> PyResult<()> {
    rt().map(|_| ())
}

/// The final status of a call: code, message, response headers, trailers.
type Final = (u8, String, Vec<(Bytes, Bytes)>, Vec<(Bytes, Bytes)>);

#[derive(Default)]
struct RespState {
    msgs: VecDeque<Bytes>,
    initial: Vec<(Bytes, Bytes)>,
    done: Option<Final>,
    // 非同期の待ち手（次のメッセージ・最終の状態）
    recv_cb: Option<Py<PyAny>>,
    status_cb: Option<Py<PyAny>>,
}

struct CallShared {
    // 要求のメッセージ（None = 送り終わり）
    reqs: Mutex<VecDeque<Option<Bytes>>>,
    // クライアントのスレッドで作る旗（要求の起床・取消しの起床）。作る前は None
    flag: Mutex<Option<(Arc<WakeFlag>, Arc<WakeFlag>)>>,
    cancelled: std::sync::atomic::AtomicBool,
    sending_done: std::sync::atomic::AtomicBool,
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
            call_back(cb, Some(b));
        }
    }

    fn finish(&self, f: Final) {
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
            call_back(cb, None);
        }
        if let Some(cb) = scb {
            veil::runtime::offload::spawn_handler(move || {
                Python::attach(|py| {
                    let v = final_tuple(py, &f);
                    if let Err(e) = v.and_then(|v| cb.call1(py, (v,))) {
                        e.print(py);
                    }
                })
            });
        }
    }
}

/// Call a Python callback with a message (`bytes`) or `None`, on the handler pool (not on the client thread).
fn call_back(cb: Py<PyAny>, msg: Option<Bytes>) {
    veil::runtime::offload::spawn_handler(move || {
        Python::attach(|py| {
            let arg: Py<PyAny> = match msg {
                Some(b) => PyBytes::new(py, &b).into_any().unbind(),
                None => py.None(),
            };
            if let Err(e) = cb.call1(py, (arg,)) {
                e.print(py);
            }
        })
    });
}

fn pairs(py: Python<'_>, h: &[(Bytes, Bytes)]) -> Vec<(String, Py<PyAny>)> {
    h.iter()
        .map(|(n, v)| {
            let name = String::from_utf8_lossy(n).into_owned();
            // `-bin` の値はバイト列、他は文字列（grpcio と同じ）
            let val: Py<PyAny> = if name.ends_with("-bin") {
                PyBytes::new(py, v).into_any().unbind()
            } else {
                pyo3::types::PyString::new(py, &String::from_utf8_lossy(v))
                    .into_any()
                    .unbind()
            };
            (name, val)
        })
        .collect()
}

fn final_tuple(py: Python<'_>, f: &Final) -> PyResult<Py<PyAny>> {
    let t = (f.0, f.1.clone(), pairs(py, &f.2), pairs(py, &f.3));
    Ok(t.into_pyobject(py)?.into_any().unbind())
}

fn status_final(st: &Status, initial: Vec<(Bytes, Bytes)>) -> Final {
    (
        st.code() as u8,
        st.message().to_string(),
        initial,
        st.metadata().to_vec(),
    )
}

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
    // 要求のストリーム: キューから取り出す（送り終わり・取消しで終わる）
    let src = Arc::clone(&call);
    let reqs = Box::pin(futures::stream::unfold(
        (src, Arc::clone(&flag)),
        |(s, f)| async move {
            loop {
                f.reset();
                if s.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    return None;
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
        Err(st) => {
            call.finish(status_final(&st, Vec::new()));
            return;
        }
    };
    let initial = resp.metadata().to_vec();
    call.resp.lock().unwrap().initial = initial.clone();
    let mut stream = resp.into_inner();
    loop {
        if call.cancelled.load(std::sync::atomic::Ordering::Acquire) {
            call.finish((1, "cancelled".into(), initial, Vec::new()));
            return;
        }
        // 取消しはメッセージを待つ間にも効かせる（旗で起こす）
        let msg = {
            let next = stream.message();
            futures::pin_mut!(next);
            let cancel = async {
                loop {
                    cancel_flag.reset();
                    if call.cancelled.load(std::sync::atomic::Ordering::Acquire) {
                        return;
                    }
                    cancel_flag.wait().await;
                }
            };
            futures::pin_mut!(cancel);
            match futures::future::select(next, cancel).await {
                futures::future::Either::Left((m, _)) => Some(m),
                futures::future::Either::Right(_) => None,
            }
        };
        match msg {
            None => {
                call.finish((1, "cancelled".into(), initial, Vec::new()));
                return;
            }
            Some(Ok(Some(b))) => call.push_msg(b),
            Some(Ok(None)) => {
                let trailers = stream.trailers().to_vec();
                call.finish((0, String::new(), initial, trailers));
                return;
            }
            Some(Err(st)) => {
                call.finish(status_final(&st, initial));
                return;
            }
        }
    }
}

/// A gRPC channel for Python (see `veil_kit.grpc.Channel`).
#[pyclass(name = "GrpcChannel", module = "veil._veil", frozen)]
pub struct GrpcChannel {
    channel: Channel,
}

#[pymethods]
impl GrpcChannel {
    /// A channel to `url` (`http://` = h2c, `https://` = TLS with ALPN h2). `ca` lists trusted CA files (default:
    /// the system's), `cert` is a client certificate `(cert_file, key_file)`.
    #[new]
    #[pyo3(signature = (url, ca=None, cert=None, gzip=false, max_message_size=None))]
    fn new(
        url: &str,
        ca: Option<Vec<String>>,
        cert: Option<(String, String)>,
        gzip: bool,
        max_message_size: Option<usize>,
    ) -> PyResult<Self> {
        let mut b = veil::api::client::Client::builder().http2(veil::api::client::Http2::On);
        if let Some(ca) = ca {
            b = b.ca(ca);
        }
        if let Some((c, k)) = cert {
            b = b.client_cert(c, k);
        }
        let client = b
            .build()
            .map_err(|e| PyValueError::new_err(e.to_string()))?;
        let mut channel = Channel::with_client(&client, url)
            .map_err(|e| PyValueError::new_err(e.message().to_string()))?;
        if gzip {
            channel = channel.compression(Compression::Gzip);
        }
        if let Some(n) = max_message_size {
            channel = channel.max_message_size(n);
        }
        Ok(GrpcChannel { channel })
    }

    /// Start a call of `method` (`/package.Service/Method`); `metadata` is a list of `(name, str | bytes)`.
    #[pyo3(signature = (method, metadata=Vec::new(), timeout=None))]
    fn call(
        &self,
        method: &str,
        metadata: Vec<(String, Bound<'_, PyAny>)>,
        timeout: Option<f64>,
    ) -> PyResult<GrpcCall> {
        let rt = rt()?;
        let mut md = Vec::with_capacity(metadata.len());
        for (n, v) in metadata {
            let v: Bytes = if let Ok(b) = v.cast::<PyBytes>() {
                Bytes::copy_from_slice(b.as_bytes())
            } else {
                Bytes::from(v.extract::<String>()?)
            };
            md.push((Bytes::from(n.to_ascii_lowercase()), v));
        }
        let timeout = match timeout {
            Some(t) if !(t.is_finite() && t >= 0.0) => {
                return Err(PyValueError::new_err(
                    "timeout must be a non-negative number",
                ))
            }
            Some(t) => Some(Duration::from_secs_f64(t)),
            None => None,
        };
        let shared = Arc::new(CallShared {
            reqs: Mutex::new(VecDeque::new()),
            flag: Mutex::new(None),
            cancelled: std::sync::atomic::AtomicBool::new(false),
            sending_done: std::sync::atomic::AtomicBool::new(false),
            resp: Mutex::new(RespState::default()),
            cond: Condvar::new(),
        });
        rt.queue.lock().unwrap().push_back(Cmd::Start {
            channel: self.channel.clone(),
            path: method.to_string(),
            metadata: md,
            timeout,
            call: Arc::clone(&shared),
        });
        rt.wake.notify();
        Ok(GrpcCall { shared })
    }
}

/// One call started by `GrpcChannel.call`.
#[pyclass(name = "GrpcCall", module = "veil._veil", frozen)]
pub struct GrpcCall {
    shared: Arc<CallShared>,
}

#[pymethods]
impl GrpcCall {
    /// Send one request message; `False` once the call has finished or sending was closed.
    fn send(&self, message: &Bound<'_, PyAny>) -> PyResult<bool> {
        let s = &self.shared;
        if s.sending_done.load(std::sync::atomic::Ordering::Acquire)
            || s.resp.lock().unwrap().done.is_some()
        {
            return Ok(false);
        }
        let b = if let Ok(b) = message.cast::<PyBytes>() {
            Bytes::copy_from_slice(b.as_bytes())
        } else {
            Bytes::from(message.extract::<Vec<u8>>()?)
        };
        s.reqs.lock().unwrap().push_back(Some(b));
        s.wake_task();
        Ok(true)
    }

    /// Finish sending (half-close).
    fn done_sending(&self) {
        let s = &self.shared;
        if !s
            .sending_done
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            s.reqs.lock().unwrap().push_back(None);
            s.wake_task();
        }
    }

    /// The next response message (`bytes`), or `None` at the end; waits with the GIL released.
    fn recv(&self, py: Python<'_>) -> Option<Py<PyBytes>> {
        let s = Arc::clone(&self.shared);
        let b = py.detach(move || {
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
        });
        b.map(|b| PyBytes::new(py, &b).unbind())
    }

    /// Call `callback(bytes | None)` with the next response message (from another thread).
    fn recv_cb(&self, py: Python<'_>, callback: Py<PyAny>) -> PyResult<()> {
        let mut st = self.shared.resp.lock().unwrap();
        let now: Option<Py<PyAny>> = if let Some(b) = st.msgs.pop_front() {
            Some(PyBytes::new(py, &b).into_any().unbind())
        } else if st.done.is_some() {
            Some(py.None())
        } else {
            st.recv_cb = Some(callback.clone_ref(py));
            None
        };
        drop(st);
        if let Some(v) = now {
            callback.call1(py, (v,))?;
        }
        Ok(())
    }

    /// `(code, details, initial_metadata, trailing_metadata)`; waits for the end of the call with the GIL released.
    fn status(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let s = Arc::clone(&self.shared);
        let f = py.detach(move || {
            let mut st = s.resp.lock().unwrap();
            loop {
                if let Some(f) = &st.done {
                    return f.clone();
                }
                st = s.cond.wait(st).unwrap();
            }
        });
        final_tuple(py, &f)
    }

    /// Call `callback((code, details, initial, trailing))` at the end of the call.
    fn status_cb(&self, py: Python<'_>, callback: Py<PyAny>) -> PyResult<()> {
        let mut st = self.shared.resp.lock().unwrap();
        if let Some(f) = st.done.clone() {
            drop(st);
            let v = final_tuple(py, &f)?;
            callback.call1(py, (v,))?;
        } else {
            st.status_cb = Some(callback);
        }
        Ok(())
    }

    /// The response headers (empty until they arrived).
    fn initial_metadata(&self, py: Python<'_>) -> Vec<(String, Py<PyAny>)> {
        let st = self.shared.resp.lock().unwrap();
        pairs(py, &st.initial)
    }

    /// Cancel the call (the status becomes CANCELLED unless it already finished).
    fn cancel(&self) {
        self.shared
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.shared.wake_task();
    }
}

// 終わる前に Python のオブジェクトが捨てられたら取り消す（相手に RST_STREAM を送り、task を終わらせる）
impl Drop for GrpcCall {
    fn drop(&mut self) {
        if self.shared.resp.lock().unwrap().done.is_none() {
            self.cancel_inner();
        }
    }
}

impl GrpcCall {
    fn cancel_inner(&self) {
        self.shared
            .cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.shared.wake_task();
    }
}
