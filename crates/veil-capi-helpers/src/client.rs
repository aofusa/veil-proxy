// F-291: HTTP クライアントの補助（Rust の kit の client::get_json・post_json・retry の C 版）。送る仕組みはコアの
// veil_client_*（veil の専用スレッド）。retry は Rust の kit の関数をそのまま使う。
//! HTTP client helpers: JSON requests and retries.

use crate::{tryc, UserData};
use bytes::Bytes;
use kit::client::{ClientError, ClientResponse, Retry};
use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::time::Duration;
use veil_capi::internal::{self, cstr, fail, read_sized, OwnedRequest};
use veil_capi::{
    veil_client_done_fn, veil_client_request_t, veil_client_response_t, veil_client_t,
    VEIL_ERR_INVALID_ARG, VEIL_OK,
};

/// The response status was not 2xx (`veil_kit_client_get_json`, `veil_kit_client_post_json`); the response is
/// still returned in `*out`.
pub const VEIL_KIT_ERR_STATUS: c_int = -21;
/// The response body is not JSON; the response is still returned in `*out`.
pub const VEIL_KIT_ERR_JSON: c_int = -22;

/// 2xx と JSON を確かめる（違えば応答を残したまま誤りを返す）。
fn check_json(r: &ClientResponse) -> c_int {
    if !(200..300).contains(&r.status()) {
        return fail(VEIL_KIT_ERR_STATUS, format!("HTTP status {}", r.status()));
    }
    if let Err(e) = serde_json::from_slice::<serde::de::IgnoredAny>(r.bytes()) {
        return fail(
            VEIL_KIT_ERR_JSON,
            format!("the response is not JSON: {}", e),
        );
    }
    VEIL_OK
}

unsafe fn send_json(
    c: *const veil_client_t,
    method: &str,
    url: *const c_char,
    body: Option<Bytes>,
    out: *mut *mut veil_client_response_t,
) -> c_int {
    if out.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL output");
    }
    *out = ptr::null_mut();
    let Some(client) = internal::client(c) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL client");
    };
    tryc!(internal::check_blocking());
    let url = tryc!(cstr(url));
    let mut headers = vec![(
        Bytes::from_static(b"accept"),
        Bytes::from_static(b"application/json"),
    )];
    let has_body = body.is_some();
    if has_body {
        headers.push((
            Bytes::from_static(b"content-type"),
            Bytes::from_static(b"application/json"),
        ));
    }
    let r = OwnedRequest::new(method, url, headers, body.unwrap_or_default(), None);
    match veil::api::client_thread::run_blocking(move || r.send(client)) {
        Ok(Ok(resp)) => {
            let rc = check_json(&resp);
            *out = internal::client_response(resp);
            rc
        }
        Ok(Err(e)) => internal::client_error(&e),
        Err(e) => fail(veil_capi::VEIL_ERR_CLIENT, e),
    }
}

/// `GET url` with `Accept: application/json`; waits. `VEIL_OK` when the status is 2xx and the body is JSON
/// (parse it with your JSON library); `VEIL_KIT_ERR_STATUS` / `VEIL_KIT_ERR_JSON` with the response still in
/// `*out` (free it); the errors of `veil_client_send` otherwise.
///
/// # Safety
///
/// `c` is a valid client, `url` a NUL-terminated string, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_client_get_json(
    c: *const veil_client_t,
    url: *const c_char,
    out: *mut *mut veil_client_response_t,
) -> c_int {
    send_json(c, "GET", url, None, out)
}

/// `POST url` with the JSON text `json` (checked before sending) and `Accept: application/json`; the results are
/// those of `veil_kit_client_get_json`.
///
/// # Safety
///
/// As for `veil_kit_client_get_json`; `json` has `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_client_post_json(
    c: *const veil_client_t,
    url: *const c_char,
    json: *const c_char,
    len: usize,
    out: *mut *mut veil_client_response_t,
) -> c_int {
    if json.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL JSON body");
    }
    let body = std::slice::from_raw_parts(json as *const u8, len);
    if let Err(e) = serde_json::from_slice::<serde::de::IgnoredAny>(body) {
        return fail(VEIL_ERR_INVALID_ARG, format!("the body is not JSON: {}", e));
    }
    send_json(c, "POST", url, Some(Bytes::copy_from_slice(body)), out)
}

/// Retry policy (zero-filled means the defaults of the Rust kit: 3 attempts, 100 ms doubling up to 2 s, retry
/// 502/503/504; `size` is `sizeof(veil_kit_retry_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_kit_retry_t {
    pub size: usize,
    /// Total attempts (0: 3).
    pub attempts: u32,
    /// Delay before the second attempt in milliseconds (0: 100); doubled each time.
    pub backoff_ms: u64,
    /// Upper bound of the delay in milliseconds (0: 2000).
    pub max_backoff_ms: u64,
    /// Retry 502, 503 and 504 answers (tri-state: default and on = yes, off = no).
    pub retry_gateway_errors: c_int,
}

unsafe fn policy(p: *const veil_kit_retry_t) -> Result<Retry, c_int> {
    let mut r = Retry::default();
    if p.is_null() {
        return Ok(r);
    }
    let o = read_sized(p, "veil_kit_retry_t")?;
    if o.attempts > 0 {
        r.attempts = o.attempts;
    }
    if o.backoff_ms > 0 {
        r.backoff = Duration::from_millis(o.backoff_ms);
    }
    if o.max_backoff_ms > 0 {
        r.max_backoff = Duration::from_millis(o.max_backoff_ms);
    }
    if o.retry_gateway_errors < 0 {
        r.retry_gateway_errors = false;
    }
    Ok(r)
}

async fn send_retry(
    client: veil::api::client::Client,
    req: OwnedRequest,
    policy: Retry,
) -> Result<ClientResponse, ClientError> {
    kit::client::retry(policy, || req.clone().send(client.clone())).await
}

/// Sends `req` like `veil_client_send`, retrying connection errors, timeouts and (by default) 502/503/504 with
/// exponential backoff (`policy` may be NULL). Only for requests that are safe to repeat. Waits;
/// `VEIL_ERR_STATE` on a worker.
///
/// # Safety
///
/// `c` is a valid client, `req` a valid request, `policy` NULL or a valid `veil_kit_retry_t`, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_client_send_retry(
    c: *const veil_client_t,
    req: *const veil_client_request_t,
    policy_p: *const veil_kit_retry_t,
    out: *mut *mut veil_client_response_t,
) -> c_int {
    if out.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL output");
    }
    *out = ptr::null_mut();
    let Some(client) = internal::client(c) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL client");
    };
    tryc!(internal::check_blocking());
    let r = tryc!(internal::client_request(req));
    let p = tryc!(policy(policy_p));
    match veil::api::client_thread::run_blocking(move || send_retry(client, r, p)) {
        Ok(Ok(resp)) => {
            *out = internal::client_response(resp);
            VEIL_OK
        }
        Ok(Err(e)) => internal::client_error(&e),
        Err(e) => fail(veil_capi::VEIL_ERR_CLIENT, e),
    }
}

/// The asynchronous form of `veil_kit_client_send_retry` (`done` as for `veil_client_send_async`; works on
/// workers too).
///
/// # Safety
///
/// As for `veil_client_send_async`; `policy` NULL or a valid `veil_kit_retry_t` (copied before return).
#[no_mangle]
pub unsafe extern "C" fn veil_kit_client_send_retry_async(
    c: *const veil_client_t,
    req: *const veil_client_request_t,
    policy_p: *const veil_kit_retry_t,
    done: Option<veil_client_done_fn>,
    user_data: *mut c_void,
) -> c_int {
    let (Some(client), Some(done)) = (internal::client(c), done) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL client or callback");
    };
    let r = tryc!(internal::client_request(req));
    let p = tryc!(policy(policy_p));
    let ud = UserData(user_data);
    let submitted = veil::api::client_thread::run_then(
        move || send_retry(client, r, p),
        move |res| {
            let ud = ud;
            // SAFETY: veil_kit_client_send_retry_async の契約
            unsafe {
                match res {
                    Ok(resp) => done(VEIL_OK, internal::client_response(resp), ud.get()),
                    Err(e) => done(internal::client_error(&e), ptr::null_mut(), ud.get()),
                }
            }
        },
    );
    match submitted {
        Ok(()) => VEIL_OK,
        Err(e) => fail(veil_capi::VEIL_ERR_CLIENT, e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::c;

    #[test]
    fn arguments_and_policy() {
        unsafe {
            let p = policy(ptr::null()).unwrap();
            assert_eq!(p.attempts, 3);
            let o = veil_kit_retry_t {
                size: std::mem::size_of::<veil_kit_retry_t>(),
                attempts: 5,
                backoff_ms: 10,
                max_backoff_ms: 0,
                retry_gateway_errors: veil_capi::VEIL_OFF,
            };
            let p = policy(&o).unwrap();
            assert_eq!(
                (p.attempts, p.backoff, p.retry_gateway_errors),
                (5, Duration::from_millis(10), false)
            );
            let cl = veil_capi::veil_client_new(ptr::null());
            let mut out = ptr::null_mut();
            let bad = c("{not json");
            assert_eq!(
                veil_kit_client_post_json(
                    cl,
                    c("http://127.0.0.1:1/").as_ptr(),
                    bad.as_ptr(),
                    9,
                    &mut out
                ),
                VEIL_ERR_INVALID_ARG
            );
            veil_capi::veil_client_free(cl);
        }
    }
}
