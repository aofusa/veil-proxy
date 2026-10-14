//! veil API サーバライブラリの Python バインディング（F-194）。
//!
//! Python 側（`python/veil/__init__.py`）がデコレータでルートを集め、`register_route` で
//! ここへ登録し、`run` で起動する。リクエストごとにルートの呼び出し口（`Route._call`）を GIL 下で
//! [`native::NativeRequest`] 1 つを渡して呼び、戻り値を Rust で応答にする（F-212）。

mod native;

use bytes::Bytes;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use std::sync::{Arc, Mutex};
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
}

static PENDING: Mutex<Vec<PendingRoute>> = Mutex::new(Vec::new());

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
#[pyo3(signature = (path, methods, handler_id, offload=true, openapi_json=None, status_code=200))]
fn register_route(
    path: String,
    methods: Vec<String>,
    handler_id: u64,
    offload: bool,
    openapi_json: Option<String>,
    status_code: u16,
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
    });
    Ok(())
}

/// 1 ルートの Python 側の呼び出し口（`Route._call` と、扱わない戻り値の変換 `Route.to_response`）。
struct PyRoute {
    call: Py<PyAny>,
    fallback: Py<PyAny>,
    status: u16,
}

/// ルートの呼び出し口を GIL 下で呼び、応答を組み立てる。
fn call_python(route: &PyRoute, req: Request) -> Response {
    Python::with_gil(|py| {
        let r = (|| -> PyResult<Response> {
            let req = Py::new(py, native::NativeRequest::new(req))?;
            let out = route.call.call1(py, (req,))?;
            native::to_response(py, out.into_bound(py), route.status, &route.fallback)
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
                match veil::runtime::offload::offload_handler(move || call_python(&r, req)).await {
                    Some(r) => r,
                    None => internal_error(),
                }
            } else {
                call_python(&r, req)
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
        app = app.route_fn(
            &format!("py_{}_{}", i, r.handler_id),
            &methods,
            &r.path,
            func,
            meta,
        );
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

#[pymodule]
fn _veil(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyConfig>()?;
    m.add_class::<native::NativeRequest>()?;
    m.add_class::<native::NativeHeaders>()?;
    m.add_function(wrap_pyfunction!(register_route, m)?)?;
    m.add_function(wrap_pyfunction!(run, m)?)?;
    m.add_function(wrap_pyfunction!(shutdown, m)?)?;
    m.add_function(wrap_pyfunction!(native::_json_fast, m)?)?;
    Ok(())
}
