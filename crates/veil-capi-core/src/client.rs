//! C API の外向きの呼び出し（F-291 のコア FFI 第 2 弾）: HTTP クライアント（`veil::api::client`）と gRPC のチャネル
//! （`veil::api::grpc::Channel`）。
//!
//! 呼び出しは veil の専用スレッド（`veil::api::client_thread`）で動かし、C のスレッドは結果を待つ（同期版）か、
//! ハンドラ用プールで呼ばれる関数で受ける（非同期版）。同期版をワーカー（INLINE のルート・ワーカー開始のフック）から
//! 呼ぶとイベントループが止まるので `VEIL_ERR_STATE` で拒否する。gRPC の呼び出しの仕組みは veil-python と共有する
//! （`veil::api::grpc::client_call`）。

use super::hooks::{read_sized, str_array};
use super::{
    cstr, fail, opt_cstr, out_bytes, out_none, tri, valid_header, UserData, VEIL_ERR_CLOSED,
    VEIL_ERR_INVALID_ARG, VEIL_ERR_STATE, VEIL_OK,
};
use bytes::Bytes;
use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::Arc;
use std::time::Duration;
use veil::api::client::{Client, ClientError, ClientResponse, ErrorKind, Http2};
use veil::api::grpc::client_call::{BlockingCall, CallStatus};
use veil::api::grpc::{Channel, Compression};

/// The request failed (URL, connection, TLS, protocol, too large a response).
pub const VEIL_ERR_CLIENT: c_int = -9;
/// The connect timeout or the request timeout expired.
pub const VEIL_ERR_TIMEOUT: c_int = -10;

macro_rules! tryn {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(_) => return ptr::null_mut(),
        }
    };
}

pub(crate) fn client_error(e: &ClientError) -> c_int {
    let code = if e.kind() == ErrorKind::Timeout {
        VEIL_ERR_TIMEOUT
    } else {
        VEIL_ERR_CLIENT
    };
    fail(code, e.to_string())
}

fn blocking_allowed() -> Result<(), c_int> {
    if veil::api::client_thread::on_event_loop() {
        return Err(fail(
            VEIL_ERR_STATE,
            "blocking calls are not allowed on a worker (VEIL_ROUTE_INLINE routes, worker-start functions); use the asynchronous form",
        ));
    }
    Ok(())
}

/// `name, value, name, value, ...`（`n` 組）をヘッダの組にする（名前は小文字。CR/LF 等は拒否）。
pub(crate) unsafe fn header_pairs(
    p: *const *const c_char,
    n: usize,
    what: &str,
) -> Result<Vec<(Bytes, Bytes)>, c_int> {
    let flat = str_array(p, n.checked_mul(2).ok_or(VEIL_ERR_INVALID_ARG)?, what)?;
    let mut out = Vec::with_capacity(n);
    for pair in flat.chunks(2) {
        if !valid_header(&pair[0], &pair[1]) {
            return Err(fail(
                VEIL_ERR_INVALID_ARG,
                format!("invalid {} entry '{}'", what, pair[0]),
            ));
        }
        out.push((
            Bytes::from(pair[0].to_ascii_lowercase()),
            Bytes::from(pair[1].clone()),
        ));
    }
    Ok(out)
}

fn millis(ms: u64) -> Option<Duration> {
    (ms > 0).then(|| Duration::from_millis(ms))
}

// ===================== HTTP クライアント =====================

/// HTTP client settings (zero-filled means defaults; `size` is `sizeof(veil_client_options_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_client_options_t {
    pub size: usize,
    /// Default request timeout in milliseconds (0: 30 seconds).
    pub timeout_ms: u64,
    /// Connect timeout in milliseconds (0: 10 seconds).
    pub connect_timeout_ms: u64,
    /// HTTP/2 (tri-state: default = ALPN over TLS and HTTP/1.1 in plain text, on = HTTP/2 only, off = HTTP/1.1).
    pub http2: c_int,
    /// A PEM file of trusted CA certificates (NULL: the system's).
    pub ca_file: *const c_char,
    /// A client certificate and its key (PEM files; both or neither).
    pub cert_file: *const c_char,
    pub key_file: *const c_char,
    /// Non-zero: accept any server certificate (tests only).
    pub insecure: c_int,
    /// The `User-Agent` header (NULL: veil's).
    pub user_agent: *const c_char,
    /// Maximum response body in bytes (0: the client's default).
    pub max_response_size: usize,
    /// Non-zero: accept `http://unix:/path:/uri` URLs.
    pub unix_sockets: c_int,
}

/// An HTTP client with a connection pool (thread-safe; share one per process).
#[allow(non_camel_case_types)]
pub struct veil_client_t {
    pub(crate) client: Client,
}

/// Creates an HTTP client (`opts` may be NULL). NULL on error (`veil_last_error`).
///
/// # Safety
///
/// `opts` is NULL or a valid `veil_client_options_t` whose strings are NUL-terminated.
#[no_mangle]
pub unsafe extern "C" fn veil_client_new(opts: *const veil_client_options_t) -> *mut veil_client_t {
    let mut b = Client::builder();
    if !opts.is_null() {
        let o = tryn!(read_sized(opts, "veil_client_options_t"));
        if let Some(d) = millis(o.timeout_ms) {
            b = b.timeout(d);
        }
        if let Some(d) = millis(o.connect_timeout_ms) {
            b = b.connect_timeout(d);
        }
        b = b.http2(match tri(o.http2) {
            None => Http2::Auto,
            Some(true) => Http2::On,
            Some(false) => Http2::Off,
        });
        if let Some(ca) = tryn!(opt_cstr(o.ca_file)) {
            b = b.ca([ca.to_string()]);
        }
        match (tryn!(opt_cstr(o.cert_file)), tryn!(opt_cstr(o.key_file))) {
            (Some(c), Some(k)) => b = b.client_cert(c, k),
            (None, None) => {}
            _ => {
                fail(VEIL_ERR_INVALID_ARG, "cert_file and key_file go together");
                return ptr::null_mut();
            }
        }
        if o.insecure != 0 {
            b = b.danger_accept_invalid_certs(true);
        }
        if let Some(ua) = tryn!(opt_cstr(o.user_agent)) {
            b = b.user_agent(ua);
        }
        if o.max_response_size > 0 {
            b = b.max_response_size(o.max_response_size);
        }
        if o.unix_sockets != 0 {
            b = b.unix_sockets(true);
        }
    }
    match b.build() {
        Ok(client) => Box::into_raw(Box::new(veil_client_t { client })),
        Err(e) => {
            client_error(&e);
            ptr::null_mut()
        }
    }
}

/// Frees a client (calls in flight keep their own reference).
///
/// # Safety
///
/// `c` is NULL or a client from `veil_client_new`, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_client_free(c: *mut veil_client_t) {
    if !c.is_null() {
        drop(Box::from_raw(c));
    }
}

/// One HTTP request (zero-filled means defaults; `size` is `sizeof(veil_client_request_t)` or 0). The library
/// copies everything during the call.
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_client_request_t {
    pub size: usize,
    /// The method (NULL: `GET`).
    pub method: *const c_char,
    /// `http://` or `https://` URL.
    pub url: *const c_char,
    /// Header names and values, alternating: `n_headers` pairs.
    pub headers: *const *const c_char,
    pub n_headers: usize,
    /// The body (may be NULL when `body_len` is 0).
    pub body: *const u8,
    pub body_len: usize,
    /// Timeout of this request in milliseconds (0: the client's).
    pub timeout_ms: u64,
}

/// A response of the HTTP client (owned by the caller; free it with `veil_client_response_free`).
#[allow(non_camel_case_types)]
pub struct veil_client_response_t {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(Bytes, Bytes)>,
    pub(crate) body: Bytes,
}

impl veil_client_response_t {
    pub(crate) fn from_response(r: ClientResponse) -> veil_client_response_t {
        veil_client_response_t {
            status: r.status(),
            headers: r.headers().to_vec(),
            body: r.into_bytes(),
        }
    }
}

/// A request copied out of C memory (sent on the client thread).
#[doc(hidden)]
#[derive(Clone)]
pub struct OwnedRequest {
    method: String,
    url: String,
    headers: Vec<(Bytes, Bytes)>,
    body: Bytes,
    timeout: Option<Duration>,
}

impl OwnedRequest {
    /// Builds a request in Rust (for the helpers kit).
    pub fn new(
        method: &str,
        url: &str,
        headers: Vec<(Bytes, Bytes)>,
        body: Bytes,
        timeout: Option<Duration>,
    ) -> OwnedRequest {
        OwnedRequest {
            method: method.to_ascii_uppercase(),
            url: url.to_string(),
            headers,
            body,
            timeout,
        }
    }

    /// Sends the request with `client`.
    pub async fn send(self, client: Client) -> Result<ClientResponse, ClientError> {
        let mut rb = client.request(&self.method, &self.url);
        for (n, v) in self.headers {
            rb = rb.header(n, v);
        }
        if !self.body.is_empty() {
            rb = rb.body(self.body);
        }
        if let Some(t) = self.timeout {
            rb = rb.timeout(t);
        }
        rb.send().await
    }
}

pub(crate) unsafe fn owned_request(
    req: *const veil_client_request_t,
) -> Result<OwnedRequest, c_int> {
    let r = read_sized(req, "veil_client_request_t")?;
    let method = opt_cstr(r.method)?.unwrap_or("GET");
    if method.is_empty() || !method.bytes().all(|b| b.is_ascii_alphabetic()) {
        return Err(fail(VEIL_ERR_INVALID_ARG, "invalid HTTP method"));
    }
    if r.body.is_null() && r.body_len > 0 {
        return Err(fail(
            VEIL_ERR_INVALID_ARG,
            "NULL body with a non-zero length",
        ));
    }
    let body = if r.body_len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(r.body, r.body_len))
    };
    Ok(OwnedRequest::new(
        method,
        cstr(r.url)?,
        header_pairs(r.headers, r.n_headers, "headers")?,
        body,
        millis(r.timeout_ms),
    ))
}

/// Sends `req` and waits for the response, written to `*out` (free it with `veil_client_response_free`).
/// `VEIL_ERR_TIMEOUT` / `VEIL_ERR_CLIENT` when it failed, `VEIL_ERR_STATE` on a worker (use
/// `veil_client_send_async` there). Any status code (including 4xx and 5xx) is a successful call.
///
/// # Safety
///
/// `c` is a valid client, `req` a valid request, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn veil_client_send(
    c: *const veil_client_t,
    req: *const veil_client_request_t,
    out: *mut *mut veil_client_response_t,
) -> c_int {
    let (Some(c), false) = (c.as_ref(), out.is_null()) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL client or output");
    };
    *out = ptr::null_mut();
    tryc!(blocking_allowed());
    let r = tryc!(owned_request(req));
    let client = c.client.clone();
    match veil::api::client_thread::run_blocking(move || r.send(client)) {
        Ok(Ok(resp)) => {
            *out = Box::into_raw(Box::new(veil_client_response_t::from_response(resp)));
            VEIL_OK
        }
        Ok(Err(e)) => client_error(&e),
        Err(e) => fail(VEIL_ERR_CLIENT, e),
    }
}

/// Called with the outcome of `veil_client_send_async` on the handler pool: `rc` is `VEIL_OK` with `resp` (owned
/// by the callee; free it with `veil_client_response_free`), or an error code with `resp` NULL and the reason in
/// `veil_last_error()`.
#[allow(non_camel_case_types)]
pub type veil_client_done_fn =
    unsafe extern "C" fn(rc: c_int, resp: *mut veil_client_response_t, user_data: *mut c_void);

/// Sends `req` without waiting; `done` is called on the handler pool when the response arrived or the request
/// failed. Works from any thread, including workers. Combine it with `veil_response_defer` to answer a request
/// from the callback without holding a pool thread.
///
/// # Safety
///
/// `c` is a valid client, `req` a valid request (copied before return); `user_data` stays valid until `done` runs
/// and may be used from another thread.
#[no_mangle]
pub unsafe extern "C" fn veil_client_send_async(
    c: *const veil_client_t,
    req: *const veil_client_request_t,
    done: Option<veil_client_done_fn>,
    user_data: *mut c_void,
) -> c_int {
    let (Some(c), Some(done)) = (c.as_ref(), done) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL client or callback");
    };
    let r = tryc!(owned_request(req));
    let client = c.client.clone();
    let ud = UserData(user_data);
    let submitted = veil::api::client_thread::run_then(
        move || r.send(client),
        move |res| {
            let ud = ud;
            // SAFETY: veil_client_send_async の契約
            unsafe {
                match res {
                    Ok(resp) => {
                        let b =
                            Box::into_raw(Box::new(veil_client_response_t::from_response(resp)));
                        done(VEIL_OK, b, ud.get());
                    }
                    Err(e) => done(client_error(&e), ptr::null_mut(), ud.get()),
                }
            }
        },
    );
    match submitted {
        Ok(()) => VEIL_OK,
        Err(e) => fail(VEIL_ERR_CLIENT, e),
    }
}

/// The status code of a response (0 for NULL).
///
/// # Safety
///
/// `r` is NULL or a valid response.
#[no_mangle]
pub unsafe extern "C" fn veil_client_response_status(r: *const veil_client_response_t) -> c_int {
    r.as_ref().map(|r| r.status as c_int).unwrap_or(0)
}

/// The first value of the response header `name` (case-insensitive; not NUL-terminated), or NULL. Valid until the
/// response is freed.
///
/// # Safety
///
/// `r` is a valid response, `name` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_client_response_header(
    r: *const veil_client_response_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(name)) = (r.as_ref(), cstr(name)) else {
        return out_none(len);
    };
    match r
        .headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
    {
        Some((_, v)) => out_bytes(v, len),
        None => out_none(len),
    }
}

/// The number of response headers.
///
/// # Safety
///
/// `r` is NULL or a valid response.
#[no_mangle]
pub unsafe extern "C" fn veil_client_response_header_count(
    r: *const veil_client_response_t,
) -> usize {
    r.as_ref().map(|r| r.headers.len()).unwrap_or(0)
}

/// The name and value of the `i`-th response header (not NUL-terminated; outputs may be NULL).
///
/// # Safety
///
/// `r` is a valid response; the outputs are NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_client_response_header_at(
    r: *const veil_client_response_t,
    i: usize,
    name: *mut *const c_char,
    name_len: *mut usize,
    value: *mut *const c_char,
    value_len: *mut usize,
) -> c_int {
    let Some((n, v)) = r.as_ref().and_then(|r| r.headers.get(i)) else {
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

/// The response body (not NUL-terminated; may contain NUL bytes). Valid until the response is freed.
///
/// # Safety
///
/// `r` is a valid response; `len` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_client_response_body(
    r: *const veil_client_response_t,
    len: *mut usize,
) -> *const u8 {
    match r.as_ref() {
        Some(r) => out_bytes(&r.body, len) as *const u8,
        None => out_none(len) as *const u8,
    }
}

/// Frees a response.
///
/// # Safety
///
/// `r` is NULL or a response from the client, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_client_response_free(r: *mut veil_client_response_t) {
    if !r.is_null() {
        drop(Box::from_raw(r));
    }
}

// ===================== gRPC のチャネル =====================

/// gRPC channel settings (zero-filled means defaults; `size` is `sizeof(veil_grpc_channel_options_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_grpc_channel_options_t {
    pub size: usize,
    /// A PEM file of trusted CA certificates (NULL: the system's).
    pub ca_file: *const c_char,
    /// A client certificate and its key (PEM files; both or neither).
    pub cert_file: *const c_char,
    pub key_file: *const c_char,
    /// Non-zero: compress request messages with gzip.
    pub gzip: c_int,
    /// Maximum received message size in bytes (0: the default).
    pub max_message_size: usize,
    /// Default deadline of calls in milliseconds (0: none).
    pub timeout_ms: u64,
}

/// A gRPC channel over HTTP/2 (`http://` = h2c, `https://` = TLS with ALPN h2; thread-safe).
#[allow(non_camel_case_types)]
pub struct veil_grpc_channel_t {
    pub(crate) channel: Channel,
}

/// Builds a channel from settings (shared with the helpers kit's balanced channel).
pub(crate) unsafe fn channel_from(
    url: &str,
    opts: *const veil_grpc_channel_options_t,
) -> Result<Channel, c_int> {
    let mut b = Client::builder().http2(Http2::On).unix_sockets(true);
    let o = if opts.is_null() {
        None
    } else {
        Some(read_sized(opts, "veil_grpc_channel_options_t")?)
    };
    if let Some(o) = &o {
        if let Some(ca) = opt_cstr(o.ca_file)? {
            b = b.ca([ca.to_string()]);
        }
        match (opt_cstr(o.cert_file)?, opt_cstr(o.key_file)?) {
            (Some(c), Some(k)) => b = b.client_cert(c, k),
            (None, None) => {}
            _ => {
                return Err(fail(
                    VEIL_ERR_INVALID_ARG,
                    "cert_file and key_file go together",
                ))
            }
        }
    }
    let client = b.build().map_err(|e| client_error(&e))?;
    let mut ch =
        Channel::with_client(&client, url).map_err(|e| fail(VEIL_ERR_INVALID_ARG, e.message()))?;
    if let Some(o) = o {
        if o.gzip != 0 {
            ch = ch.compression(Compression::Gzip);
        }
        if o.max_message_size > 0 {
            ch = ch.max_message_size(o.max_message_size);
        }
        if let Some(d) = millis(o.timeout_ms) {
            ch = ch.timeout(d);
        }
    }
    Ok(ch)
}

/// Creates a channel to `url` (`opts` may be NULL). NULL on error. The connection is made on the first call.
///
/// # Safety
///
/// `url` is a NUL-terminated string; `opts` NULL or a valid `veil_grpc_channel_options_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_channel_new(
    url: *const c_char,
    opts: *const veil_grpc_channel_options_t,
) -> *mut veil_grpc_channel_t {
    let url = tryn!(cstr(url));
    match channel_from(url, opts) {
        Ok(channel) => Box::into_raw(Box::new(veil_grpc_channel_t { channel })),
        Err(_) => ptr::null_mut(),
    }
}

/// Frees a channel (calls in flight keep their own reference).
///
/// # Safety
///
/// `ch` is NULL or a channel from `veil_grpc_channel_new`, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_channel_free(ch: *mut veil_grpc_channel_t) {
    if !ch.is_null() {
        drop(Box::from_raw(ch));
    }
}

/// Per-call settings (zero-filled means defaults; `size` is `sizeof(veil_grpc_call_options_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_grpc_call_options_t {
    pub size: usize,
    /// Deadline in milliseconds (0: the channel's).
    pub timeout_ms: u64,
    /// Metadata names and values, alternating: `n_metadata` pairs (`-bin` names carry base64 text).
    pub metadata: *const *const c_char,
    pub n_metadata: usize,
}

/// One client call (streaming or the result of a unary call). Free it with `veil_grpc_client_call_free`.
#[allow(non_camel_case_types)]
pub struct veil_grpc_client_call_t {
    call: Arc<BlockingCall>,
    /// 最後に受け取ったメッセージ（`veil_grpc_client_recv` の戻り値が指す。次の recv まで有効）
    current: Option<Bytes>,
    /// 終わった後の状態（`veil_grpc_client_finish` で入る）
    status: Option<CallStatus>,
}

impl veil_grpc_client_call_t {
    fn new(call: Arc<BlockingCall>) -> veil_grpc_client_call_t {
        veil_grpc_client_call_t {
            call,
            current: None,
            status: None,
        }
    }
}

unsafe fn start_call(
    ch: *const veil_grpc_channel_t,
    path: *const c_char,
    opts: *const veil_grpc_call_options_t,
) -> Result<BlockingCall, c_int> {
    let Some(ch) = ch.as_ref() else {
        return Err(fail(VEIL_ERR_INVALID_ARG, "NULL channel"));
    };
    let path = cstr(path)?;
    if !path.starts_with('/') {
        return Err(fail(
            VEIL_ERR_INVALID_ARG,
            "method path must look like /package.Service/Method",
        ));
    }
    let (md, timeout) = if opts.is_null() {
        (Vec::new(), None)
    } else {
        let o = read_sized(opts, "veil_grpc_call_options_t")?;
        (
            header_pairs(o.metadata, o.n_metadata, "metadata")?,
            millis(o.timeout_ms),
        )
    };
    BlockingCall::start(&ch.channel, path, md, timeout).map_err(|e| fail(VEIL_ERR_CLIENT, e))
}

/// Starts a call of `path` (`/package.Service/Method`) for any kind of method: send request messages with
/// `veil_grpc_client_send`, end them with `veil_grpc_client_close_send`, read responses with
/// `veil_grpc_client_recv` and the final status with `veil_grpc_client_finish`. NULL on error.
///
/// # Safety
///
/// `ch` is a valid channel, `path` a NUL-terminated string, `opts` NULL or a valid `veil_grpc_call_options_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_channel_call(
    ch: *const veil_grpc_channel_t,
    path: *const c_char,
    opts: *const veil_grpc_call_options_t,
) -> *mut veil_grpc_client_call_t {
    let call = tryn!(start_call(ch, path, opts));
    Box::into_raw(Box::new(veil_grpc_client_call_t::new(Arc::new(call))))
}

unsafe fn one_message(
    ch: *const veil_grpc_channel_t,
    path: *const c_char,
    msg: *const u8,
    len: usize,
    opts: *const veil_grpc_call_options_t,
) -> Result<BlockingCall, c_int> {
    if msg.is_null() && len > 0 {
        return Err(fail(
            VEIL_ERR_INVALID_ARG,
            "NULL message with a non-zero length",
        ));
    }
    let call = start_call(ch, path, opts)?;
    let m = if len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(msg, len))
    };
    call.send(m);
    call.done_sending();
    Ok(call)
}

/// A unary (or server-streaming) call with one request message; waits until the call ends and writes the
/// finished call to `*out` (read the reply with `veil_grpc_client_recv` and the status with
/// `veil_grpc_client_finish`, which no longer wait). `VEIL_OK` means the call completed, whatever its gRPC status;
/// `VEIL_ERR_STATE` on a worker.
///
/// # Safety
///
/// As for `veil_grpc_channel_call`; `msg` has `len` readable bytes; `out` is writable.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_channel_unary(
    ch: *const veil_grpc_channel_t,
    path: *const c_char,
    msg: *const u8,
    len: usize,
    opts: *const veil_grpc_call_options_t,
    out: *mut *mut veil_grpc_client_call_t,
) -> c_int {
    if out.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL output");
    }
    *out = ptr::null_mut();
    tryc!(blocking_allowed());
    let call = tryc!(one_message(ch, path, msg, len, opts));
    let st = call.status();
    let mut c = veil_grpc_client_call_t::new(Arc::new(call));
    c.status = Some(st);
    *out = Box::into_raw(Box::new(c));
    VEIL_OK
}

/// Called on the handler pool when a call of `veil_grpc_channel_unary_async` ended; the callee owns `call` (free
/// it with `veil_grpc_client_call_free`).
#[allow(non_camel_case_types)]
pub type veil_grpc_done_fn =
    unsafe extern "C" fn(call: *mut veil_grpc_client_call_t, user_data: *mut c_void);

/// The asynchronous form of `veil_grpc_channel_unary` (works from any thread, including workers).
///
/// # Safety
///
/// As for `veil_grpc_channel_unary`; `user_data` stays valid until `done` runs and may be used from another thread.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_channel_unary_async(
    ch: *const veil_grpc_channel_t,
    path: *const c_char,
    msg: *const u8,
    len: usize,
    opts: *const veil_grpc_call_options_t,
    done: Option<veil_grpc_done_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(done) = done else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL callback");
    };
    let call = Arc::new(tryc!(one_message(ch, path, msg, len, opts)));
    let ud = UserData(user_data);
    let keep = Arc::clone(&call);
    call.status_then(move |st| {
        let ud = ud;
        let mut c = veil_grpc_client_call_t::new(keep);
        c.status = Some(st);
        // SAFETY: veil_grpc_channel_unary_async の契約
        unsafe { done(Box::into_raw(Box::new(c)), ud.get()) };
    });
    VEIL_OK
}

/// Sends one request message (copied). `VEIL_ERR_CLOSED` once the call ended or sending was closed.
///
/// # Safety
///
/// `c` is a valid call; `msg` has `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_client_send(
    c: *mut veil_grpc_client_call_t,
    msg: *const u8,
    len: usize,
) -> c_int {
    let Some(c) = c.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL call");
    };
    if msg.is_null() && len > 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL message with a non-zero length");
    }
    let m = if len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(msg, len))
    };
    if c.call.send(m) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_CLOSED, "the call has ended or sending was closed")
    }
}

/// Ends the request messages (half-close).
///
/// # Safety
///
/// `c` is a valid call.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_client_close_send(c: *mut veil_grpc_client_call_t) -> c_int {
    match c.as_ref() {
        Some(c) => {
            c.call.done_sending();
            VEIL_OK
        }
        None => fail(VEIL_ERR_INVALID_ARG, "NULL call"),
    }
}

/// Waits for the next response message: 1 with `*data`/`*len` set (valid until the next receive or the free), 0
/// at the end of the responses (then read the status), negative on error (`VEIL_ERR_STATE` on a worker).
///
/// # Safety
///
/// `c` is a valid call; `data` and `len` are writable.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_client_recv(
    c: *mut veil_grpc_client_call_t,
    data: *mut *const u8,
    len: *mut usize,
) -> c_int {
    let (Some(c), false, false) = (c.as_mut(), data.is_null(), len.is_null()) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL call or output");
    };
    *data = ptr::null();
    *len = 0;
    // 終わった呼び出しの残りのメッセージは待たずに読める
    if c.status.is_none() {
        tryc!(blocking_allowed());
    }
    c.current = c.call.recv();
    match &c.current {
        Some(b) => {
            *data = b.as_ptr();
            *len = b.len();
            1
        }
        None => 0,
    }
}

/// Waits for the end of the call and returns its gRPC status code (0 = OK); negative on error (`VEIL_ERR_STATE` on
/// a worker). Unread response messages stay readable.
///
/// # Safety
///
/// `c` is a valid call.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_client_finish(c: *mut veil_grpc_client_call_t) -> c_int {
    let Some(c) = c.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL call");
    };
    if c.status.is_none() {
        tryc!(blocking_allowed());
        c.status = Some(c.call.status());
    }
    c.status.as_ref().map(|s| s.code as c_int).unwrap_or(2)
}

/// The status message after `veil_grpc_client_finish` (not NUL-terminated; empty before the end).
///
/// # Safety
///
/// `c` is a valid call; `len` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_client_status_message(
    c: *const veil_grpc_client_call_t,
    len: *mut usize,
) -> *const c_char {
    match c.as_ref().and_then(|c| c.status.as_ref()) {
        Some(s) => out_bytes(s.message.as_bytes(), len),
        None => out_none(len),
    }
}

/// The value of the response metadata `name` (headers first, then trailers after the end; not NUL-terminated),
/// or NULL.
///
/// # Safety
///
/// `c` is a valid call, `name` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_client_metadata(
    c: *mut veil_grpc_client_call_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(c), Ok(name)) = (c.as_mut(), cstr(name)) else {
        return out_none(len);
    };
    if c.status.is_none() {
        // 応答ヘッダだけ（届いていなければ空）。状態に写して指す先を残す
        let initial = c.call.initial_metadata();
        if let Some(v) = initial
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
        {
            c.current = Some(v.1.clone());
            return out_bytes(c.current.as_ref().unwrap(), len);
        }
        return out_none(len);
    }
    let s = c.status.as_ref().unwrap();
    match s
        .initial
        .iter()
        .chain(s.trailers.iter())
        .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
    {
        Some((_, v)) => out_bytes(v, len),
        None => out_none(len),
    }
}

/// Cancels the call (the status becomes CANCELLED unless it already ended).
///
/// # Safety
///
/// `c` is a valid call.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_client_cancel(c: *mut veil_grpc_client_call_t) {
    if let Some(c) = c.as_ref() {
        c.call.cancel();
    }
}

/// Frees a call (cancelling it when it has not ended).
///
/// # Safety
///
/// `c` is NULL or a call from this API, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_client_call_free(c: *mut veil_grpc_client_call_t) {
    // 終わっていない呼び出しは BlockingCall の Drop が取り消す（非同期の完了待ちが持つ参照が残れば続く）
    if !c.is_null() {
        drop(Box::from_raw(c));
    }
}

/// Wraps a finished unary call made in Rust (for the helpers kit's balanced channel).
pub(crate) fn finished_call(
    call: Arc<BlockingCall>,
    st: CallStatus,
) -> *mut veil_grpc_client_call_t {
    let mut c = veil_grpc_client_call_t::new(call);
    c.status = Some(st);
    Box::into_raw(Box::new(c))
}

/// Starts a one-message call in Rust (for the helpers kit's balanced channel).
pub(crate) unsafe fn start_one(
    ch: &Channel,
    path: &str,
    msg: Bytes,
    opts: *const veil_grpc_call_options_t,
) -> Result<BlockingCall, c_int> {
    let (md, timeout) = if opts.is_null() {
        (Vec::new(), None)
    } else {
        let o = read_sized(opts, "veil_grpc_call_options_t")?;
        (
            header_pairs(o.metadata, o.n_metadata, "metadata")?,
            millis(o.timeout_ms),
        )
    };
    let call = BlockingCall::start(ch, path, md, timeout).map_err(|e| fail(VEIL_ERR_CLIENT, e))?;
    call.send(msg);
    call.done_sending();
    Ok(call)
}

/// Whether blocking calls are allowed on this thread (for the helpers kit).
pub(crate) fn check_blocking() -> Result<(), c_int> {
    blocking_allowed()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    #[test]
    fn arguments_are_checked() {
        unsafe {
            let c = veil_client_new(ptr::null());
            assert!(!c.is_null());
            let url = CString::new("http://127.0.0.1:1/").unwrap();
            let mut req: veil_client_request_t = std::mem::zeroed();
            req.url = url.as_ptr();
            let bad = CString::new("GE T").unwrap();
            req.method = bad.as_ptr();
            let mut out = ptr::null_mut();
            assert_eq!(veil_client_send(c, &req, &mut out), VEIL_ERR_INVALID_ARG);
            req.method = ptr::null();
            req.body_len = 3;
            assert_eq!(veil_client_send(c, &req, &mut out), VEIL_ERR_INVALID_ARG);
            req.body_len = 0;
            // ヘッダの値の CR/LF は拒否
            let (n, v) = (CString::new("x").unwrap(), CString::new("a\r\nb").unwrap());
            let hs = [n.as_ptr(), v.as_ptr()];
            req.headers = hs.as_ptr();
            req.n_headers = 1;
            assert_eq!(veil_client_send(c, &req, &mut out), VEIL_ERR_INVALID_ARG);
            veil_client_free(c);

            assert!(
                veil_grpc_channel_new(CString::new("ftp://x").unwrap().as_ptr(), ptr::null())
                    .is_null()
            );
            let ch = veil_grpc_channel_new(
                CString::new("http://127.0.0.1:1").unwrap().as_ptr(),
                ptr::null(),
            );
            assert!(!ch.is_null());
            let p = CString::new("no-slash").unwrap();
            assert!(veil_grpc_channel_call(ch, p.as_ptr(), ptr::null()).is_null());
            veil_grpc_channel_free(ch);
        }
    }
}
