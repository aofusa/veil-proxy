//! veil API サーバライブラリの Python バインディング（F-194）。
//!
//! Python 側（`python/veil/__init__.py`）がデコレータでルートを集め、`register_route` で
//! ここへ登録し、`run` で起動する。リクエストごとに Python の dispatcher を GIL 下で呼ぶ。

use bytes::Bytes;
use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict, PyList};
use std::sync::{Arc, Mutex};
use veil::api::{App, Config as VeilConfig, HandlerFn, OperationMeta, Request, Response};

/// `register_route` で登録された、起動前のルート。
struct PendingRoute {
    path: String,
    methods: Vec<String>,
    handler_id: u64,
    offload: bool,
    openapi: Option<serde_json::Value>,
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

    /// `[server].listen_cleartext`（平文 HTTP/1.1 + h2c のリスナー。ライブラリ既定は `0.0.0.0:8080`）。
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
#[pyo3(signature = (path, methods, handler_id, offload=true, openapi_json=None))]
fn register_route(
    path: String,
    methods: Vec<String>,
    handler_id: u64,
    offload: bool,
    openapi_json: Option<String>,
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
    });
    Ok(())
}

fn lossy(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// Python の dispatcher を GIL 下で呼び、応答を組み立てる。
fn call_python(dispatcher: &Py<PyAny>, handler_id: u64, req: &Request) -> Response {
    Python::with_gil(|py| {
        let r = (|| -> PyResult<Response> {
            let headers = PyList::empty(py);
            for (n, v) in req.headers() {
                headers.append((lossy(n), lossy(v)))?;
            }
            let params = PyDict::new(py);
            for (name, _) in req.params().iter() {
                if let Some(v) = req.param(name) {
                    params.set_item(&**name, v.as_ref())?;
                }
            }
            let args = (
                handler_id,
                req.method(),
                req.path(),
                req.query(),
                headers,
                PyBytes::new(py, req.body()),
                params,
                req.client_ip().to_string(),
            );
            let out = dispatcher.call1(py, args)?;
            let (status, hdrs, body): (u16, Vec<(String, String)>, Vec<u8>) = out.extract(py)?;
            let mut resp = Response::new(status).with_body(Bytes::from(body));
            for (n, v) in hdrs {
                resp = resp.with_header(Bytes::from(n), Bytes::from(v));
            }
            Ok(resp)
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

fn make_handler(dispatcher: Py<PyAny>, handler_id: u64, offload: bool) -> HandlerFn {
    let dispatcher = Arc::new(dispatcher);
    Arc::new(move |req: Request| {
        let d = Arc::clone(&dispatcher);
        Box::pin(async move {
            if offload {
                // GIL を握る呼び出しはイベントループ（ワーカー）を止めないようハンドラ用プールへ退避する。
                match veil::runtime::offload::offload_handler(move || {
                    call_python(&d, handler_id, &req)
                })
                .await
                {
                    Some(r) => r,
                    None => internal_error(),
                }
            } else {
                call_python(&d, handler_id, &req)
            }
        })
    })
}

/// サーバを起動し、停止（SIGINT/SIGTERM・`shutdown()`）までブロックする。GIL は解放する。
#[pyfunction]
fn run(py: Python<'_>, config: PyConfig, dispatcher: Py<PyAny>) -> PyResult<()> {
    let routes = std::mem::take(&mut *PENDING.lock().unwrap());
    let mut app = App::new().config(config.inner).without_macro_routes();
    for (i, r) in routes.into_iter().enumerate() {
        let methods: Vec<&str> = r.methods.iter().map(|s| s.as_str()).collect();
        let func = make_handler(dispatcher.clone_ref(py), r.handler_id, r.offload);
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
    drop(dispatcher);
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
    m.add_function(wrap_pyfunction!(register_route, m)?)?;
    m.add_function(wrap_pyfunction!(run, m)?)?;
    m.add_function(wrap_pyfunction!(shutdown, m)?)?;
    Ok(())
}
