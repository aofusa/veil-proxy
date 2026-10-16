//! Python の gRPC・L4（F-210）の Rust 側。
//!
//! - gRPC: 1 回の呼び出しを [`NativeGrpcCall`] で Python へ渡す（受信・送信は GIL を外して待つ）。4 種類の RPC・
//!   直列化（`request_deserializer` / `response_serializer`）・grpcio の生成コードへの対応は Python 側
//!   （`veil/grpc.py`）。メッセージはバイト列のまま。
//! - L4: TCP は接続ごとに Python のプロトコル（`connection_made` / `data_received` / `connection_lost`）と
//!   [`NativeTransport`]（`write` / `close` / `get_extra_info`）を作る。UDP は関数を呼ぶ。

use bytes::Bytes;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString, PyTuple};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use veil::api::grpc::blocking::GrpcCall;
use veil::api::grpc::{Code, Status};
use veil::api::l4::{L4Out, L4Session};

// ===================== gRPC =====================

/// 1 回の gRPC の呼び出し（Python のハンドラの間だけ有効。終わったら操作は例外）。
#[pyclass(name = "NativeGrpcCall", module = "veil._veil")]
pub struct NativeGrpcCall {
    /// `*mut GrpcCall`（呼び出しの外は 0）
    ptr: AtomicUsize,
}

impl NativeGrpcCall {
    fn with_call<R>(&self, f: impl FnOnce(&mut GrpcCall) -> R) -> PyResult<R> {
        let p = self.ptr.load(Ordering::Acquire);
        if p == 0 {
            return Err(PyRuntimeError::new_err("the gRPC call has finished"));
        }
        // SAFETY: ptr は `run_grpc` がハンドラの呼び出しの間だけ置く `&mut GrpcCall`（同じプールのスレッド）。
        // 借用はこのクロージャの間だけで、Python のハンドラは同じスレッドで 1 つずつ呼ぶので重ならない。
        Ok(f(unsafe { &mut *(p as *mut GrpcCall) }))
    }
}

#[pymethods]
impl NativeGrpcCall {
    /// 次の受信メッセージ（終わりは None）。壊れた・大きすぎるメッセージは `(code, message)` の RuntimeError。
    fn recv(&self, py: Python<'_>) -> PyResult<Option<Py<PyBytes>>> {
        match self.with_call(|call| py.allow_threads(|| call.recv()))? {
            Ok(Some(b)) => Ok(Some(PyBytes::new(py, &b).unbind())),
            Ok(None) => Ok(None),
            Err(s) => Err(PyRuntimeError::new_err((
                s.code().as_u8(),
                s.message().to_string(),
            ))),
        }
    }

    /// 送る。クライアントが切断していたら False。
    fn send(&self, py: Python<'_>, data: &[u8]) -> PyResult<bool> {
        let b = Bytes::copy_from_slice(data);
        self.with_call(|call| py.allow_threads(|| call.send(b)))
    }

    /// 応答ヘッダに足す（最初の send より前だけ。後なら False）。
    fn add_header(&self, name: &str, value: &Bound<'_, PyAny>) -> PyResult<bool> {
        let v = meta_value(value)?;
        self.with_call(|call| call.add_header(lower(name), v))
    }

    /// Add a response trailer (sent after the last message).
    fn add_trailer(&self, name: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let v = meta_value(value)?;
        self.with_call(|call| call.add_trailer(lower(name), v))
    }

    /// 要求メタデータ `[(name, value)]`（`-bin` の値は bytes、他は str）。
    fn metadata(&self, py: Python<'_>) -> PyResult<Vec<(String, Py<PyAny>)>> {
        self.with_call(|call| {
            call.metadata()
                .iter()
                .map(|(n, v)| {
                    let name = String::from_utf8_lossy(n).into_owned();
                    let val = if name.ends_with("-bin") {
                        PyBytes::new(py, v).into_any().unbind()
                    } else {
                        PyString::new(py, &String::from_utf8_lossy(v))
                            .into_any()
                            .unbind()
                    };
                    (name, val)
                })
                .collect()
        })
    }

    /// Full method path of the call, e.g. `/pkg.Svc/Method`.
    fn method(&self) -> PyResult<String> {
        self.with_call(|call| call.method().to_string())
    }

    /// 期限までの残り秒（期限なしは None）。
    fn time_remaining(&self) -> PyResult<Option<f64>> {
        self.with_call(|call| {
            call.deadline().map(|d| {
                d.saturating_duration_since(std::time::Instant::now())
                    .as_secs_f64()
            })
        })
    }

    /// False once the client has cancelled or disconnected.
    fn is_active(&self) -> PyResult<bool> {
        self.with_call(|call| !call.is_cancelled())
    }
}

fn lower(name: &str) -> Bytes {
    Bytes::from(name.to_ascii_lowercase())
}

fn meta_value(v: &Bound<'_, PyAny>) -> PyResult<Bytes> {
    if let Ok(s) = v.downcast::<PyString>() {
        return Ok(Bytes::from(s.to_cow()?.into_owned()));
    }
    let b: Vec<u8> = v.extract()?;
    Ok(Bytes::from(b))
}

/// Python のハンドラ（`handle(call) -> (code, message)`）で 1 回の呼び出しを処理する。
pub fn run_grpc(handle: &Py<PyAny>, call: &mut GrpcCall) -> Status {
    Python::with_gil(|py| {
        let native = match Py::new(
            py,
            NativeGrpcCall {
                ptr: AtomicUsize::new(call as *mut GrpcCall as usize),
            },
        ) {
            Ok(n) => n,
            Err(e) => {
                e.print(py);
                return Status::internal("internal error");
            }
        };
        let r = handle.call1(py, (native.clone_ref(py),));
        native.borrow(py).ptr.store(0, Ordering::Release);
        match r.and_then(|o| o.extract::<(u8, String)>(py)) {
            Ok((code, msg)) => Status::new(Code::from_u8(code).unwrap_or(Code::Unknown), msg),
            Err(e) => {
                e.print(py);
                Status::internal("handler raised an exception")
            }
        }
    })
}

// ===================== L4 =====================

/// L4 の TCP 接続の送り口（asyncio の Transport と同じ名前のメソッド）。`write` はコールバックの中でだけ。
#[pyclass(name = "NativeTransport", module = "veil._veil")]
pub struct NativeTransport {
    /// コールバックの間だけ指す `*mut L4Out`
    out: AtomicUsize,
    peer: SocketAddr,
    closing: std::sync::atomic::AtomicBool,
}

impl NativeTransport {
    fn with_out<R>(&self, f: impl FnOnce(&mut L4Out) -> R) -> PyResult<R> {
        let p = self.out.load(Ordering::Acquire);
        if p == 0 {
            return Err(PyRuntimeError::new_err(
                "transport.write is only valid inside connection_made / data_received",
            ));
        }
        // SAFETY: out は PySession がコールバックの間だけ置く `&mut L4Out`（同じプールのスレッド）。
        Ok(f(unsafe { &mut *(p as *mut L4Out) }))
    }
}

#[pymethods]
impl NativeTransport {
    /// Send bytes to the peer (valid only inside the protocol callbacks).
    fn write(&self, data: &[u8]) -> PyResult<()> {
        let b = Bytes::copy_from_slice(data);
        self.with_out(|o| o.write(b))
    }

    /// 送り終えたら閉じる。
    fn close(&self) -> PyResult<()> {
        self.closing.store(true, Ordering::Release);
        let _ = self.with_out(|o| o.close());
        Ok(())
    }

    /// True after `close()` was called.
    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// `"peername"` は `(host, port)`。
    #[pyo3(signature = (name, default=None))]
    fn get_extra_info(
        &self,
        py: Python<'_>,
        name: &str,
        default: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        if name == "peername" {
            let t = PyTuple::new(
                py,
                [
                    PyString::new(py, &self.peer.ip().to_string()).into_any(),
                    self.peer.port().into_pyobject(py)?.into_any(),
                ],
            )?;
            return Ok(t.into_any().unbind());
        }
        Ok(default.unwrap_or_else(|| py.None()))
    }
}

/// 1 つの TCP 接続の Python のプロトコル。
pub struct PySession {
    proto: Py<PyAny>,
    transport: Py<NativeTransport>,
}

impl PySession {
    fn invoke(&self, py: Python<'_>, out: &mut L4Out, f: impl FnOnce() -> PyResult<()>) {
        let t = self.transport.borrow(py);
        t.out.store(out as *mut L4Out as usize, Ordering::Release);
        let r = f();
        t.out.store(0, Ordering::Release);
        if t.closing.load(Ordering::Acquire) {
            out.close();
        }
        if let Err(e) = r {
            e.print(py);
            out.close();
        }
    }

    /// 接続（`factory()` でプロトコルを作り `connection_made(transport)`）。
    pub fn connect(
        factory: &Py<PyAny>,
        peer: SocketAddr,
        out: &mut L4Out,
    ) -> Option<Box<dyn L4Session>> {
        Python::with_gil(|py| {
            let proto = match factory.call0(py) {
                Ok(p) => p,
                Err(e) => {
                    e.print(py);
                    return None;
                }
            };
            let transport = Py::new(
                py,
                NativeTransport {
                    out: AtomicUsize::new(0),
                    peer,
                    closing: std::sync::atomic::AtomicBool::new(false),
                },
            )
            .ok()?;
            let s = PySession { proto, transport };
            s.invoke(py, out, || {
                s.proto
                    .call_method1(py, "connection_made", (s.transport.clone_ref(py),))
                    .map(|_| ())
            });
            Some(Box::new(s) as Box<dyn L4Session>)
        })
    }
}

impl L4Session for PySession {
    fn on_data(&mut self, data: &[u8], out: &mut L4Out) {
        Python::with_gil(|py| {
            let b = PyBytes::new(py, data);
            self.invoke(py, out, || {
                self.proto
                    .call_method1(py, "data_received", (b,))
                    .map(|_| ())
            });
        })
    }

    fn on_close(&mut self) {
        Python::with_gil(|py| {
            if let Err(e) = self.proto.call_method1(py, "connection_lost", (py.None(),)) {
                e.print(py);
            }
        })
    }
}

/// UDP: `f(data: bytes, peer: (host, port)) -> bytes | None`。
pub fn run_udp(f: &Py<PyAny>, data: Bytes, peer: SocketAddr) -> Option<Bytes> {
    Python::with_gil(|py| {
        let r = (|| -> PyResult<Option<Bytes>> {
            let peer = PyTuple::new(
                py,
                [
                    PyString::new(py, &peer.ip().to_string()).into_any(),
                    peer.port().into_pyobject(py)?.into_any(),
                ],
            )?;
            let out = f.call1(py, (PyBytes::new(py, &data), peer))?;
            if out.is_none(py) {
                return Ok(None);
            }
            let v: Vec<u8> = out.extract(py)?;
            Ok(Some(Bytes::from(v)))
        })();
        r.unwrap_or_else(|e| {
            e.print(py);
            None
        })
    })
}
