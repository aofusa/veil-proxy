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
