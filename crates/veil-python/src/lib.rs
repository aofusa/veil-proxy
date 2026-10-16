//! veil API サーバライブラリの Python バインディング（F-194）。
//!
//! Python 側（`python/veil/__init__.py`）がデコレータでルートを集め、`register_route` で
//! ここへ登録し、`run` で起動する。リクエストごとにルートの呼び出し口（`Route._call`）を GIL 下で
//! [`native::NativeRequest`] 1 つを渡して呼び、戻り値を Rust で応答にする（F-212）。

mod ext;
mod native;

use bytes::Bytes;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use std::sync::{Arc, Mutex};
use veil::api::bridge::HandlerContext;
use veil::api::{App, Config as VeilConfig, HandlerFn, OperationMeta, Request, Response};

/// `register_route` で登録された、起動前のルート。
struct PendingRoute {
    path: String,
    methods: Vec<String>,
    handler_id: u64,
    offload: bool,
    openapi: Option<serde_json::Value>,
    /// 既定のステータス（デコレータの `status_code`）
    status_code: u16,
    /// 要求本文をストリーミングで受ける（`stream_body=True`、F-210）
    stream_body: bool,
}

static PENDING: Mutex<Vec<PendingRoute>> = Mutex::new(Vec::new());

/// gRPC・L4 の登録（`run` までの間に溜める。F-210）。
enum PendingExtra {
    Grpc {
        path: String,
        handler_id: u64,
    },
    L4Tcp {
        name: String,
        listen: String,
        handler_id: u64,
    },
    L4Udp {
        name: String,
        listen: String,
        handler_id: u64,
    },
    GrpcHealth,
}

static PENDING_EXTRA: Mutex<Vec<PendingExtra>> = Mutex::new(Vec::new());

/// gRPC のメソッドを登録する（`handlers[handler_id]` は `handle(call) -> (code, message)` を持つ）。
#[pyfunction]
fn register_grpc(path: String, handler_id: u64) {
    PENDING_EXTRA
        .lock()
        .unwrap()
        .push(PendingExtra::Grpc { path, handler_id });
}

/// `grpc.health.v1.Health`（Check・Watch）を登録する。
#[pyfunction]
fn register_grpc_health() {
    PENDING_EXTRA.lock().unwrap().push(PendingExtra::GrpcHealth);
}

/// L4 の TCP リスナー（`handlers[handler_id]` はプロトコルを作る呼び出し可能オブジェクト）。
#[pyfunction]
fn register_l4_tcp(name: String, listen: String, handler_id: u64) {
    PENDING_EXTRA.lock().unwrap().push(PendingExtra::L4Tcp {
        name,
        listen,
        handler_id,
    });
}

/// L4 の UDP リスナー（`handlers[handler_id]` は `f(data, peer) -> bytes | None`）。
#[pyfunction]
fn register_l4_udp(name: String, listen: String, handler_id: u64) {
    PENDING_EXTRA.lock().unwrap().push(PendingExtra::L4Udp {
        name,
        listen,
        handler_id,
    });
}

/// 設定（`veil::api::Config` のラッパ）。
#[pyclass(name = "Config", module = "veil._veil")]
#[derive(Clone)]
struct PyConfig {
    inner: VeilConfig,
}

fn cfg_err(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

#[pymethods]
impl PyConfig {
    /// ライブラリ既定設定。
    #[new]
    fn new() -> Self {
        PyConfig {
            inner: VeilConfig::new(),
        }
    }

    /// config.toml を取り込む。
    #[staticmethod]
    fn from_file(path: &str) -> PyResult<Self> {
        Ok(PyConfig {
            inner: VeilConfig::from_file(path).map_err(cfg_err)?,
        })
    }

    /// 任意のキーを TOML リテラルで上書きする（例: `set("server.threads", "4")`）。
    fn set(&mut self, path: &str, toml_value: &str) -> PyResult<()> {
        self.inner
            .set_override(&format!("{} = {}", path, toml_value))
            .map_err(cfg_err)
    }

    /// `[server].listen`（TLS のリスナー。`tls(cert, key)` と併用する）。
    fn listen(&mut self, addr: &str) -> PyResult<()> {
        let lit = serde_json::to_string(addr).map_err(cfg_err)?;
        self.set("server.listen", &lit)
    }

    /// `[server].listen_cleartext`（平文 HTTP/1.1 + h2c のリスナー。ライブラリ既定は `127.0.0.1:8080`）。
    fn listen_cleartext(&mut self, addr: &str) -> PyResult<()> {
        let lit = serde_json::to_string(addr).map_err(cfg_err)?;
        self.set("server.listen_cleartext", &lit)
    }

    /// `[tls].cert_path` / `key_path`。
    fn tls(&mut self, cert_path: &str, key_path: &str) -> PyResult<()> {
        let cert = serde_json::to_string(cert_path).map_err(cfg_err)?;
        let key = serde_json::to_string(key_path).map_err(cfg_err)?;
        self.set("tls.cert_path", &cert)?;
        self.set("tls.key_path", &key)
    }

    /// `[server].threads`。
    fn threads(&mut self, n: usize) -> PyResult<()> {
        self.set("server.threads", &n.to_string())
    }

    /// 合成後の設定を TOML 文字列で返す（デバッグ用）。
    fn to_toml(&self) -> PyResult<String> {
        self.inner.to_toml_string().map_err(cfg_err)
    }
}

/// ルートを登録する（`run` までの間に溜める）。
#[pyfunction]
#[pyo3(signature = (path, methods, handler_id, offload=true, openapi_json=None, status_code=200, stream_body=false))]
#[allow(clippy::too_many_arguments)] // 理由: Python から呼ぶ登録関数で、引数はキーワード引数として渡る
fn register_route(
    path: String,
    methods: Vec<String>,
    handler_id: u64,
    offload: bool,
    openapi_json: Option<String>,
    status_code: u16,
    stream_body: bool,
) -> PyResult<()> {
    let openapi = match openapi_json {
        Some(s) => Some(
            serde_json::from_str::<serde_json::Value>(&s)
                .map_err(|e| PyValueError::new_err(format!("invalid openapi_json: {}", e)))?,
        ),
        None => None,
    };
    PENDING.lock().unwrap().push(PendingRoute {
        path,
        methods,
        handler_id,
        offload,
        openapi,
        status_code,
        stream_body,
    });
    Ok(())
}

/// 1 ルートの Python 側の呼び出し口（`Route._call` と、扱わない戻り値の変換 `Route.to_response`）。
struct PyRoute {
    call: Py<PyAny>,
    fallback: Py<PyAny>,
    status: u16,
}

/// ルートの呼び出し口を GIL 下で呼び、応答を組み立てる。`ctx` はハンドラ用プールで動くとき（ストリーミングの
/// 応答・要求本文の順次受信ができる。F-210）。
fn call_python(route: &PyRoute, req: Request, ctx: Option<&mut HandlerContext>) -> Response {
    Python::with_gil(|py| {
        let r = (|| -> PyResult<Response> {
            let native_req = native::NativeRequest::new(req);
            let ctx_ptr = ctx.map(|c| c as *mut HandlerContext);
            if let Some(p) = ctx_ptr {
                native_req
                    .ctx
                    .store(p as usize, std::sync::atomic::Ordering::Release);
            }
            let req = Py::new(py, native_req)?;
            let out = route.call.call1(py, (req.clone_ref(py),));
            let resp = out.and_then(|out| {
                // SAFETY: ctx_ptr は call_blocking の閉包が持つ HandlerContext（この呼び出しの間有効）
                let ctx = ctx_ptr.map(|p| unsafe { &mut *p });
                native::to_response(py, out.into_bound(py), route.status, &route.fallback, ctx)
            });
            // 呼び出しの外で要求本文を読まれないように外す
            req.borrow(py)
                .ctx
                .store(0, std::sync::atomic::Ordering::Release);
            resp
        })();
        match r {
            Ok(resp) => resp,
            Err(err) => {
                err.print(py);
                internal_error()
            }
        }
    })
}

fn internal_error() -> Response {
    Response::json_bytes(Bytes::from_static(
        b"{\"detail\":\"Internal Server Error\"}",
    ))
    .with_status(500)
}

fn make_handler(route: PyRoute, offload: bool) -> HandlerFn {
    let route = Arc::new(route);
    Arc::new(move |req: Request| {
        let r = Arc::clone(&route);
        Box::pin(async move {
            if offload {
                // GIL を握る呼び出しはイベントループ（ワーカー）を止めないようハンドラ用プールへ退避する。
                // 応答のストリーミング・要求本文の順次受信は call_blocking の文脈で（F-210）
                veil::api::bridge::call_blocking(req, move |req, ctx| {
                    call_python(&r, req, Some(ctx))
                })
                .await
            } else {
                call_python(&r, req, None)
            }
        })
    })
}

/// サーバを起動し、停止（SIGINT/SIGTERM・`shutdown()`）までブロックする。GIL は解放する。
/// `handlers` は handler_id → Python の `Route`（`_call` と `to_response` を持つ）。
#[pyfunction]
fn run(py: Python<'_>, config: PyConfig, handlers: Vec<Py<PyAny>>) -> PyResult<()> {
    let routes = std::mem::take(&mut *PENDING.lock().unwrap());
    let mut app = App::new().config(config.inner).without_macro_routes();
    for (i, r) in routes.into_iter().enumerate() {
        let methods: Vec<&str> = r.methods.iter().map(|s| s.as_str()).collect();
        let route = handlers
            .get(r.handler_id as usize)
            .ok_or_else(|| PyValueError::new_err(format!("unknown handler id {}", r.handler_id)))?
            .bind(py);
        let func = make_handler(
            PyRoute {
                call: route.getattr("_call")?.unbind(),
                fallback: route.getattr("to_response")?.unbind(),
                status: r.status_code,
            },
            r.offload,
        );
        let meta = OperationMeta {
            operation_id: format!("py_{}", r.handler_id),
            raw: r.openapi,
            hidden: false,
            ..Default::default()
        };
        let name = format!("py_{}_{}", i, r.handler_id);
        app = if r.stream_body {
            app.route_fn_streaming(&name, &methods, &r.path, func, meta)
        } else {
            app.route_fn(&name, &methods, &r.path, func, meta)
        };
    }
    let handler = |id: u64| -> PyResult<Py<PyAny>> {
        handlers
            .get(id as usize)
            .map(|h| h.clone_ref(py))
            .ok_or_else(|| PyValueError::new_err(format!("unknown handler id {}", id)))
    };
    for e in std::mem::take(&mut *PENDING_EXTRA.lock().unwrap()) {
        app = match e {
            PendingExtra::Grpc { path, handler_id } => {
                let h = handler(handler_id)?.bind(py).getattr("handle")?.unbind();
                app.grpc_blocking(&path, move |call| ext::run_grpc(&h, call))
            }
            PendingExtra::L4Tcp {
                name,
                listen,
                handler_id,
            } => {
                let factory = handler(handler_id)?;
                app.l4_tcp_blocking(&name, &listen, move |peer, out| {
                    ext::PySession::connect(&factory, peer, out)
                })
            }
            PendingExtra::L4Udp {
                name,
                listen,
                handler_id,
            } => {
                let f = handler(handler_id)?;
                app.l4_udp_blocking(&name, &listen, move |data, peer| {
                    ext::run_udp(&f, data, peer)
                })
            }
            PendingExtra::GrpcHealth => app.grpc_health(),
        };
    }
    drop(handlers);
    py.allow_threads(move || app.run())
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

/// 実行中のサーバを graceful に停止する。
#[pyfunction]
fn shutdown() {
    veil::api::app::shutdown();
}

/// Native extension of the veil API server library (use the `veil` package instead).
#[pymodule]
fn _veil(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyConfig>()?;
    m.add_class::<native::NativeRequest>()?;
    m.add_class::<native::NativeHeaders>()?;
    m.add_class::<native::NativeBodyIter>()?;
    m.add_class::<ext::NativeGrpcCall>()?;
    m.add_class::<ext::NativeTransport>()?;
    m.add_function(wrap_pyfunction!(register_grpc, m)?)?;
    m.add_function(wrap_pyfunction!(register_grpc_health, m)?)?;
    m.add_function(wrap_pyfunction!(register_l4_tcp, m)?)?;
    m.add_function(wrap_pyfunction!(register_l4_udp, m)?)?;
    m.add_function(wrap_pyfunction!(register_route, m)?)?;
    m.add_function(wrap_pyfunction!(run, m)?)?;
    m.add_function(wrap_pyfunction!(shutdown, m)?)?;
    m.add_function(wrap_pyfunction!(native::_json_fast, m)?)?;
    Ok(())
}
