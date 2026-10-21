// F-210: Python の gRPC・L4 の Rust 側。
//! Rust side of the Python gRPC and L4 support.
//!
//! - gRPC: each call is handed to Python as a [`NativeGrpcCall`]; receiving and sending wait with the GIL released.
//!   The four RPC kinds, (de)serialization (`request_deserializer` / `response_serializer`) and the grpcio
//!   generated-code adapter live on the Python side (`veil/grpc.py`). Messages stay as raw bytes.
//! - L4: for every TCP connection a Python protocol (`connection_made` / `data_received` / `connection_lost`) and a
//!   [`NativeTransport`] (`write` / `close` / `get_extra_info`) are created. UDP calls a plain function.

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

/// One gRPC call; valid only while the handler runs (operations raise once it has finished).
#[pyclass(name = "NativeGrpcCall", module = "veil._veil")]
pub struct NativeGrpcCall {
    // `*mut GrpcCall`（呼び出しの外は 0）
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
    /// Next request message, or `None` at the end of the stream.
    ///
    /// A malformed or oversized message raises `RuntimeError` with `(code, message)`.
    fn recv(&self, py: Python<'_>) -> PyResult<Option<Py<PyBytes>>> {
        match self.with_call(|call| py.detach(|| call.recv()))? {
            Ok(Some(b)) => Ok(Some(PyBytes::new(py, &b).unbind())),
            Ok(None) => Ok(None),
            Err(s) => Err(PyRuntimeError::new_err((
                s.code().as_u8(),
                s.message().to_string(),
            ))),
        }
    }

    /// Send a response message. `False` if the client has gone.
    fn send(&self, py: Python<'_>, data: &[u8]) -> PyResult<bool> {
        let b = Bytes::copy_from_slice(data);
        self.with_call(|call| py.detach(|| call.send(b)))
    }

    /// Add a response header (before the first `send`; otherwise `False`).
    fn add_header(&self, name: &str, value: &Bound<'_, PyAny>) -> PyResult<bool> {
        let v = meta_value(value)?;
        self.with_call(|call| call.add_header(lower(name), v))
    }

    /// Add a response trailer (sent after the last message).
    fn add_trailer(&self, name: &str, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let v = meta_value(value)?;
        self.with_call(|call| call.add_trailer(lower(name), v))
    }

    /// Request metadata as `[(name, value)]` (`-bin` values are `bytes`, the others `str`).
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

    /// Seconds until the deadline, or `None` if there is none.
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
    if let Ok(s) = v.cast::<PyString>() {
        return Ok(Bytes::from(s.to_cow()?.into_owned()));
    }
    let b: Vec<u8> = v.extract()?;
    Ok(Bytes::from(b))
}

/// Run one call through the Python handler (`handle(call) -> (code, message)`).
pub fn run_grpc(handle: &Py<PyAny>, call: &mut GrpcCall) -> Status {
    Python::attach(|py| {
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

/// Send side of an L4 TCP connection (asyncio-transport-like). `write` is valid only inside the protocol callbacks.
#[pyclass(name = "NativeTransport", module = "veil._veil")]
pub struct NativeTransport {
    // コールバックの間だけ指す `*mut L4Out`
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

    /// Close after the pending data has been sent.
    fn close(&self) -> PyResult<()> {
        self.closing.store(true, Ordering::Release);
        let _ = self.with_out(|o| o.close());
        Ok(())
    }

    /// True after `close()` was called.
    fn is_closing(&self) -> bool {
        self.closing.load(Ordering::Acquire)
    }

    /// `"peername"` gives `(host, port)`; other names give `default`.
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

/// The Python protocol object of one TCP connection.
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

    /// Accept a connection: create the protocol with `factory()` and call `connection_made(transport)`.
    pub fn connect(
        factory: &Py<PyAny>,
        peer: SocketAddr,
        out: &mut L4Out,
    ) -> Option<Box<dyn L4Session>> {
        Python::attach(|py| {
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
        Python::attach(|py| {
            let b = PyBytes::new(py, data);
            self.invoke(py, out, || {
                self.proto
                    .call_method1(py, "data_received", (b,))
                    .map(|_| ())
            });
        })
    }

    fn on_close(&mut self) {
        Python::attach(|py| {
            if let Err(e) = self.proto.call_method1(py, "connection_lost", (py.None(),)) {
                e.print(py);
            }
        })
    }
}

/// UDP: `f(data: bytes, peer: (host, port)) -> bytes | None`。
pub fn run_udp(f: &Py<PyAny>, data: Bytes, peer: SocketAddr) -> Option<Bytes> {
    Python::attach(|py| {
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
