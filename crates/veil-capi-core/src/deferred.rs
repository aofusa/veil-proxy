// F-291: 遅延応答（ハンドラが戻った後に別スレッド・非同期の完了関数から応答する）と、ソケットを開かずにルートを呼ぶ
// 試験の API（`veil_test_*`）。遅延応答は veil::api::bridge::HandlerContext::defer、試験は veil::api::testing を使う。
//! Deferred responses and in-process test calls.

use super::hooks::{read_sized, str_array};
use super::{
    build_app, cstr, fail, hooks, opt_cstr, out_bytes, out_none, veil_config_t, veil_response_t,
    VEIL_ERR_CLOSED, VEIL_ERR_INVALID_ARG, VEIL_ERR_RUNTIME, VEIL_HTTP_1_1, VEIL_HTTP_2,
    VEIL_HTTP_3, VEIL_OK,
};
use bytes::Bytes;
use std::ffi::{c_char, c_int};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use veil::api::bridge::Deferred;
use veil::api::testing::{TestClient, TestResponse};
use veil::api::{HttpVersion, Request, Response};

// ===================== 遅延応答 =====================

/// A response given after the handler returned (see `veil_response_defer`).
#[allow(non_camel_case_types)]
pub struct veil_deferred_t {
    deferred: Option<Deferred>,
    /// 組み立て中の応答（`veil_response_set_*` で書く。ストリーミングはできない）
    resp: veil_response_t,
}

/// Turns the current request into a deferred one: the handler returns right away (its `resp` is no longer used)
/// and the response is sent when `veil_deferred_complete` is called, from any thread. Build the response with
/// `veil_deferred_response` and the usual `veil_response_set_*` functions. NULL on a `VEIL_ROUTE_INLINE` route or
/// once the response was started (streaming or already deferred). Middleware "after" steps do not run on a
/// deferred response.
///
/// # Safety
///
/// `resp` is the response of a running handler callback.
#[no_mangle]
pub unsafe extern "C" fn veil_response_defer(resp: *mut veil_response_t) -> *mut veil_deferred_t {
    let Some(r) = resp.as_mut() else {
        fail(VEIL_ERR_INVALID_ARG, "NULL response");
        return ptr::null_mut();
    };
    let Some(ctx) = r.ctx.as_mut() else {
        fail(
            VEIL_ERR_INVALID_ARG,
            "deferred responses need a handler-pool route (not VEIL_ROUTE_INLINE)",
        );
        return ptr::null_mut();
    };
    let Some(d) = ctx.defer() else {
        fail(VEIL_ERR_INVALID_ARG, "the response was already started");
        return ptr::null_mut();
    };
    Box::into_raw(Box::new(veil_deferred_t {
        deferred: Some(d),
        resp: veil_response_t {
            resp: Response::new(200),
            ctx: ptr::null_mut(),
        },
    }))
}

/// The response being built for `d` (valid until `veil_deferred_complete` or `veil_deferred_free`).
///
/// # Safety
///
/// `d` is a valid deferred response.
#[no_mangle]
pub unsafe extern "C" fn veil_deferred_response(d: *mut veil_deferred_t) -> *mut veil_response_t {
    match d.as_mut() {
        Some(d) => &mut d.resp,
        None => ptr::null_mut(),
    }
}

/// Sends the built response and frees `d`. `VEIL_ERR_CLOSED` when the client has gone.
///
/// # Safety
///
/// `d` is a valid deferred response, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_deferred_complete(d: *mut veil_deferred_t) -> c_int {
    if d.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL deferred response");
    }
    let mut d = Box::from_raw(d);
    let resp = std::mem::replace(&mut d.resp.resp, Response::new(200));
    match d.deferred.take().map(|def| def.complete(resp)) {
        Some(true) => VEIL_OK,
        _ => fail(VEIL_ERR_CLOSED, "the client has disconnected"),
    }
}

/// 1 when the client has disconnected (the response would be dropped), 0 otherwise.
///
/// # Safety
///
/// `d` is a valid deferred response.
#[no_mangle]
pub unsafe extern "C" fn veil_deferred_is_closed(d: *const veil_deferred_t) -> c_int {
    match d.as_ref().and_then(|d| d.deferred.as_ref()) {
        Some(def) => def.is_closed() as c_int,
        None => 1,
    }
}

/// Frees `d` without completing it: the client gets 500.
///
/// # Safety
///
/// `d` is NULL or a deferred response, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_deferred_free(d: *mut veil_deferred_t) {
    if !d.is_null() {
        let mut d = Box::from_raw(d);
        if let Some(def) = d.deferred.take() {
            def.complete(Response::error(500, "Internal Server Error"));
        }
    }
}

// ===================== 試験の呼び出し =====================

/// An app built for in-process calls (see `veil_test_new`).
#[allow(non_camel_case_types)]
pub struct veil_test_t {
    client: TestClient,
    shutdown: Vec<hooks::Hook>,
}

/// Builds the app of `cfg` for in-process calls, without opening sockets: setup functions run now, then
/// `veil_test_request` calls the routes (with the middleware, plugins, validation and handler pool of a server).
/// `cfg` is consumed whether this succeeds or not. NULL on error (`veil_last_error`). Free with `veil_test_free`,
/// which runs the shutdown functions.
///
/// # Safety
///
/// `cfg` is a valid configuration handle, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_test_new(cfg: *mut veil_config_t) -> *mut veil_test_t {
    if cfg.is_null() {
        fail(VEIL_ERR_INVALID_ARG, "NULL config");
        return ptr::null_mut();
    }
    let cfg = *Box::from_raw(cfg);
    let (app, shutdown, _started) = build_app(cfg);
    match catch_unwind(AssertUnwindSafe(|| app.into_test_client())) {
        Ok(Ok(client)) => Box::into_raw(Box::new(veil_test_t { client, shutdown })),
        Ok(Err(e)) => {
            fail(VEIL_ERR_RUNTIME, e.0);
            ptr::null_mut()
        }
        Err(_) => {
            fail(VEIL_ERR_RUNTIME, "building the app panicked");
            ptr::null_mut()
        }
    }
}

/// Frees a test app and runs its shutdown functions (in reverse order of registration).
///
/// # Safety
///
/// `t` is NULL or a test app from `veil_test_new`, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_test_free(t: *mut veil_test_t) {
    if !t.is_null() {
        let t = Box::from_raw(t);
        let veil_test_t { client, shutdown } = *t;
        drop(client);
        hooks::run_shutdown(shutdown);
    }
}

/// One in-process request (zero-filled means defaults; `size` is `sizeof(veil_test_request_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_test_request_t {
    pub size: usize,
    /// The method (NULL: `GET`).
    pub method: *const c_char,
    /// The path with an optional query string (`/users/1?x=2`).
    pub target: *const c_char,
    /// Header names and values, alternating: `n_headers` pairs.
    pub headers: *const *const c_char,
    pub n_headers: usize,
    /// The body (may be NULL when `body_len` is 0).
    pub body: *const u8,
    pub body_len: usize,
    /// 0 or `VEIL_HTTP_1_1`, `VEIL_HTTP_2` or `VEIL_HTTP_3`.
    pub version: c_int,
    /// Non-zero: as if received over TLS.
    pub tls: c_int,
    /// The client address reported to the handler (NULL: `127.0.0.1`).
    pub client_ip: *const c_char,
}

/// The response of an in-process call. Free it with `veil_test_response_free`.
#[allow(non_camel_case_types)]
pub struct veil_test_response_t {
    resp: TestResponse,
}

/// Calls the app with `req` on this thread and waits for the whole response (a streamed body is collected). Do
/// not call it from a handler or a worker.
///
/// # Safety
///
/// `t` is a valid test app, `req` a valid request, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn veil_test_request(
    t: *const veil_test_t,
    req: *const veil_test_request_t,
    out: *mut *mut veil_test_response_t,
) -> c_int {
    let (Some(t), false) = (t.as_ref(), out.is_null()) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL test app or output");
    };
    *out = ptr::null_mut();
    let r = tryc!(read_sized(req, "veil_test_request_t"));
    let method = tryc!(opt_cstr(r.method)).unwrap_or("GET");
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        return fail(VEIL_ERR_INVALID_ARG, "invalid HTTP method");
    }
    let target = tryc!(cstr(r.target));
    if !target.starts_with('/') {
        return fail(VEIL_ERR_INVALID_ARG, "target must start with '/'");
    }
    let flat = tryc!(str_array(
        r.headers,
        match r.n_headers.checked_mul(2) {
            Some(n) => n,
            None => return fail(VEIL_ERR_INVALID_ARG, "too many headers"),
        },
        "headers"
    ));
    let headers: Vec<(Bytes, Bytes)> = flat
        .chunks(2)
        .map(|p| {
            (
                Bytes::from(p[0].to_ascii_lowercase()),
                Bytes::from(p[1].clone()),
            )
        })
        .collect();
    if r.body.is_null() && r.body_len > 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL body with a non-zero length");
    }
    let body = if r.body_len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(r.body, r.body_len))
    };
    let version = match r.version {
        0 | VEIL_HTTP_1_1 => HttpVersion::Http11,
        VEIL_HTTP_2 => HttpVersion::Http2,
        VEIL_HTTP_3 => HttpVersion::Http3,
        _ => return fail(VEIL_ERR_INVALID_ARG, "unknown HTTP version"),
    };
    let ip = match tryc!(opt_cstr(r.client_ip)) {
        None => std::net::IpAddr::from([127, 0, 0, 1]),
        Some(s) => match s.parse() {
            Ok(ip) => ip,
            Err(_) => return fail(VEIL_ERR_INVALID_ARG, "invalid client_ip"),
        },
    };
    let request = Request::new(
        method.to_ascii_uppercase(),
        target.to_string(),
        headers,
        body,
    )
    .with_version(version)
    .with_tls(r.tls != 0)
    .with_client_ip(ip);
    match catch_unwind(AssertUnwindSafe(|| t.client.call(request))) {
        Ok(resp) => {
            *out = Box::into_raw(Box::new(veil_test_response_t { resp }));
            VEIL_OK
        }
        Err(_) => fail(VEIL_ERR_RUNTIME, "the call panicked"),
    }
}

/// The status code (0 for NULL).
///
/// # Safety
///
/// `r` is NULL or a valid response.
#[no_mangle]
pub unsafe extern "C" fn veil_test_response_status(r: *const veil_test_response_t) -> c_int {
    r.as_ref().map(|r| r.resp.status as c_int).unwrap_or(0)
}

/// The first value of the header `name` (case-insensitive; not NUL-terminated), or NULL.
///
/// # Safety
///
/// `r` is a valid response, `name` a NUL-terminated string; `len` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_test_response_header(
    r: *const veil_test_response_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(name)) = (r.as_ref(), cstr(name)) else {
        return out_none(len);
    };
    match r.resp.header(name) {
        Some(v) => out_bytes(v, len),
        None => out_none(len),
    }
}

/// The number of response headers.
///
/// # Safety
///
/// `r` is NULL or a valid response.
#[no_mangle]
pub unsafe extern "C" fn veil_test_response_header_count(r: *const veil_test_response_t) -> usize {
    r.as_ref().map(|r| r.resp.headers.len()).unwrap_or(0)
}

/// The name and value of the `i`-th response header (not NUL-terminated; outputs may be NULL).
///
/// # Safety
///
/// `r` is a valid response; the outputs are NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_test_response_header_at(
    r: *const veil_test_response_t,
    i: usize,
    name: *mut *const c_char,
    name_len: *mut usize,
    value: *mut *const c_char,
    value_len: *mut usize,
) -> c_int {
    let Some((n, v)) = r.as_ref().and_then(|r| r.resp.headers.get(i)) else {
        return fail(VEIL_ERR_INVALID_ARG, "header index out of range");
    };
    if !name.is_null() {
        *name = out_bytes(n, name_len);
    }
    if !value.is_null() {
        *value = out_bytes(v, value_len);
    }
    VEIL_OK
}

/// The whole body (not NUL-terminated).
///
/// # Safety
///
/// `r` is a valid response; `len` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_test_response_body(
    r: *const veil_test_response_t,
    len: *mut usize,
) -> *const u8 {
    match r.as_ref() {
        Some(r) => out_bytes(&r.resp.body, len) as *const u8,
        None => out_none(len) as *const u8,
    }
}

/// The value of the trailer `name` of a streamed body (not NUL-terminated), or NULL.
///
/// # Safety
///
/// `r` is a valid response, `name` a NUL-terminated string; `len` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_test_response_trailer(
    r: *const veil_test_response_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(name)) = (r.as_ref(), cstr(name)) else {
        return out_none(len);
    };
    match r
        .resp
        .trailers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
    {
        Some((_, v)) => out_bytes(v, len),
        None => out_none(len),
    }
}

/// Frees a test response.
///
/// # Safety
///
/// `r` is NULL or a test response, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_test_response_free(r: *mut veil_test_response_t) {
    if !r.is_null() {
        drop(Box::from_raw(r));
    }
}
