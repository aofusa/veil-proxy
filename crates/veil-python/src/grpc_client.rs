// F-268: Python の gRPC クライアント（veil_kit.grpc.Channel の土台）。
// F-291: 呼び出しの仕組み（専用スレッド・キューと WakeFlag・取消し）は veil::api::grpc::client_call へ移し、C の
// veil_grpc_channel_* と共有する。ここは Python の型への写しだけ。
//! gRPC client calls for Python (`veil_kit.grpc`), on veil's own HTTP/2 client.
//!
//! Calls run on veil's client thread ([`veil::api::client_thread`]) through
//! [`veil::api::grpc::client_call::BlockingCall`], shared with the C API: responses come back by waiting (with the
//! GIL released) or through a callback that the handler pool calls with the GIL (asyncio:
//! `loop.call_soon_threadsafe`). Every call uses the bidirectional primitive with the raw-bytes codec, so the four
//! kinds of methods share one path (protobuf encoding stays in Python).

use bytes::Bytes;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyBytes;
use std::time::Duration;
use veil::api::grpc::client_call::{BlockingCall, CallStatus};
use veil::api::grpc::{Channel, Compression};

/// Start the client thread now (otherwise it starts with the first call).
#[pyfunction]
pub fn grpc_client_setup() -> PyResult<()> {
    veil::api::client_thread::start().map_err(PyRuntimeError::new_err)
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

fn final_tuple(py: Python<'_>, f: &CallStatus) -> PyResult<Py<PyAny>> {
    let t = (
        f.code,
        f.message.clone(),
        pairs(py, &f.initial),
        pairs(py, &f.trailers),
    );
    Ok(t.into_pyobject(py)?.into_any().unbind())
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
        // B-198: チャネルの URL はアプリが書くので Unix ソケットも受ける（従来どおり）
        let mut b = veil::api::client::Client::builder()
            .http2(veil::api::client::Http2::On)
            .unix_sockets(true);
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
        let mut md = Vec::with_capacity(metadata.len());
        for (n, v) in metadata {
            let v: Bytes = if let Ok(b) = v.cast::<PyBytes>() {
                Bytes::copy_from_slice(b.as_bytes())
            } else {
                Bytes::from(v.extract::<String>()?)
            };
            md.push((Bytes::from(n.to_ascii_lowercase()), v));
        }
        // B-203: 大きすぎる値で Duration::from_secs_f64 が panic しない
        let timeout = match timeout {
            Some(t) => Some(Duration::try_from_secs_f64(t).map_err(|_| {
                PyValueError::new_err("timeout must be a non-negative number of seconds")
            })?),
            None => None,
        };
        let call = BlockingCall::start(&self.channel, method, md, timeout)
            .map_err(PyRuntimeError::new_err)?;
        Ok(GrpcCall { call })
    }
}

/// One call started by `GrpcChannel.call`. Dropping it before the call finished cancels the call.
#[pyclass(name = "GrpcCall", module = "veil._veil", frozen)]
pub struct GrpcCall {
    call: BlockingCall,
}

#[pymethods]
impl GrpcCall {
    /// Send one request message; `False` once the call has finished or sending was closed.
    fn send(&self, message: &Bound<'_, PyAny>) -> PyResult<bool> {
        let b = if let Ok(b) = message.cast::<PyBytes>() {
            Bytes::copy_from_slice(b.as_bytes())
        } else {
            Bytes::from(message.extract::<Vec<u8>>()?)
        };
        Ok(self.call.send(b))
    }

    /// Finish sending (half-close).
    fn done_sending(&self) {
        self.call.done_sending();
    }

    /// The next response message (`bytes`), or `None` at the end; waits with the GIL released.
    fn recv(&self, py: Python<'_>) -> Option<Py<PyBytes>> {
        let b = py.detach(|| self.call.recv());
        b.map(|b| PyBytes::new(py, &b).unbind())
    }

    /// Call `callback(bytes | None)` with the next response message (from another thread).
    fn recv_cb(&self, callback: Py<PyAny>) {
        self.call.recv_then(move |msg| {
            Python::attach(|py| {
                let arg: Py<PyAny> = match msg {
                    Some(b) => PyBytes::new(py, &b).into_any().unbind(),
                    None => py.None(),
                };
                if let Err(e) = callback.call1(py, (arg,)) {
                    e.print(py);
                }
            })
        });
    }

    /// `(code, details, initial_metadata, trailing_metadata)`; waits for the end of the call with the GIL released.
    fn status(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let f = py.detach(|| self.call.status());
        final_tuple(py, &f)
    }

    /// Call `callback((code, details, initial, trailing))` at the end of the call.
    fn status_cb(&self, callback: Py<PyAny>) {
        self.call.status_then(move |f| {
            Python::attach(|py| {
                if let Err(e) = final_tuple(py, &f).and_then(|v| callback.call1(py, (v,))) {
                    e.print(py);
                }
            })
        });
    }

    /// The response headers (empty until they arrived).
    fn initial_metadata(&self, py: Python<'_>) -> Vec<(String, Py<PyAny>)> {
        pairs(py, &self.call.initial_metadata())
    }

    /// Cancel the call (the status becomes CANCELLED unless it already finished).
    fn cancel(&self) {
        self.call.cancel();
    }
}
