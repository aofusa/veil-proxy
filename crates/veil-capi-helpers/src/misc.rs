// F-291: リダイレクト・1 ファイルの応答・フォームの値・エラー応答・JSON の文字列・OAuth2 の securitySchemes。
use crate::{alloc_string, out_kept, out_none, tryc};
use kit::redirect::Redirect;
use std::ffi::{c_char, c_int};
use veil::api::{IntoResponse, Response};
use veil_capi::internal::{self, cstr, fail};
use veil_capi::{veil_request_t, veil_response_t, VEIL_ERR_INVALID_ARG, VEIL_ERR_STATE, VEIL_OK};

unsafe fn redirect_with(resp: *mut veil_response_t, status: u16, location: &str) -> c_int {
    let r = match status {
        0 | 302 => Redirect::to(location),
        301 => Redirect::moved(location),
        303 => Redirect::see_other(location),
        307 => Redirect::temporary(location),
        308 => Redirect::permanent(location),
        _ => {
            return fail(
                VEIL_ERR_INVALID_ARG,
                "redirect status must be 301, 302, 303, 307 or 308",
            )
        }
    };
    if internal::update_response(resp, |_| r.into_response()) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_INVALID_ARG, "NULL response")
    }
}

/// Sets the response to a redirect to `location` (`status` 301, 302, 303, 307 or 308; 0 is 302). Control
/// characters in `location` are dropped.
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `location` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_redirect(
    resp: *mut veil_response_t,
    status: u16,
    location: *const c_char,
) -> c_int {
    redirect_with(resp, status, tryc!(cstr(location)))
}

/// Redirects to `target` only when it stays on this site (a path such as `/a?b`, not `//host`) or its host is
/// one of `allowed_hosts` (against open redirects through `?next=` parameters). Otherwise the response is left
/// unchanged and the result is `VEIL_ERR_INVALID_ARG` (redirect to a fixed page instead).
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `target` is a NUL-terminated string; `allowed_hosts`
/// points to `n` NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_redirect_safe(
    resp: *mut veil_response_t,
    status: u16,
    target: *const c_char,
    allowed_hosts: *const *const c_char,
    n: usize,
) -> c_int {
    let target = tryc!(cstr(target));
    if allowed_hosts.is_null() && n > 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL host list with a non-zero count");
    }
    let mut hosts = Vec::with_capacity(n);
    for i in 0..n {
        hosts.push(tryc!(cstr(*allowed_hosts.add(i))));
    }
    match Redirect::safe(target, &hosts) {
        Some(_) => redirect_with(resp, status, target),
        None => fail(VEIL_ERR_INVALID_ARG, "redirect target is not allowed"),
    }
}

/// Sets the response to the file `path` with a `Content-Type` from its extension: 404 when it does not exist,
/// 403 when it cannot be read, 413 when it is larger than `max_size`. It reads the file in the callback, so it is
/// not allowed on `VEIL_ROUTE_INLINE` routes (`VEIL_ERR_STATE`). `path` must not come from the request unchecked;
/// serve directories with `veil_config_serve_dir`.
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `path` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_send_file(
    resp: *mut veil_response_t,
    path: *const c_char,
    max_size: u64,
) -> c_int {
    let path = tryc!(cstr(path));
    if !internal::on_handler_pool(resp) {
        return fail(
            VEIL_ERR_STATE,
            "veil_kit_send_file reads the file and needs a handler-pool route (not VEIL_ROUTE_INLINE)",
        );
    }
    // 理由: ハンドラ用プールのスレッド（ワーカーではない。上で確かめた）なのでブロッキングの fs を使ってよい
    #[allow(clippy::disallowed_methods)]
    let r = (|| -> std::io::Result<Option<Vec<u8>>> {
        let meta = std::fs::metadata(path)?;
        if !meta.is_file() {
            return Err(std::io::ErrorKind::NotFound.into());
        }
        if meta.len() > max_size {
            return Ok(None);
        }
        std::fs::read(path).map(Some)
    })();
    let out = match r {
        Ok(Some(data)) => {
            let mut r = Response::bytes(data);
            r.set_header("content-type", kit::serve::content_type(path));
            r
        }
        Ok(None) => Response::error(413, "Payload Too Large"),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Response::not_found(),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
            Response::error(403, "Forbidden")
        }
        Err(e) => {
            ftlog::error!("reading {} failed: {}", path, e);
            Response::error(500, "Internal Server Error")
        }
    };
    if internal::update_response(resp, |_| out) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_INVALID_ARG, "NULL response")
    }
}

/// `%XX` と `+` を復号する（壊れた `%` はそのまま）
fn form_decode(s: &[u8]) -> String {
    let hex = |b: u8| match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    };
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'+' => out.push(b' '),
            b'%' if i + 2 < s.len() => match (hex(s[i + 1]), hex(s[i + 2])) {
                (Some(h), Some(l)) => {
                    out.push(h << 4 | l);
                    i += 2;
                }
                _ => out.push(b'%'),
            },
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The first value of `name` in an `application/x-www-form-urlencoded` body (`+` and `%XX` decoded), for example
/// the `username` and `password` of an OAuth2 password-flow token request. NULL when absent. Valid until the
/// callback returns.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `name` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_form_get(
    req: *const veil_request_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(name)) = (internal::request(req), cstr(name)) else {
        return out_none(len);
    };
    for pair in r.body().split(|&b| b == b'&') {
        let (k, v) = match pair.iter().position(|&b| b == b'=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, &pair[pair.len()..]),
        };
        if form_decode(k) == name {
            return out_kept(req, form_decode(v), len);
        }
    }
    out_none(len)
}

/// Sets the response to `{"detail": "<detail>"}` with `status` (the error shape of the Rust and Python APIs).
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `detail` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_error(
    resp: *mut veil_response_t,
    status: u16,
    detail: *const c_char,
) -> c_int {
    if !(100..=599).contains(&status) {
        return fail(VEIL_ERR_INVALID_ARG, "status out of range");
    }
    let d = tryc!(cstr(detail)).to_string();
    if internal::update_response(resp, move |_| Response::error(status, &d)) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_INVALID_ARG, "NULL response")
    }
}

/// `s` as a JSON string literal with quotes and escapes (for building JSON in C). Free it with `veil_kit_free`.
///
/// # Safety
///
/// `s` is a NUL-terminated UTF-8 string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_json_string(s: *const c_char) -> *mut c_char {
    match cstr(s) {
        Ok(s) => alloc_string(serde_json::Value::from(s).to_string()),
        Err(_) => std::ptr::null_mut(),
    }
}

/// The OpenAPI security scheme of the OAuth2 password flow whose token endpoint is `token_url` (put it under
/// `components.securitySchemes`). Free it with `veil_kit_free`.
///
/// # Safety
///
/// `token_url` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_oauth2_security_scheme(token_url: *const c_char) -> *mut c_char {
    match cstr(token_url) {
        Ok(u) => alloc_string(
            kit::auth::OAuth2PasswordBearer::new(u)
                .security_scheme()
                .to_string(),
        ),
        Err(_) => std::ptr::null_mut(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn redirects() {
        let (_, r) = with_handles(request(&[], ""), |_, rs| unsafe {
            assert_eq!(veil_kit_redirect(rs, 303, c("/done").as_ptr()), VEIL_OK);
        });
        assert_eq!(
            (r.status_code(), r.header("location")),
            (303, Some(&b"/done"[..]))
        );
        let (_, r) = with_handles(request(&[], ""), |_, rs| unsafe {
            let hosts = [c("app.example.com")];
            let hp: Vec<_> = hosts.iter().map(|h| h.as_ptr()).collect();
            assert_eq!(
                veil_kit_redirect_safe(rs, 0, c("//evil.example/").as_ptr(), hp.as_ptr(), 1),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_kit_redirect_safe(rs, 0, c("https://evil.example/").as_ptr(), hp.as_ptr(), 1),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_kit_redirect(rs, 200, c("/x").as_ptr()),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_kit_redirect_safe(
                    rs,
                    0,
                    c("https://app.example.com/x").as_ptr(),
                    hp.as_ptr(),
                    1
                ),
                VEIL_OK
            );
        });
        assert_eq!(
            (r.status_code(), r.header("location")),
            (302, Some(&b"https://app.example.com/x"[..]))
        );
    }

    #[test]
    fn forms_errors_and_json() {
        let req = request(&[], "kind=form&name=al+ice&code=p%40ss&x=%zz&flag");
        let (_, r) = with_handles(req, |rq, rs| unsafe {
            let mut len = 0;
            assert_eq!(
                take(veil_kit_form_get(rq, c("name").as_ptr(), &mut len), len),
                "al ice"
            );
            assert_eq!(
                take(veil_kit_form_get(rq, c("code").as_ptr(), &mut len), len),
                "p@ss"
            );
            assert_eq!(
                take(veil_kit_form_get(rq, c("x").as_ptr(), &mut len), len),
                "%zz"
            );
            assert_eq!(
                take(veil_kit_form_get(rq, c("flag").as_ptr(), &mut len), len),
                ""
            );
            assert!(veil_kit_form_get(rq, c("nope").as_ptr(), &mut len).is_null());
            // INLINE と同じ（プールの外）では読まない
            assert_eq!(
                veil_kit_send_file(rs, c("/etc/hostname").as_ptr(), 10),
                VEIL_ERR_STATE
            );
            assert_eq!(veil_kit_error(rs, 422, c("bad \"x\"").as_ptr()), VEIL_OK);
        });
        assert_eq!(r.status_code(), 422);
        assert_eq!(&r.body()[..], br#"{"detail":"bad \"x\""}"#);
        unsafe {
            assert_eq!(
                owned(veil_kit_json_string(c("a\"b\n").as_ptr())),
                "\"a\\\"b\\n\""
            );
            assert!(owned(veil_kit_oauth2_security_scheme(c("/token").as_ptr()))
                .contains("\"tokenUrl\":\"/token\""));
        }
        assert_eq!(form_decode(b"%4"), "%4");
        assert_eq!(form_decode(b"a%2"), "a%2");
    }
}
