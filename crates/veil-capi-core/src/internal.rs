// F-291: veil-capi-helpers（libveil_kit）が C の不透明型の中身に触るための口。C には公開しない（#[no_mangle] が無い）。
//! Access to the opaque C types for `veil-capi-helpers` (not part of the C API).
#![doc(hidden)]

use super::{veil_config_t, veil_request_t, veil_response_t};
use std::ffi::{c_char, c_int};
use veil::api::{App, Request, Response};

pub use super::{
    VEIL_ERR_CLOSED, VEIL_ERR_CONFIG, VEIL_ERR_FORM, VEIL_ERR_INVALID_ARG, VEIL_ERR_RUNTIME,
    VEIL_ERR_STATE, VEIL_ERR_TOO_LARGE, VEIL_ERR_UTF8, VEIL_OK,
};

/// The request behind a handle passed to a callback.
///
/// # Safety
///
/// `req` is NULL or a handle passed to the current callback.
pub unsafe fn request<'a>(req: *const veil_request_t) -> Option<&'a Request> {
    req.as_ref().map(|r| &r.req)
}

/// The response behind a handle passed to a callback.
///
/// # Safety
///
/// `resp` is NULL or a handle passed to the current callback.
pub unsafe fn response<'a>(resp: *const veil_response_t) -> Option<&'a Response> {
    resp.as_ref().map(|r| &r.resp)
}

/// Replaces the response with `f(response)`; `false` when `resp` is NULL.
///
/// # Safety
///
/// `resp` is NULL or a handle passed to the current callback.
pub unsafe fn update_response(
    resp: *mut veil_response_t,
    f: impl FnOnce(Response) -> Response,
) -> bool {
    match resp.as_mut() {
        Some(r) => {
            let cur = std::mem::replace(&mut r.resp, Response::new(200));
            r.resp = f(cur);
            true
        }
        None => false,
    }
}

/// Keeps `s` alive until the callback returns and gives its pointer and length.
///
/// # Safety
///
/// `req` is a handle passed to the current callback.
pub unsafe fn keep(req: *const veil_request_t, s: String) -> Option<(*const c_char, usize)> {
    req.as_ref().map(|r| r.keep(s))
}

/// Adds a step that `veil_run` applies to the `App` (a plugin of `veil-proxy-kit`, for example); `false` when
/// `cfg` is NULL.
///
/// # Safety
///
/// `cfg` is NULL or a valid configuration handle.
pub unsafe fn push_plugin(
    cfg: *mut veil_config_t,
    f: impl FnOnce(App) -> App + Send + 'static,
) -> bool {
    match cfg.as_mut() {
        Some(c) => {
            c.extra.push(Box::new(f));
            true
        }
        None => false,
    }
}

/// Records `msg` for `veil_last_error` and returns `code`.
pub fn fail(code: c_int, msg: impl AsRef<str>) -> c_int {
    super::fail(code, msg)
}

/// A NUL-terminated C string as `&str` (records the error and returns its code otherwise).
///
/// # Safety
///
/// `p` is NULL or a NUL-terminated string that lives for `'a`.
pub unsafe fn cstr<'a>(p: *const c_char) -> Result<&'a str, c_int> {
    super::cstr(p)
}

/// `None` for NULL, otherwise like [`cstr`].
///
/// # Safety
///
/// Same as [`cstr`].
pub unsafe fn opt_cstr<'a>(p: *const c_char) -> Result<Option<&'a str>, c_int> {
    super::opt_cstr(p)
}

/// Whether `name: value` may be sent as a header (no control characters such as CR and LF).
pub fn valid_header(name: &str, value: &str) -> bool {
    super::valid_header(name, value)
}

/// Reads a structure that starts with `size_t size` (see `veil_listener_t`).
///
/// # Safety
///
/// `p` is NULL or points to at least `size` readable bytes; `T` is `#[repr(C)]`, starts with `usize` and has only
/// pointers and integers (all-zero is a valid value).
pub unsafe fn read_sized<T: Copy>(p: *const T, what: &str) -> Result<T, c_int> {
    super::hooks::read_sized(p, what)
}

/// Whether the callback that received `resp` runs on the handler pool (`false` for `VEIL_ROUTE_INLINE` routes,
/// which run on a worker thread and must not block).
///
/// # Safety
///
/// `resp` is NULL or a handle passed to the current callback.
pub unsafe fn on_handler_pool(resp: *const veil_response_t) -> bool {
    resp.as_ref().is_some_and(|r| !r.ctx.is_null())
}

/// Wraps `req` in handles as for an inline callback, runs `f` and returns its result with the response (for the
/// unit tests of `veil-capi-helpers`).
pub fn with_test_handles<R>(
    req: Request,
    f: impl FnOnce(*const veil_request_t, *mut veil_response_t) -> R,
) -> (R, Response) {
    let wreq = veil_request_t::new(req);
    let mut wresp = veil_response_t {
        resp: Response::new(200),
        ctx: std::ptr::null_mut(),
    };
    let r = f(&wreq, &mut wresp);
    (r, wresp.resp)
}

// ===================== F-291: クライアント・gRPC のチャネル（helpers の client・grpc Balanced 用） =====================

pub use super::client::OwnedRequest;
use super::client::{
    veil_client_request_t, veil_client_response_t, veil_client_t, veil_grpc_call_options_t,
    veil_grpc_channel_t, veil_grpc_client_call_t,
};
use bytes::Bytes;
use veil::api::client::{Client, ClientError, ClientResponse};
use veil::api::grpc::client_call::{BlockingCall, CallStatus};
use veil::api::grpc::Channel;

/// The Rust client behind a handle.
///
/// # Safety
///
/// `c` is NULL or a valid client.
pub unsafe fn client(c: *const veil_client_t) -> Option<Client> {
    c.as_ref().map(|c| c.client.clone())
}

/// Copies a C request (records the error and returns its code on failure).
///
/// # Safety
///
/// `req` is a valid `veil_client_request_t`.
pub unsafe fn client_request(req: *const veil_client_request_t) -> Result<OwnedRequest, c_int> {
    super::client::owned_request(req)
}

/// A response handle for C (freed with `veil_client_response_free`).
pub fn client_response(r: ClientResponse) -> *mut veil_client_response_t {
    Box::into_raw(Box::new(veil_client_response_t::from_response(r)))
}

/// Records a client error and returns `VEIL_ERR_TIMEOUT` or `VEIL_ERR_CLIENT`.
pub fn client_error(e: &ClientError) -> c_int {
    super::client::client_error(e)
}

/// `Err(VEIL_ERR_STATE)` (with the reason recorded) on a thread that runs an event loop.
pub fn check_blocking() -> Result<(), c_int> {
    super::client::check_blocking()
}

/// Header pairs from `name, value, ...` (`n` pairs; names lower-cased, control characters rejected).
///
/// # Safety
///
/// `p` is NULL with `n == 0` or points to `2 * n` NUL-terminated strings.
pub unsafe fn header_pairs(
    p: *const *const c_char,
    n: usize,
    what: &str,
) -> Result<Vec<(Bytes, Bytes)>, c_int> {
    super::client::header_pairs(p, n, what)
}

/// The Rust channel behind a handle.
///
/// # Safety
///
/// `ch` is NULL or a valid channel.
pub unsafe fn grpc_channel(ch: *const veil_grpc_channel_t) -> Option<Channel> {
    ch.as_ref().map(|c| c.channel.clone())
}

/// Starts a call with one request message (the messages are closed).
///
/// # Safety
///
/// `opts` is NULL or a valid `veil_grpc_call_options_t`.
pub unsafe fn grpc_start_one(
    ch: &Channel,
    path: &str,
    msg: Bytes,
    opts: *const veil_grpc_call_options_t,
) -> Result<BlockingCall, c_int> {
    super::client::start_one(ch, path, msg, opts)
}

/// A call handle for C of a finished call (freed with `veil_grpc_client_call_free`).
pub fn grpc_finished(
    call: std::sync::Arc<BlockingCall>,
    st: CallStatus,
) -> *mut veil_grpc_client_call_t {
    super::client::finished_call(call, st)
}

/// `n` NUL-terminated strings as owned strings (records the error and returns its code otherwise).
///
/// # Safety
///
/// `p` is NULL with `n == 0` or points to `n` NUL-terminated strings.
pub unsafe fn str_array(
    p: *const *const c_char,
    n: usize,
    what: &str,
) -> Result<Vec<String>, c_int> {
    super::hooks::str_array(p, n, what)
}
