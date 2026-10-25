// F-194: Python バインディング本体。F-212: 要求ごとの呼び出しは NativeRequest 1 つだけを渡す。
//! Python bindings for the veil API server library.
//!
//! The Python side (`python/veil/__init__.py`) collects routes with decorators, registers them here with
//! `register_route` and starts the server with `run`. For every request the route entry point (`Route._call`) is
//! called with the GIL held and a single [`native::NativeRequest`]; Rust turns the return value into the response.

mod aio;
mod ext;
mod native;

use bytes::Bytes;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use std::sync::{Arc, Mutex};
use veil::api::bridge::HandlerContext;
use veil::api::{
    App, Config as VeilConfig, HandlerFn, Listener as VeilListener, OperationMeta, Request,
    Response,
};

/// A route registered with `register_route`, kept until the server starts.
struct PendingRoute {
    path: String,
    methods: Vec<String>,
    handler_id: u64,
    offload: bool,
    openapi: Option<serde_json::Value>,
    // 既定のステータス（デコレータの `status_code`）
    status_code: u16,
    // F-210: 要求本文をストリーミングで受ける（`stream_body=True`）
    stream_body: bool,
    // F-263: `async def` を常駐のイベントループで動かす
    on_loop: bool,
}

// F-263: `@app.websocket` のルート（`run` までの間に溜める）。
/// A WebSocket route registered with `register_websocket`.
struct PendingWs {
    path: String,
    handler_id: u64,
    openapi: Option<serde_json::Value>,
    // `WebSocketEndpoint` のクラス（コールバックはハンドラ用プール）。偽なら `async def`（常駐のループ）
    endpoint: bool,
    hidden: bool,
}

static PENDING_WS: Mutex<Vec<PendingWs>> = Mutex::new(Vec::new());

/// Register a WebSocket route (collected until `run`).
#[pyfunction]
#[pyo3(signature = (path, handler_id, openapi_json=None, endpoint=false, hidden=false))]
fn register_websocket(
    path: String,
    handler_id: u64,
    openapi_json: Option<String>,
    endpoint: bool,
    hidden: bool,
) -> PyResult<()> {
    let openapi = parse_openapi(openapi_json)?;
    PENDING_WS.lock().unwrap().push(PendingWs {
        path,
        handler_id,
        openapi,
        endpoint,
        hidden,
    });
    Ok(())
}

fn parse_openapi(openapi_json: Option<String>) -> PyResult<Option<serde_json::Value>> {
    match openapi_json {
        Some(s) => Ok(Some(
            serde_json::from_str::<serde_json::Value>(&s)
                .map_err(|e| PyValueError::new_err(format!("invalid openapi_json: {}", e)))?,
        )),
        None => Ok(None),
    }
}

static PENDING: Mutex<Vec<PendingRoute>> = Mutex::new(Vec::new());

// F-210: gRPC・L4 の登録（`run` までの間に溜める）。
/// gRPC and L4 registrations, kept until `run`.
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

/// Register a gRPC method; `handlers[handler_id].handle(call)` returns `(code, message)`.
#[pyfunction]
fn register_grpc(path: String, handler_id: u64) {
    PENDING_EXTRA
        .lock()
        .unwrap()
        .push(PendingExtra::Grpc { path, handler_id });
}

/// Register `grpc.health.v1.Health` (Check and Watch).
#[pyfunction]
fn register_grpc_health() {
    PENDING_EXTRA.lock().unwrap().push(PendingExtra::GrpcHealth);
}

/// Register an L4 TCP listener; `handlers[handler_id]` creates the protocol.
#[pyfunction]
fn register_l4_tcp(name: String, listen: String, handler_id: u64) {
    PENDING_EXTRA.lock().unwrap().push(PendingExtra::L4Tcp {
        name,
        listen,
        handler_id,
    });
}

/// Register an L4 UDP listener; `handlers[handler_id](data, peer)` returns `bytes | None`.
#[pyfunction]
fn register_l4_udp(name: String, listen: String, handler_id: u64) {
    PENDING_EXTRA.lock().unwrap().push(PendingExtra::L4Udp {
        name,
        listen,
        handler_id,
    });
}

/// Server configuration (a wrapper of `veil::api::Config`).
#[pyclass(name = "Config", module = "veil._veil", from_py_object)]
#[derive(Clone)]
struct PyConfig {
    inner: VeilConfig,
}

fn cfg_err(e: impl std::fmt::Display) -> PyErr {
    PyValueError::new_err(e.to_string())
}

#[pymethods]
impl PyConfig {
    /// Create the library default configuration.
    #[new]
    fn new() -> Self {
        PyConfig {
            inner: VeilConfig::new(),
        }
    }

    /// Load a `config.toml`.
    #[staticmethod]
    fn from_file(path: &str) -> PyResult<Self> {
        Ok(PyConfig {
            inner: VeilConfig::from_file(path).map_err(cfg_err)?,
        })
    }

    /// Override any key with a TOML literal, e.g. `set("server.threads", "4")`.
    fn set(&mut self, path: &str, toml_value: &str) -> PyResult<()> {
        self.inner
            .set_override(&format!("{} = {}", path, toml_value))
            .map_err(cfg_err)
    }

    /// Set `[server].listen` (TLS listener; use together with `tls`).
    fn listen(&mut self, addr: &str) -> PyResult<()> {
        let lit = serde_json::to_string(addr).map_err(cfg_err)?;
        self.set("server.listen", &lit)
    }

    /// Set `[server].listen_cleartext` (HTTP/1.1 + h2c; default `127.0.0.1:8080`).
    fn listen_cleartext(&mut self, addr: &str) -> PyResult<()> {
        let lit = serde_json::to_string(addr).map_err(cfg_err)?;
        self.set("server.listen_cleartext", &lit)
    }

    /// Set `[tls].cert_path` and `key_path`.
    fn tls(&mut self, cert_path: &str, key_path: &str) -> PyResult<()> {
        let cert = serde_json::to_string(cert_path).map_err(cfg_err)?;
        let key = serde_json::to_string(key_path).map_err(cfg_err)?;
        self.set("tls.cert_path", &cert)?;
        self.set("tls.key_path", &key)
    }

    /// Set `[server].threads`.
    fn threads(&mut self, n: usize) -> PyResult<()> {
        self.set("server.threads", &n.to_string())
    }

    // F-260: 待ち受けを 1 個足す（`veil.Listener` と `run(listeners=...)` が使う）
    /// Add a `[[listener]]` (one port). Listeners added here replace the `[[listener]]` array of the file;
    /// with any listener, the default `listen_cleartext` (`127.0.0.1:8080`) is not opened unless set explicitly.
    #[pyo3(signature = (listen, tls=false, *, name=None, http2=None, http3=false, http3_listen=None, allow_http=None, allow_h2c=None, secure_redirect=None, redirect_to=None, threads=None, unix_socket_permissions=None))]
    #[allow(clippy::too_many_arguments)] // 理由: Python のキーワード引数（`[[listener]]` のキーと 1 対 1）
    fn add_listener(
        &mut self,
        listen: &str,
        tls: bool,
        name: Option<&str>,
        http2: Option<bool>,
        http3: bool,
        http3_listen: Option<&str>,
        allow_http: Option<bool>,
        allow_h2c: Option<bool>,
        secure_redirect: Option<bool>,
        redirect_to: Option<&str>,
        threads: Option<usize>,
        unix_socket_permissions: Option<&str>,
    ) {
        let mut l = if tls {
            VeilListener::tls(listen)
        } else {
            VeilListener::cleartext(listen)
        };
        if let Some(v) = name {
            l = l.name(v);
        }
        if let Some(v) = http2 {
            l = l.http2(v);
        }
        if http3 {
            l = l.http3(true);
        }
        if let Some(v) = http3_listen {
            l = l.http3_listen(v);
        }
        if let Some(v) = allow_http {
            l = l.allow_http(v);
        }
        if let Some(v) = allow_h2c {
            l = l.allow_h2c(v);
        }
        if let Some(v) = secure_redirect {
            l = l.secure_redirect(v);
        }
        if let Some(v) = redirect_to {
            l = l.redirect_to(v);
        }
        if let Some(v) = threads {
            l = l.threads(v);
        }
        if let Some(v) = unix_socket_permissions {
            l = l.unix_socket_permissions(v);
        }
        self.inner.add_listener(l);
    }

    // F-263: Python 側が読む `[api] python_*` のキー
    /// The value of `[api] <name>` in the merged configuration (`str`, `int`, `float` or `bool`), or `None`.
    fn api_setting(&self, py: Python<'_>, name: &str) -> PyResult<Option<Py<PyAny>>> {
        let text = self.inner.to_toml_string().map_err(cfg_err)?;
        let v: toml::Value = toml::from_str(&text).map_err(cfg_err)?;
        let Some(x) = v.get("api").and_then(|a| a.get(name)) else {
            return Ok(None);
        };
        Ok(Some(match x {
            toml::Value::String(s) => s.into_pyobject(py)?.into_any().unbind(),
            toml::Value::Integer(i) => i.into_pyobject(py)?.into_any().unbind(),
            toml::Value::Float(f) => f.into_pyobject(py)?.into_any().unbind(),
            toml::Value::Boolean(b) => b.into_pyobject(py)?.to_owned().into_any().unbind(),
            _ => return Ok(None),
        }))
    }

    /// Return the merged configuration as a TOML string (for debugging).
    fn to_toml(&self) -> PyResult<String> {
        self.inner.to_toml_string().map_err(cfg_err)
    }
}

/// Register an HTTP route (collected until `run`).
#[pyfunction]
#[pyo3(signature = (path, methods, handler_id, offload=true, openapi_json=None, status_code=200, stream_body=false, on_loop=false))]
#[allow(clippy::too_many_arguments)] // 理由: Python から呼ぶ登録関数で、引数はキーワード引数として渡る
fn register_route(
    path: String,
    methods: Vec<String>,
    handler_id: u64,
    offload: bool,
    openapi_json: Option<String>,
    status_code: u16,
    stream_body: bool,
    on_loop: bool,
) -> PyResult<()> {
    let openapi = parse_openapi(openapi_json)?;
    PENDING.lock().unwrap().push(PendingRoute {
        path,
        methods,
        handler_id,
        offload,
        openapi,
        status_code,
        stream_body,
        on_loop,
    });
    Ok(())
}

/// The Python entry points of one route: `Route._call`, and `Route.to_response` for return values Rust does not convert.
struct PyRoute {
    call: Py<PyAny>,
    fallback: Py<PyAny>,
    status: u16,
}

// F-210: `ctx` があればストリーミングの応答・要求本文の順次受信ができる。
/// Call the route entry point with the GIL held and build the response. `ctx` is present when running on the
/// handler pool (streaming responses and incremental request bodies are possible).
fn call_python(route: &PyRoute, req: Request, ctx: Option<&mut HandlerContext>) -> Response {
    Python::attach(|py| {
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

pub(crate) fn internal_error() -> Response {
    Response::json_bytes(Bytes::from_static(
        b"{\"detail\":\"Internal Server Error\"}",
    ))
    .with_status(500)
}

// F-263: `async def` を常駐のループで動かす経路（ワーカーは GIL を取らず、キュー + 通知で待つ）
fn make_loop_handler(route: PyRoute, handler_id: u64) -> HandlerFn {
    let route = Arc::new(route);
    Arc::new(move |req: Request| {
        let r = Arc::clone(&route);
        Box::pin(aio::call_on_loop(r, handler_id, req))
    })
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

/// Start the server and block until it stops (SIGINT, SIGTERM or `shutdown()`); the GIL is released.
///
/// `handlers` maps `handler_id` to the Python `Route` registered for it (with `_call` and `to_response`).
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
        let py_route = PyRoute {
            call: route.getattr("_call")?.unbind(),
            fallback: route.getattr("to_response")?.unbind(),
            status: r.status_code,
        };
        let func = if r.on_loop {
            make_loop_handler(py_route, r.handler_id)
        } else {
            make_handler(py_route, r.offload)
        };
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
    for (i, w) in std::mem::take(&mut *PENDING_WS.lock().unwrap())
        .into_iter()
        .enumerate()
    {
        let id = w.handler_id;
        let func: HandlerFn = if w.endpoint {
            let route = Arc::new(
                handlers
                    .get(id as usize)
                    .ok_or_else(|| PyValueError::new_err(format!("unknown handler id {}", id)))?
                    .clone_ref(py),
            );
            Arc::new(move |req: Request| {
                let route = Arc::clone(&route);
                Box::pin(async move { aio::websocket_endpoint(route, req) })
            })
        } else {
            Arc::new(move |req: Request| Box::pin(aio::websocket_on_loop(id, req)))
        };
        let meta = OperationMeta {
            operation_id: format!("py_ws_{}", id),
            raw: w.openapi,
            hidden: w.hidden,
            ..Default::default()
        };
        app = app.route_fn(
            &format!("py_ws_{}_{}", i, id),
            &["GET"],
            &w.path,
            func,
            meta,
        );
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
    py.detach(move || app.run())
        .map_err(|e| PyRuntimeError::new_err(e.to_string()))
}

/// Stop the running server gracefully.
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
    m.add_class::<aio::AioJob>()?;
    m.add_class::<aio::AioStream>()?;
    m.add_class::<aio::NativeWebSocket>()?;
    m.add_class::<aio::NativeWsSender>()?;
    m.add_function(wrap_pyfunction!(aio::aio_setup, m)?)?;
    m.add_function(wrap_pyfunction!(aio::aio_take, m)?)?;
    m.add_function(wrap_pyfunction!(register_websocket, m)?)?;
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
