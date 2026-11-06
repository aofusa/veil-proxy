// F-291: 認証・認可（veil_kit::auth）。主体（Principal）は Rust の kit と同じ型で要求の extensions に置くので、
// Rust のプラグインと C のハンドラ・ミドルウェアが同じ主体を見る。ガードは C のミドルウェアの鎖に入れる
// （C の authenticate より後に Rust 側で検査して先に断ってしまわないように）。
use crate::{alloc_string, ct_eq, out_bytes, out_kept, out_none, tryc};
use kit::auth::{BasicAuth, BearerToken, Principal};
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::Arc;
use veil::api::Response;
use veil_capi::internal::{self, cstr, fail, opt_cstr};
use veil_capi::{
    veil_config_on_shutdown, veil_config_t, veil_register_middleware, veil_request_t,
    veil_response_t, VEIL_ERR_INVALID_ARG, VEIL_OK,
};

/// The user name and password of `Authorization: Basic`: 1 when present, 0 when absent or malformed.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; the output pointers may be NULL.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_basic_auth(
    req: *const veil_request_t,
    user: *mut *const c_char,
    user_len: *mut usize,
    password: *mut *const c_char,
    password_len: *mut usize,
) -> c_int {
    let Some(r) = internal::request(req) else {
        return 0;
    };
    let Ok(b) = BasicAuth::from_headers(r) else {
        return 0;
    };
    let u = out_kept(req, b.user, user_len);
    let p = out_kept(req, b.password, password_len);
    if !user.is_null() {
        *user = u;
    }
    if !password.is_null() {
        *password = p;
    }
    1
}

/// Whether the request's Basic credentials are `user` / `password` (compared in constant time; 0 when absent).
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `user` and `password` are NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_basic_matches(
    req: *const veil_request_t,
    user: *const c_char,
    password: *const c_char,
) -> c_int {
    let (Some(r), Ok(u), Ok(p)) = (internal::request(req), cstr(user), cstr(password)) else {
        return 0;
    };
    match BasicAuth::from_headers(r) {
        Ok(b) => b.matches(u, p) as c_int,
        Err(_) => 0,
    }
}

/// Sets the response to `401 Unauthorized` with `WWW-Authenticate: <challenge>` (for example
/// `Basic realm="admin"`; NULL is `Bearer`).
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `challenge` is NULL or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_unauthorized(
    resp: *mut veil_response_t,
    challenge: *const c_char,
) -> c_int {
    let ch = tryc!(opt_cstr(challenge)).unwrap_or("Bearer");
    if !internal::valid_header("www-authenticate", ch) {
        return fail(VEIL_ERR_INVALID_ARG, "invalid challenge");
    }
    let ch = ch.to_string();
    if internal::update_response(resp, move |_| {
        Response::error(401, "Unauthorized").with_header("www-authenticate", ch)
    }) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_INVALID_ARG, "NULL response")
    }
}

/// The token of `Authorization: Bearer <token>`, or NULL. Not NUL-terminated; valid until the callback returns.
///
/// # Safety
///
/// `req` is a handle passed to the current callback.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_bearer(
    req: *const veil_request_t,
    len: *mut usize,
) -> *const c_char {
    match internal::request(req).map(BearerToken::from_headers) {
        Some(Ok(t)) => out_kept(req, t.0, len),
        _ => out_none(len),
    }
}

/// The value of the API key header `header` (NULL is `X-API-Key`), or NULL when absent or empty.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `header` is NULL or a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_api_key(
    req: *const veil_request_t,
    header: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(h)) = (internal::request(req), opt_cstr(header)) else {
        return out_none(len);
    };
    let h = h.unwrap_or("x-api-key");
    match r
        .headers()
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(h.as_bytes()))
    {
        Some((_, v)) if !v.is_empty() => out_bytes(v, len),
        _ => out_none(len),
    }
}

/// Whether the API key header `header` (NULL is `X-API-Key`) is one of `keys` (each compared in constant time).
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `keys` points to `n` NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_api_key_matches(
    req: *const veil_request_t,
    header: *const c_char,
    keys: *const *const c_char,
    n: usize,
) -> c_int {
    let mut len = 0usize;
    let p = veil_kit_api_key(req, header, &mut len);
    if p.is_null() || (keys.is_null() && n > 0) {
        return 0;
    }
    let got = std::slice::from_raw_parts(p as *const u8, len);
    let mut ok = false;
    for i in 0..n {
        let k = *keys.add(i);
        if !k.is_null() {
            ok |= ct_eq(got, CStr::from_ptr(k).to_bytes());
        }
    }
    ok as c_int
}

// ===================== 主体 =====================

/// An authenticated principal (the Rust kit's `veil_kit::auth::Principal`).
#[allow(non_camel_case_types)]
pub type veil_kit_principal_t = Principal;

/// A new principal with subject `subject` (add scopes and roles, then give it to `veil_kit_set_principal`, or
/// free it with `veil_kit_principal_free`).
///
/// # Safety
///
/// `subject` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_new(
    subject: *const c_char,
) -> *mut veil_kit_principal_t {
    match cstr(subject) {
        Ok(s) => Box::into_raw(Box::new(Principal::new(s))),
        Err(_) => std::ptr::null_mut(),
    }
}

/// Adds a scope.
///
/// # Safety
///
/// `p` is a principal from `veil_kit_principal_new`; `scope` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_add_scope(
    p: *mut veil_kit_principal_t,
    scope: *const c_char,
) -> c_int {
    let Some(p) = p.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL principal");
    };
    p.scopes.push(tryc!(cstr(scope)).to_string());
    VEIL_OK
}

/// Adds a role.
///
/// # Safety
///
/// `p` is a principal from `veil_kit_principal_new`; `role` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_add_role(
    p: *mut veil_kit_principal_t,
    role: *const c_char,
) -> c_int {
    let Some(p) = p.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL principal");
    };
    p.roles.push(tryc!(cstr(role)).to_string());
    VEIL_OK
}

/// Sets the extra attributes (a JSON value, for example the claims of a token).
///
/// # Safety
///
/// `p` is a principal from `veil_kit_principal_new`; `json` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_set_attributes(
    p: *mut veil_kit_principal_t,
    json: *const c_char,
) -> c_int {
    let Some(p) = p.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL principal");
    };
    match serde_json::from_str(tryc!(cstr(json))) {
        Ok(v) => {
            p.attributes = v;
            VEIL_OK
        }
        Err(e) => fail(VEIL_ERR_INVALID_ARG, format!("invalid JSON: {}", e)),
    }
}

/// Frees a principal that was not given to `veil_kit_set_principal` (`NULL` does nothing).
///
/// # Safety
///
/// `p` is NULL or a principal from `veil_kit_principal_new` that has not been freed or given away.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_free(p: *mut veil_kit_principal_t) {
    if !p.is_null() {
        drop(Box::from_raw(p));
    }
}

/// Makes `p` the principal of the request (takes ownership of `p`). Call it from a middleware's `before` function
/// (see `veil_register_middleware`); later middleware, the handler and `veil_kit_require_scope` see it.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `p` is a principal from `veil_kit_principal_new`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_set_principal(
    req: *const veil_request_t,
    p: *mut veil_kit_principal_t,
) -> c_int {
    let Some(r) = internal::request(req) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL request");
    };
    if p.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL principal");
    }
    r.extensions().insert_arc(Arc::new(*Box::from_raw(p)));
    VEIL_OK
}

/// The principal of the request (set by `veil_kit_set_principal`, `veil_kit_jwt_authenticate` or a Rust plugin),
/// or NULL. Valid until the callback returns or the principal is replaced; do not free it.
///
/// # Safety
///
/// `req` is a handle passed to the current callback.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal(
    req: *const veil_request_t,
) -> *const veil_kit_principal_t {
    match internal::request(req).and_then(|r| r.extensions().get::<Principal>()) {
        // 要求の extensions が Arc を持ち続ける（置き換えられるまで）
        Some(p) => Arc::as_ptr(&p),
        None => std::ptr::null(),
    }
}

/// The subject of a principal (not NUL-terminated).
///
/// # Safety
///
/// `p` is a valid principal.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_subject(
    p: *const veil_kit_principal_t,
    len: *mut usize,
) -> *const c_char {
    match p.as_ref() {
        Some(p) => out_bytes(p.subject.as_bytes(), len),
        None => out_none(len),
    }
}

/// 1 when the principal has the scope.
///
/// # Safety
///
/// `p` is a valid principal; `scope` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_has_scope(
    p: *const veil_kit_principal_t,
    scope: *const c_char,
) -> c_int {
    match (p.as_ref(), cstr(scope)) {
        (Some(p), Ok(s)) => p.has_scope(s) as c_int,
        _ => 0,
    }
}

/// 1 when the principal has the role.
///
/// # Safety
///
/// `p` is a valid principal; `role` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_has_role(
    p: *const veil_kit_principal_t,
    role: *const c_char,
) -> c_int {
    match (p.as_ref(), cstr(role)) {
        (Some(p), Ok(s)) => p.has_role(s) as c_int,
        _ => 0,
    }
}

/// The attributes of a principal as JSON (`null` when none). Free the string with `veil_kit_free`.
///
/// # Safety
///
/// `p` is a valid principal.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_principal_attributes(
    p: *const veil_kit_principal_t,
) -> *mut c_char {
    match p.as_ref() {
        Some(p) => alloc_string(p.attributes.to_string()),
        None => std::ptr::null_mut(),
    }
}

// ===================== ガード =====================

/// `veil_kit_require_scope` の状態（`veil_run` の後の停止の関数で解放する）
struct Require {
    scope: Option<String>,
}

unsafe extern "C" fn require_before(
    req: *const veil_request_t,
    resp: *mut veil_response_t,
    ud: *mut c_void,
) -> c_int {
    let g = &*(ud as *const Require);
    let p = internal::request(req).and_then(|r| r.extensions().get::<Principal>());
    let deny = match (&p, &g.scope) {
        (None, _) => {
            Some(Response::error(401, "Unauthorized").with_header("www-authenticate", "Bearer"))
        }
        (Some(p), Some(s)) if !p.has_scope(s) => Some(Response::error(403, "Forbidden")),
        _ => None,
    };
    match deny {
        Some(r) => {
            internal::update_response(resp, |_| r);
            veil_capi::VEIL_MW_STOP
        }
        None => veil_capi::VEIL_MW_CONTINUE,
    }
}

/// `Box::into_raw` した状態を停止後に解放する（veil_config_on_shutdown に渡す）
pub(crate) unsafe extern "C" fn drop_box<T>(p: *mut c_void) {
    drop(Box::from_raw(p as *mut T));
}

/// 状態を持つ C のミドルウェアを登録し、状態は停止後に解放する。
pub(crate) unsafe fn register_guard<T>(
    cfg: *mut veil_config_t,
    prefix: *const c_char,
    before: veil_capi::veil_before_fn,
    state: T,
) -> c_int {
    let ud = Box::into_raw(Box::new(state)) as *mut c_void;
    let rc = veil_register_middleware(cfg, prefix, Some(before), None, ud);
    if rc != VEIL_OK {
        drop_box::<T>(ud);
        return rc;
    }
    // 停止後の関数は起動に成功したときだけ呼ばれる。起動に失敗したときの状態は残る（プロセスの終わりまで）
    veil_config_on_shutdown(cfg, Some(drop_box::<T>), ud)
}

/// Requires a principal on the C routes under `prefix` (NULL is `/`): 401 without one, 403 when `scope` (NULL for
/// any principal) is missing. It is a C middleware: register it **before** the middleware that sets the principal
/// (the middleware registered last runs first).
///
/// # Safety
///
/// `cfg` is a valid configuration handle; `prefix` and `scope` are NULL or NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_require_scope(
    cfg: *mut veil_config_t,
    prefix: *const c_char,
    scope: *const c_char,
) -> c_int {
    let scope = tryc!(opt_cstr(scope)).map(str::to_string);
    register_guard(cfg, prefix, require_before, Require { scope })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn basic_bearer_and_api_keys() {
        // admin:pw
        let req = request(
            &[
                ("authorization", "Basic YWRtaW46cHc="),
                ("x-api-key", "k-123"),
            ],
            "",
        );
        with_handles(req, |rq, rs| unsafe {
            let (mut u, mut ul, mut p, mut pl) = (std::ptr::null(), 0, std::ptr::null(), 0);
            assert_eq!(veil_kit_basic_auth(rq, &mut u, &mut ul, &mut p, &mut pl), 1);
            assert_eq!(take(u, ul), "admin");
            assert_eq!(take(p, pl), "pw");
            assert_eq!(
                veil_kit_basic_matches(rq, c("admin").as_ptr(), c("pw").as_ptr()),
                1
            );
            assert_eq!(
                veil_kit_basic_matches(rq, c("admin").as_ptr(), c("px").as_ptr()),
                0
            );
            let mut len = 0;
            assert!(veil_kit_bearer(rq, &mut len).is_null());
            assert_eq!(
                take(veil_kit_api_key(rq, std::ptr::null(), &mut len), len),
                "k-123"
            );
            let keys = [c("nope"), c("k-123")];
            let ptrs: Vec<_> = keys.iter().map(|k| k.as_ptr()).collect();
            assert_eq!(
                veil_kit_api_key_matches(rq, std::ptr::null(), ptrs.as_ptr(), 2),
                1
            );
            assert_eq!(
                veil_kit_api_key_matches(rq, std::ptr::null(), ptrs.as_ptr(), 1),
                0
            );
            assert_eq!(
                veil_kit_unauthorized(rs, c("Basic realm=\"x\"").as_ptr()),
                VEIL_OK
            );
        });
        let (_, resp) = with_handles(
            request(&[("authorization", "Bearer abc")], ""),
            |rq, _| unsafe {
                let mut len = 0;
                assert_eq!(take(veil_kit_bearer(rq, &mut len), len), "abc");
                assert_eq!(
                    veil_kit_basic_auth(
                        rq,
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut(),
                        std::ptr::null_mut()
                    ),
                    0
                );
            },
        );
        assert_eq!(resp.status_code(), 200);
    }

    #[test]
    fn principals_and_the_scope_guard() {
        let g = Require {
            scope: Some("admin".into()),
        };
        let gp = &g as *const Require as *mut c_void;
        // 主体が無い: 401
        let (rc, resp) = with_handles(request(&[], ""), |rq, rs| unsafe {
            require_before(rq, rs, gp)
        });
        assert_eq!((rc, resp.status_code()), (veil_capi::VEIL_MW_STOP, 401));
        // スコープが無い: 403、ある: 続ける
        for (scope, want) in [("read", 403), ("admin", 200)] {
            let (rc, resp) = with_handles(request(&[], ""), |rq, rs| unsafe {
                let p = veil_kit_principal_new(c("alice").as_ptr());
                veil_kit_principal_add_scope(p, c(scope).as_ptr());
                veil_kit_principal_add_role(p, c("staff").as_ptr());
                assert_eq!(
                    veil_kit_principal_set_attributes(p, c("{\"a\":1}").as_ptr()),
                    VEIL_OK
                );
                assert_eq!(veil_kit_set_principal(rq, p), VEIL_OK);
                let got = veil_kit_principal(rq);
                let mut len = 0;
                assert_eq!(
                    take(veil_kit_principal_subject(got, &mut len), len),
                    "alice"
                );
                assert_eq!(veil_kit_principal_has_role(got, c("staff").as_ptr()), 1);
                assert_eq!(owned(veil_kit_principal_attributes(got)), "{\"a\":1}");
                require_before(rq, rs, gp)
            });
            let cont = if want == 200 {
                veil_capi::VEIL_MW_CONTINUE
            } else {
                veil_capi::VEIL_MW_STOP
            };
            assert_eq!((rc, resp.status_code()), (cont, want));
        }
        unsafe {
            let p = veil_kit_principal_new(c("x").as_ptr());
            assert_eq!(
                veil_kit_principal_set_attributes(p, c("{").as_ptr()),
                VEIL_ERR_INVALID_ARG
            );
            veil_kit_principal_free(p);
        }
    }
}
