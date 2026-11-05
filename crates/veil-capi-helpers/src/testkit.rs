// F-291: 試験の補助（Rust の App::into_test_client・Python の TestClient に当たる）。コアの veil_test_* の上に
// Cookie の入れ物（Set-Cookie を覚えて次の要求に付ける）・既定のヘッダ・GET と JSON の POST の近道を足す。
//! A test client with a cookie jar on top of `veil_test_*`.

use crate::tryc;
use std::ffi::{c_char, c_int, CString};
use std::ptr;
use veil_capi::internal::{cstr, fail, read_sized};
use veil_capi::{
    veil_config_t, veil_test_free, veil_test_new, veil_test_request, veil_test_request_t,
    veil_test_response_header_at, veil_test_response_header_count, veil_test_response_t,
    veil_test_t, VEIL_ERR_INVALID_ARG, VEIL_OK,
};

/// A test client: an in-process app plus cookies and default headers.
#[allow(non_camel_case_types)]
pub struct veil_kit_test_t {
    app: *mut veil_test_t,
    cookies: Vec<(String, String)>,
    headers: Vec<(String, String)>,
}

// SAFETY: app は veil_test_t（中身の TestClient はスレッドを問わない）。&mut で使うので同時には使われない
unsafe impl Send for veil_kit_test_t {}

/// Builds the app of `cfg` for in-process calls (as `veil_test_new`; `cfg` is consumed). NULL on error.
///
/// # Safety
///
/// `cfg` is a valid configuration handle, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_new(cfg: *mut veil_config_t) -> *mut veil_kit_test_t {
    let app = veil_test_new(cfg);
    if app.is_null() {
        return ptr::null_mut();
    }
    Box::into_raw(Box::new(veil_kit_test_t {
        app,
        cookies: Vec::new(),
        headers: Vec::new(),
    }))
}

/// Frees the client and runs the app's shutdown functions.
///
/// # Safety
///
/// `t` is NULL or from `veil_kit_test_new`, not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_free(t: *mut veil_kit_test_t) {
    if !t.is_null() {
        let t = Box::from_raw(t);
        veil_test_free(t.app);
    }
}

/// Adds a header sent with every request (a later value for the same name replaces it).
///
/// # Safety
///
/// `t` is a valid client; `name` and `value` are NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_header(
    t: *mut veil_kit_test_t,
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    let Some(t) = t.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL test client");
    };
    let (n, v) = (tryc!(cstr(name)).to_ascii_lowercase(), tryc!(cstr(value)));
    if !veil_capi::internal::valid_header(&n, v) {
        return fail(VEIL_ERR_INVALID_ARG, "invalid header");
    }
    t.headers.retain(|(k, _)| *k != n);
    t.headers.push((n, v.to_string()));
    VEIL_OK
}

/// Sets a cookie in the jar (`value` NULL removes it).
///
/// # Safety
///
/// `t` is a valid client; `name` is a NUL-terminated string, `value` NULL or one.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_set_cookie(
    t: *mut veil_kit_test_t,
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    let Some(t) = t.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL test client");
    };
    let n = tryc!(cstr(name)).to_string();
    let v = tryc!(veil_capi::internal::opt_cstr(value));
    if n.is_empty()
        || n.contains(['=', ';', ' '])
        || v.is_some_and(|v| v.contains([';', '\r', '\n']))
    {
        return fail(VEIL_ERR_INVALID_ARG, "invalid cookie");
    }
    t.cookies.retain(|(k, _)| *k != n);
    if let Some(v) = v {
        t.cookies.push((n, v.to_string()));
    }
    VEIL_OK
}

/// The value of a cookie in the jar (not NUL-terminated), or NULL. Valid until the jar changes.
///
/// # Safety
///
/// `t` is a valid client, `name` a NUL-terminated string; `len` NULL or writable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_cookie(
    t: *const veil_kit_test_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(t), Ok(name)) = (t.as_ref(), cstr(name)) else {
        return crate::out_none(len);
    };
    match t.cookies.iter().find(|(k, _)| k == name) {
        Some((_, v)) => crate::out_bytes(v.as_bytes(), len),
        None => crate::out_none(len),
    }
}

/// Empties the cookie jar.
///
/// # Safety
///
/// `t` is a valid client.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_clear_cookies(t: *mut veil_kit_test_t) {
    if let Some(t) = t.as_mut() {
        t.cookies.clear();
    }
}

/// `Set-Cookie` を入れ物に写す（`Max-Age=0` か過去の `Expires` 相当の空値は削除）。
fn store_cookie(jar: &mut Vec<(String, String)>, header: &str) {
    let mut parts = header.split(';');
    let Some((name, value)) = parts.next().and_then(|p| p.split_once('=')) else {
        return;
    };
    let (name, value) = (name.trim().to_string(), value.trim().to_string());
    let removed = parts.any(|a| {
        let a = a.trim();
        a.len() >= 8
            && a[..8].eq_ignore_ascii_case("max-age=")
            && a[8..].trim().parse::<i64>().is_ok_and(|n| n <= 0)
    });
    jar.retain(|(k, _)| *k != name);
    if !removed {
        jar.push((name, value));
    }
}

/// Sends `req` with the default headers and the cookies of the jar (headers given in `req` come after them), then
/// stores the response's `Set-Cookie` headers in the jar. `*out` as for `veil_test_request`.
///
/// # Safety
///
/// `t` is a valid client, `req` a valid `veil_test_request_t`, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_request(
    t: *mut veil_kit_test_t,
    req: *const veil_test_request_t,
    out: *mut *mut veil_test_response_t,
) -> c_int {
    let Some(t) = t.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL test client");
    };
    let mut r = tryc!(read_sized(req, "veil_test_request_t"));
    // 既定のヘッダ・Cookie・要求のヘッダの順に並べ直す
    let mut owned: Vec<CString> = Vec::new();
    let push = |owned: &mut Vec<CString>, s: &str| -> Result<(), c_int> {
        owned.push(
            CString::new(s)
                .map_err(|_| fail(VEIL_ERR_INVALID_ARG, "header contains a NUL byte"))?,
        );
        Ok(())
    };
    for (n, v) in &t.headers {
        tryc!(push(&mut owned, n));
        tryc!(push(&mut owned, v));
    }
    if !t.cookies.is_empty() {
        let jar: Vec<String> = t
            .cookies
            .iter()
            .map(|(k, v)| format!("{}={}", k, v))
            .collect();
        tryc!(push(&mut owned, "cookie"));
        tryc!(push(&mut owned, &jar.join("; ")));
    }
    let given = tryc!(veil_capi::internal::str_array(
        r.headers,
        match r.n_headers.checked_mul(2) {
            Some(n) => n,
            None => return fail(VEIL_ERR_INVALID_ARG, "too many headers"),
        },
        "headers"
    ));
    for s in &given {
        tryc!(push(&mut owned, s));
    }
    let ptrs: Vec<*const c_char> = owned.iter().map(|s| s.as_ptr()).collect();
    r.size = std::mem::size_of::<veil_test_request_t>();
    r.headers = if ptrs.is_empty() {
        ptr::null()
    } else {
        ptrs.as_ptr()
    };
    r.n_headers = ptrs.len() / 2;
    let rc = veil_test_request(t.app, &r, out);
    if rc != VEIL_OK {
        return rc;
    }
    let resp = *out;
    for i in 0..veil_test_response_header_count(resp) {
        let (mut n, mut nl, mut v, mut vl) = (ptr::null(), 0usize, ptr::null(), 0usize);
        if veil_test_response_header_at(resp, i, &mut n, &mut nl, &mut v, &mut vl) != VEIL_OK {
            continue;
        }
        let name = std::slice::from_raw_parts(n as *const u8, nl);
        if name.eq_ignore_ascii_case(b"set-cookie") {
            let value = String::from_utf8_lossy(std::slice::from_raw_parts(v as *const u8, vl));
            store_cookie(&mut t.cookies, &value);
        }
    }
    VEIL_OK
}

/// `GET target` (see `veil_kit_test_request`).
///
/// # Safety
///
/// `t` is a valid client, `target` a NUL-terminated string, `out` writable.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_get(
    t: *mut veil_kit_test_t,
    target: *const c_char,
    out: *mut *mut veil_test_response_t,
) -> c_int {
    let mut r: veil_test_request_t = std::mem::zeroed();
    r.size = std::mem::size_of::<veil_test_request_t>();
    r.target = target;
    veil_kit_test_request(t, &r, out)
}

/// `POST target` with the JSON text `json` and `Content-Type: application/json` (see `veil_kit_test_request`).
///
/// # Safety
///
/// As for `veil_kit_test_get`; `json` has `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_test_post_json(
    t: *mut veil_kit_test_t,
    target: *const c_char,
    json: *const c_char,
    len: usize,
    out: *mut *mut veil_test_response_t,
) -> c_int {
    let hs = [c"content-type".as_ptr(), c"application/json".as_ptr()];
    let mut r: veil_test_request_t = std::mem::zeroed();
    r.size = std::mem::size_of::<veil_test_request_t>();
    r.method = c"POST".as_ptr();
    r.target = target;
    r.headers = hs.as_ptr();
    r.n_headers = 1;
    r.body = json as *const u8;
    r.body_len = len;
    veil_kit_test_request(t, &r, out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cookie_jar() {
        let mut jar = Vec::new();
        store_cookie(&mut jar, "a=1; Path=/; HttpOnly");
        store_cookie(&mut jar, "b=2");
        store_cookie(&mut jar, "a=3");
        assert_eq!(
            jar,
            vec![("b".into(), "2".into()), ("a".into(), "3".into())]
        );
        store_cookie(&mut jar, "b=; Max-Age=0");
        assert_eq!(jar, vec![("a".to_string(), "3".to_string())]);
    }
}
