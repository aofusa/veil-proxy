// F-212: 要求ごとの Python オブジェクトを減らす経路。
//! Native request, headers and response conversion objects handed to Python handlers.
//!
//! - [`NativeRequest`]: wraps the Rust `veil::api::Request` and creates Python objects only for the attributes a
//!   handler touches (`method` is an interned string; `path`, `body` and the like are built on first access and
//!   cached). It has the same attributes and methods as the Python `veil.Request`, and `isinstance(req, veil.Request)`
//!   is true because it is registered as a virtual subclass. Arbitrary attributes can be added (`__dict__`; needs
//!   abi3-py39).
//! - [`NativeHeaders`]: read-only, case-insensitive headers. `get`, `[]` and `in` match directly against the Rust
//!   header array and only create the `str` that is returned.
//! - [`to_response`]: turns a handler return value into a `Response` directly in Rust (`str`, `bytes`, `dict`, `list`,
//!   `None`, `veil.Response`). `bytes` and JSON bodies borrow the Python bytes without copying (`PyBackedBytes`).
//!   Anything else (tuples, pydantic models, ...) is passed to the Python `Route.to_response`.

use bytes::Bytes;
use pyo3::exceptions::PyKeyError;
use pyo3::prelude::*;
use pyo3::pybacked::{PyBackedBytes, PyBackedStr};
use pyo3::sync::PyOnceLock;
use pyo3::types::{PyBytes, PyDict, PyList, PyString, PyTuple};
use std::sync::atomic::{AtomicUsize, Ordering};
use veil::api::bridge::HandlerContext;
use veil::api::{Request, Response};

fn lossy(b: &[u8]) -> std::borrow::Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// The request object passed to handlers (same shape as `veil.Request`).
#[pyclass(name = "NativeRequest", module = "veil._veil", dict)]
pub struct NativeRequest {
    pub(crate) req: Request,
    path: PyOnceLock<Py<PyString>>,
    query: PyOnceLock<Py<PyString>>,
    body: PyOnceLock<Py<PyBytes>>,
    params: PyOnceLock<Py<PyDict>>,
    client_ip: PyOnceLock<Py<PyString>>,
    query_params: PyOnceLock<Py<PyAny>>,
    // F-210: ハンドラ用プールで動く間の文脈（`*mut HandlerContext`。呼び出しの外・INLINE は 0）
    pub(crate) ctx: AtomicUsize,
}

impl NativeRequest {
    pub fn new(req: Request) -> NativeRequest {
        NativeRequest {
            req,
            path: PyOnceLock::new(),
            query: PyOnceLock::new(),
            body: PyOnceLock::new(),
            params: PyOnceLock::new(),
            client_ip: PyOnceLock::new(),
            query_params: PyOnceLock::new(),
            ctx: AtomicUsize::new(0),
        }
    }

    /// Next chunk of the request body (only on streaming-body routes, during the handler call; waits with the GIL released).
    fn read_chunk(&self, py: Python<'_>) -> Option<Bytes> {
        let p = self.ctx.load(Ordering::Acquire);
        if p == 0 {
            return None;
        }
        // SAFETY: ctx はこの要求を処理しているプールのスレッドの `call_blocking` の閉包が持つ `HandlerContext` で、
        // 呼び出しが終わる前に 0 へ戻す（`lib.rs::call_python`）。同じスレッドからしか使わない。
        let ctx = unsafe { &mut *(p as *mut HandlerContext) };
        if !ctx.has_body_stream() {
            return None;
        }
        py.detach(|| ctx.read_body())
    }
}

/// Python-side helpers (`_query_params` and `_dumps` of the `veil` module, and `json.loads`).
struct Helpers {
    query_params: Py<PyAny>,
    dumps: Py<PyAny>,
    loads: Py<PyAny>,
    response_type: Py<PyAny>,
    // F-210: `veil.StreamingResponse`・`veil._stream_of`（ジェネレータ等をストリーミングの応答にする）
    streaming_type: Py<PyAny>,
    stream_of: Py<PyAny>,
}

static HELPERS: PyOnceLock<Helpers> = PyOnceLock::new();

fn helpers(py: Python<'_>) -> PyResult<&'static Helpers> {
    HELPERS.get_or_try_init(py, || {
        let veil = py.import("veil")?;
        Ok(Helpers {
            query_params: veil.getattr("_query_params")?.unbind(),
            dumps: veil.getattr("_dumps")?.unbind(),
            loads: py.import("json")?.getattr("loads")?.unbind(),
            response_type: veil.getattr("Response")?.unbind(),
            streaming_type: veil.getattr("StreamingResponse")?.unbind(),
            stream_of: veil.getattr("_stream_of")?.unbind(),
        })
    })
}

#[pymethods]
impl NativeRequest {
    /// Build a request directly (tests); takes the same arguments as `veil.Request`.
    #[new]
    #[pyo3(signature = (method="GET", path="/", query="", headers=None, body=None, params=None, client_ip=""))]
    fn py_new(
        method: &str,
        path: &str,
        query: &str,
        headers: Option<Vec<(String, String)>>,
        body: Option<Vec<u8>>,
        params: Option<std::collections::HashMap<String, String>>,
        client_ip: &str,
    ) -> NativeRequest {
        let target = if query.is_empty() {
            path.to_string()
        } else {
            format!("{}?{}", path, query)
        };
        let hdrs = headers
            .unwrap_or_default()
            .into_iter()
            .map(|(n, v)| (Bytes::from(n), Bytes::from(v)))
            .collect();
        let mut req = Request::new(method.to_string(), target, hdrs, body.unwrap_or_default());
        if let Ok(ip) = client_ip.parse() {
            req = req.with_client_ip(ip);
        }
        let params: Vec<(std::sync::Arc<str>, Bytes)> = params
            .unwrap_or_default()
            .into_iter()
            .map(|(k, v)| (std::sync::Arc::from(k), Bytes::from(v)))
            .collect();
        if !params.is_empty() {
            req = req.with_params(params);
        }
        NativeRequest::new(req)
    }

    /// HTTP method, upper-case (e.g. `GET`).
    #[getter]
    fn method<'py>(&self, py: Python<'py>) -> Bound<'py, PyString> {
        match self.req.method() {
            "GET" => pyo3::intern!(py, "GET").clone(),
            "POST" => pyo3::intern!(py, "POST").clone(),
            "PUT" => pyo3::intern!(py, "PUT").clone(),
            "DELETE" => pyo3::intern!(py, "DELETE").clone(),
            "PATCH" => pyo3::intern!(py, "PATCH").clone(),
            "HEAD" => pyo3::intern!(py, "HEAD").clone(),
            "OPTIONS" => pyo3::intern!(py, "OPTIONS").clone(),
            m => PyString::new(py, m),
        }
    }

    /// Request path without the query string.
    #[getter]
    fn path(&self, py: Python<'_>) -> Py<PyString> {
        self.path
            .get_or_init(py, || PyString::new(py, self.req.path()).unbind())
            .clone_ref(py)
    }

    /// Raw query string without the leading `?`.
    #[getter]
    fn query(&self, py: Python<'_>) -> Py<PyString> {
        self.query
            .get_or_init(py, || PyString::new(py, self.req.query()).unbind())
            .clone_ref(py)
    }

    /// Whole request body as `bytes`.
    #[getter]
    fn body(&self, py: Python<'_>) -> Py<PyBytes> {
        self.body
            .get_or_init(py, || PyBytes::new(py, self.req.body()).unbind())
            .clone_ref(py)
    }

    /// Client IP address as a string.
    #[getter]
    fn client_ip(&self, py: Python<'_>) -> Py<PyString> {
        self.client_ip
            .get_or_init(py, || {
                PyString::new(py, &self.req.client_ip().to_string()).unbind()
            })
            .clone_ref(py)
    }

    /// Path parameters as a dict (`{name}` in the route pattern -> value).
    #[getter]
    fn params(&self, py: Python<'_>) -> PyResult<Py<PyDict>> {
        if let Some(d) = self.params.get(py) {
            return Ok(d.clone_ref(py));
        }
        let d = PyDict::new(py);
        for (name, _) in self.req.params().iter() {
            if let Some(v) = self.req.param(name) {
                d.set_item(&**name, v.as_ref())?;
            }
        }
        let d = d.unbind();
        let _ = self.params.set(py, d.clone_ref(py));
        Ok(d)
    }

    /// Request headers (case-insensitive, read-only).
    #[getter]
    fn headers(slf: Bound<'_, Self>) -> NativeHeaders {
        NativeHeaders { req: slf.unbind() }
    }

    /// Query parameters (the first value of a repeated key), interpreted like `veil.Request.query_params`.
    #[getter]
    fn query_params(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        if let Some(q) = self.query_params.get(py) {
            return Ok(q.clone_ref(py));
        }
        let q = helpers(py)?.query_params.call1(py, (self.query(py),))?;
        let _ = self.query_params.set(py, q.clone_ref(py));
        Ok(q)
    }

    /// Iterate over the body chunks as they arrive (`stream_body=True` routes; other routes yield the whole body once).
    fn stream(slf: Bound<'_, Self>) -> NativeBodyIter {
        NativeBodyIter {
            req: slf.unbind(),
            whole_done: false,
        }
    }

    /// Parse the body as JSON.
    fn json(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        helpers(py)?.loads.call1(py, (self.body(py),))
    }

    /// Body decoded as UTF-8 (lossy).
    fn text(&self) -> String {
        lossy(self.req.body()).into_owned()
    }

    /// Debug representation.
    fn __repr__(&self) -> String {
        format!("<veil.Request {} {}>", self.req.method(), self.req.path())
    }
}

/// Iterator returned by `NativeRequest.stream()`.
#[pyclass(name = "NativeBodyIter", module = "veil._veil")]
pub struct NativeBodyIter {
    req: Py<NativeRequest>,
    whole_done: bool,
}

#[pymethods]
impl NativeBodyIter {
    /// Return the iterator itself.
    fn __iter__(slf: PyRef<'_, Self>) -> PyRef<'_, Self> {
        slf
    }

    /// Next body chunk as `bytes`; raises `StopIteration` at the end.
    fn __next__(&mut self, py: Python<'_>) -> Option<Py<PyBytes>> {
        let r = self.req.borrow(py);
        let streaming = {
            let p = r.ctx.load(Ordering::Acquire);
            // SAFETY: read_chunk と同じ
            p != 0 && unsafe { (*(p as *mut HandlerContext)).has_body_stream() }
        };
        if streaming {
            return r.read_chunk(py).map(|b| PyBytes::new(py, &b).unbind());
        }
        if self.whole_done || r.req.body().is_empty() {
            return None;
        }
        self.whole_done = true;
        Some(r.body(py))
    }
}

/// Case-insensitive read-only request headers (same shape as `veil.Headers`).
#[pyclass(name = "NativeHeaders", module = "veil._veil")]
pub struct NativeHeaders {
    req: Py<NativeRequest>,
}

impl NativeHeaders {
    fn with<R>(&self, py: Python<'_>, f: impl FnOnce(&[(Bytes, Bytes)]) -> R) -> R {
        let r = self.req.borrow(py);
        f(r.req.headers())
    }
}

#[pymethods]
impl NativeHeaders {
    /// First value of header `name` (case-insensitive), or `default` (None).
    #[pyo3(signature = (name, default=None))]
    fn get(&self, py: Python<'_>, name: &str, default: Option<Py<PyAny>>) -> Py<PyAny> {
        let found = self.with(py, |h| {
            h.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
                .map(|(_, v)| PyString::new(py, &lossy(v)).into_any().unbind())
        });
        found.unwrap_or_else(|| default.unwrap_or_else(|| py.None()))
    }

    /// All values of header `name`, in arrival order.
    fn getlist(&self, py: Python<'_>, name: &str) -> Vec<String> {
        self.with(py, |h| {
            h.iter()
                .filter(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
                .map(|(_, v)| lossy(v).into_owned())
                .collect()
        })
    }

    /// First value of header `name`; raises `KeyError` if absent.
    fn __getitem__(&self, py: Python<'_>, name: &str) -> PyResult<String> {
        self.with(py, |h| {
            h.iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
                .map(|(_, v)| lossy(v).into_owned())
        })
        .ok_or_else(|| PyKeyError::new_err(name.to_string()))
    }

    /// True if header `name` is present (case-insensitive).
    fn __contains__(&self, py: Python<'_>, name: &str) -> bool {
        self.with(py, |h| {
            h.iter()
                .any(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
        })
    }

    /// Number of header lines (duplicates counted).
    fn __len__(&self, py: Python<'_>) -> usize {
        self.with(py, |h| h.len())
    }

    /// Distinct header names (first spelling seen).
    fn keys(&self, py: Python<'_>) -> Vec<String> {
        self.with(py, |h| {
            let mut out: Vec<String> = Vec::new();
            for (n, _) in h {
                let n = lossy(n);
                if !out.iter().any(|o| o.eq_ignore_ascii_case(&n)) {
                    out.push(n.into_owned());
                }
            }
            out
        })
    }

    /// Iterate over distinct header names.
    fn __iter__(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let keys = PyList::new(py, self.keys(py))?;
        Ok(keys.call_method0("__iter__")?.unbind())
    }

    /// All `(name, value)` pairs in arrival order.
    fn items(&self, py: Python<'_>) -> Vec<(String, String)> {
        self.with(py, |h| {
            h.iter()
                .map(|(n, v)| (lossy(n).into_owned(), lossy(v).into_owned()))
                .collect()
        })
    }

    /// Debug representation.
    fn __repr__(&self, py: Python<'_>) -> String {
        format!("Headers({:?})", self.items(py))
    }
}

/// Well-known content types as static byte strings (not allocated per request).
const CT_TEXT: &[u8] = b"text/plain; charset=utf-8";
const CT_JSON: &[u8] = b"application/json";
const CT_OCTET: &[u8] = b"application/octet-stream";

fn backed(b: &Bound<'_, PyAny>) -> PyResult<Bytes> {
    Ok(Bytes::from_owner(b.extract::<PyBackedBytes>()?))
}

// F-212: `fallback` は Python の `Route.to_response`（扱わない型用）。
/// Convert a handler return value into a response; `fallback` (Python `Route.to_response`) handles unsupported types.
pub fn to_response(
    py: Python<'_>,
    result: Bound<'_, PyAny>,
    status: u16,
    fallback: &Py<PyAny>,
    ctx: Option<&mut HandlerContext>,
) -> PyResult<Response> {
    let ok = |ct: &'static [u8], body: Bytes| {
        Response::new(status)
            .with_header(Bytes::from_static(b"content-type"), Bytes::from_static(ct))
            .with_body(body)
    };
    if result.is_none() {
        return Ok(Response::new(if status == 200 { 204 } else { status }));
    }
    if let Ok(s) = result.cast::<PyString>() {
        let s: PyBackedStr = s.extract()?;
        return Ok(ok(CT_TEXT, Bytes::from_owner(PyStrOwner(s))));
    }
    if result.is_instance_of::<PyBytes>() {
        return Ok(ok(CT_OCTET, backed(&result)?));
    }
    if result.is_instance_of::<PyDict>() || result.is_instance_of::<PyList>() {
        // 文字列・整数・真偽値・None・dict・list・tuple だけなら Rust で直接 JSON にする（Python の `_dumps` と
        // 同じバイト列）。それ以外（float・他の型・str 以外のキー等）が 1 つでもあれば `_dumps` に任せる。
        let mut out = Vec::with_capacity(64);
        if json_fast(&result, &mut out, 0) {
            return Ok(ok(CT_JSON, Bytes::from(out)));
        }
        let body = helpers(py)?.dumps.call1(py, (result,))?;
        return Ok(ok(CT_JSON, backed(body.bind(py))?));
    }
    let h = helpers(py)?;
    // ストリーミング（`StreamingResponse`・ジェネレータ・非同期ジェネレータ。F-210）
    if result.is_instance(h.streaming_type.bind(py))? {
        return stream_response(py, &result, ctx);
    }
    let resp = if result.is_instance(h.response_type.bind(py))? {
        result
    } else if let Some(sr) = {
        let s = h.stream_of.call1(py, (result.clone(), status))?;
        (!s.is_none(py)).then(|| s.into_bound(py))
    } {
        return stream_response(py, &sr, ctx);
    } else {
        // タプル・bytearray・pydantic のモデル等は Python の変換規則に任せる
        fallback.call1(py, (result,))?.into_bound(py)
    };
    response_object(&resp)
}

/// Write the same output as `json.dumps(obj, separators=(",", ":"), ensure_ascii=False)`.
///
/// Returns `false` for unsupported types (floats, whose spelling may differ from Python's repr), non-`str` keys,
/// nesting that is too deep, or strings that cannot be written (lone surrogates); the caller then falls back to
/// Python's `_dumps`.
fn json_fast(v: &Bound<'_, PyAny>, out: &mut Vec<u8>, depth: usize) -> bool {
    use pyo3::types::{PyBool, PyInt};
    if depth > 64 {
        return false;
    }
    if v.is_none() {
        out.extend_from_slice(b"null");
        return true;
    }
    // bool は int の派生型なので先に見る
    if let Ok(b) = v.cast_exact::<PyBool>() {
        out.extend_from_slice(if b.is_true() { b"true" } else { b"false" });
        return true;
    }
    if let Ok(s) = v.cast_exact::<PyString>() {
        return match s.to_cow() {
            Ok(s) => serde_json::to_writer(&mut *out, &*s).is_ok(),
            Err(_) => false,
        };
    }
    if let Ok(i) = v.cast_exact::<PyInt>() {
        return match i.extract::<i64>() {
            Ok(n) => {
                out.extend_from_slice(itoa::Buffer::new().format(n).as_bytes());
                true
            }
            Err(_) => false, // 64 ビットを超える整数は Python に任せる
        };
    }
    if let Ok(d) = v.cast_exact::<PyDict>() {
        out.push(b'{');
        for (i, (k, val)) in d.iter().enumerate() {
            let Ok(k) = k.cast_exact::<PyString>() else {
                return false;
            };
            let Ok(k) = k.to_cow() else { return false };
            if i > 0 {
                out.push(b',');
            }
            if serde_json::to_writer(&mut *out, &*k).is_err() {
                return false;
            }
            out.push(b':');
            if !json_fast(&val, out, depth + 1) {
                return false;
            }
        }
        out.push(b'}');
        return true;
    }
    if let Ok(l) = v.cast_exact::<PyList>() {
        out.push(b'[');
        for (i, item) in l.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            if !json_fast(&item, out, depth + 1) {
                return false;
            }
        }
        out.push(b']');
        return true;
    }
    if let Ok(t) = v.cast_exact::<PyTuple>() {
        out.push(b'[');
        for (i, item) in t.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            if !json_fast(&item, out, depth + 1) {
                return false;
            }
        }
        out.push(b']');
        return true;
    }
    false
}

/// Test helper: the Rust JSON encoding of `v`, or `None` if unsupported.
#[pyfunction]
pub fn _json_fast(v: Bound<'_, PyAny>) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    json_fast(&v, &mut out, 0).then_some(out)
}

/// Send a `veil.StreamingResponse`. On the handler pool the body is sent chunk by chunk (writing with the GIL
/// released); for `offload=False` routes all chunks are collected into a single body.
fn stream_response(
    py: Python<'_>,
    sr: &Bound<'_, PyAny>,
    ctx: Option<&mut HandlerContext>,
) -> PyResult<Response> {
    let mut head = response_object_head(sr)?;
    let it = sr.call_method0(pyo3::intern!(py, "_iter"))?;
    let Some(ctx) = ctx else {
        let mut body = Vec::new();
        for item in it.try_iter()? {
            body.extend_from_slice(&to_chunk(item?)?);
        }
        return Ok(head.with_body(body));
    };
    let trailers = sr.getattr(pyo3::intern!(py, "trailers")).ok();
    let Some(writer) = ctx.start_stream(std::mem::replace(&mut head, Response::new(200))) else {
        return Ok(Response::new(500));
    };
    let mut closed = false;
    for item in it.try_iter()? {
        let b = to_chunk(item?)?;
        if b.is_empty() {
            continue;
        }
        if !py.detach(|| writer.write(b)) {
            closed = true;
            break;
        }
    }
    if closed {
        // クライアントが切断した: ジェネレータを閉じる（finally 節を走らせる）
        let _ = it.call_method0(pyo3::intern!(py, "close"));
        return Ok(Response::new(200));
    }
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
    if t.is_empty() {
        drop(writer);
    } else {
        py.detach(|| writer.finish_with_trailers(t));
    }
    Ok(Response::new(200))
}

/// Turn a chunk into `Bytes` (`bytes` is not copied, `str` is UTF-8, `bytearray` and `memoryview` are copied).
fn to_chunk(item: Bound<'_, PyAny>) -> PyResult<Bytes> {
    if let Ok(s) = item.cast::<PyString>() {
        let s: PyBackedStr = s.extract()?;
        return Ok(Bytes::from_owner(PyStrOwner(s)));
    }
    if item.is_instance_of::<PyBytes>() {
        return backed(&item);
    }
    let v: Vec<u8> = item.extract()?;
    Ok(Bytes::from(v))
}

/// Read only `status` and `headers` (the head of a streaming response).
fn response_object_head(resp: &Bound<'_, PyAny>) -> PyResult<Response> {
    let status: u16 = resp
        .getattr(pyo3::intern!(resp.py(), "status"))?
        .extract()?;
    let mut r = Response::new(status);
    let headers = resp.getattr(pyo3::intern!(resp.py(), "headers"))?;
    for item in headers.try_iter()? {
        let item = item?;
        let t = item.cast::<PyTuple>()?;
        let n: PyBackedStr = t.get_item(0)?.extract()?;
        let v: PyBackedStr = t.get_item(1)?.extract()?;
        r = r.with_header(known_header(&n), Bytes::copy_from_slice(v.as_bytes()));
    }
    Ok(r)
}

/// Turn a Python `veil.Response` (`status`, `headers`, `body`) into a response.
fn response_object(resp: &Bound<'_, PyAny>) -> PyResult<Response> {
    let status: u16 = resp
        .getattr(pyo3::intern!(resp.py(), "status"))?
        .extract()?;
    let body = resp.getattr(pyo3::intern!(resp.py(), "body"))?;
    let mut r = Response::new(status).with_body(backed(&body)?);
    let headers = resp.getattr(pyo3::intern!(resp.py(), "headers"))?;
    for item in headers.try_iter()? {
        let item = item?;
        let t = item.cast::<PyTuple>()?;
        let n: PyBackedStr = t.get_item(0)?.extract()?;
        let v: PyBackedStr = t.get_item(1)?.extract()?;
        r = r.with_header(known_header(&n), Bytes::copy_from_slice(v.as_bytes()));
    }
    Ok(r)
}

/// Map common header names to static byte strings (avoids copying the name).
fn known_header(n: &str) -> Bytes {
    match n {
        "content-type" => Bytes::from_static(b"content-type"),
        "Content-Type" => Bytes::from_static(b"Content-Type"),
        "location" => Bytes::from_static(b"location"),
        "cache-control" => Bytes::from_static(b"cache-control"),
        _ => Bytes::copy_from_slice(n.as_bytes()),
    }
}

/// Lets `Bytes` own a `PyBackedStr` (as UTF-8 bytes).
struct PyStrOwner(PyBackedStr);

impl AsRef<[u8]> for PyStrOwner {
    fn as_ref(&self) -> &[u8] {
        self.0.as_bytes()
    }
}
