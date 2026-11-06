// F-291: Cookie（veil_kit::cookie そのもの。署名・暗号化の形式は Rust の kit と同じ）。
use crate::{out_bytes, out_kept, out_none, tryc};
use kit::cookie::{Cookie, CookieJar, Key, SameSite};
use std::ffi::{c_char, c_int};
use std::sync::Arc;
use std::time::Duration;
use veil_capi::internal::{self, cstr, fail, opt_cstr, read_sized};
use veil_capi::{veil_request_t, veil_response_t, VEIL_ERR_INVALID_ARG, VEIL_OK};

/// No `SameSite` attribute.
pub const VEIL_KIT_SAMESITE_DEFAULT: c_int = 0;
/// `SameSite=Strict`
pub const VEIL_KIT_SAMESITE_STRICT: c_int = 1;
/// `SameSite=Lax`
pub const VEIL_KIT_SAMESITE_LAX: c_int = 2;
/// `SameSite=None` (also sets `Secure`).
pub const VEIL_KIT_SAMESITE_NONE: c_int = 3;

/// A cookie to send (zero-filled means defaults; `size` is `sizeof(veil_kit_cookie_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_kit_cookie_t {
    pub size: usize,
    pub name: *const c_char,
    pub value: *const c_char,
    /// NULL is `Path=/`; an empty string sets no `Path`.
    pub path: *const c_char,
    pub domain: *const c_char,
    /// 0: none (a session cookie); positive: seconds; negative: delete (`Max-Age=0`).
    pub max_age_secs: i64,
    pub secure: c_int,
    pub http_only: c_int,
    /// `VEIL_KIT_SAMESITE_*`
    pub same_site: c_int,
    pub partitioned: c_int,
}

/// `veil_kit_cookie_t` から Cookie を作る（`value` は `value` の代わりに使う値。署名・暗号化の後の値）。
unsafe fn build(c: &veil_kit_cookie_t, value: Option<String>) -> Result<Cookie, c_int> {
    let name = cstr(c.name)?;
    let value = match value {
        Some(v) => v,
        None => opt_cstr(c.value)?.unwrap_or("").to_string(),
    };
    let mut k = Cookie::new(name, value);
    match opt_cstr(c.path)? {
        None => {}
        Some("") => k = k.path(None),
        Some(p) => k = k.path(Some(p)),
    }
    if let Some(d) = opt_cstr(c.domain)? {
        k = k.domain(d);
    }
    if c.max_age_secs > 0 {
        k = k.max_age(Duration::from_secs(c.max_age_secs as u64));
    } else if c.max_age_secs < 0 {
        k = k.max_age(Duration::ZERO);
    }
    k = k.secure(c.secure != 0).http_only(c.http_only != 0);
    k = match c.same_site {
        VEIL_KIT_SAMESITE_DEFAULT => k,
        VEIL_KIT_SAMESITE_STRICT => k.same_site(SameSite::Strict),
        VEIL_KIT_SAMESITE_LAX => k.same_site(SameSite::Lax),
        VEIL_KIT_SAMESITE_NONE => k.same_site(SameSite::None),
        _ => return Err(fail(VEIL_ERR_INVALID_ARG, "unknown same_site value")),
    };
    Ok(k.partitioned(c.partitioned != 0))
}

/// Cookie を `Set-Cookie` として足す（検査に通らなければ送らずに `VEIL_ERR_INVALID_ARG`）。
unsafe fn add(resp: *mut veil_response_t, k: Cookie) -> c_int {
    let h = match k.to_header() {
        Ok(h) => h,
        Err(e) => return fail(VEIL_ERR_INVALID_ARG, e.to_string()),
    };
    if internal::update_response(resp, |r| r.with_header("set-cookie", h)) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_INVALID_ARG, "NULL response")
    }
}

/// The value of the first cookie named `name` in the request's `Cookie` headers (all of them, including the
/// separate headers of HTTP/2), without surrounding quotes. NULL when absent. Not NUL-terminated; valid until the
/// callback returns.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `name` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_cookie_get(
    req: *const veil_request_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(name)) = (internal::request(req), cstr(name)) else {
        return out_none(len);
    };
    let jar = CookieJar::from_request_headers(r);
    match jar.get_bytes(name) {
        // 値は要求のヘッダの Bytes の切り出し（同じ領域）なので、コールバックの間有効
        Some(v) => out_bytes(v, len),
        None => out_none(len),
    }
}

/// Adds a `Set-Cookie` header (several calls send several cookies). The cookie is checked as by the Rust kit
/// (RFC 6265bis names and values, the `__Secure-` and `__Host-` prefixes); an invalid one is not sent and the
/// result is `VEIL_ERR_INVALID_ARG`.
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `cookie` points to a valid `veil_kit_cookie_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_set_cookie(
    resp: *mut veil_response_t,
    cookie: *const veil_kit_cookie_t,
) -> c_int {
    let c = tryc!(read_sized(cookie, "veil_kit_cookie_t"));
    let k = tryc!(build(&c, None));
    add(resp, k)
}

/// Deletes the cookie `name` in the browser (`Max-Age=0`, `Path=/`).
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `name` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_remove_cookie(
    resp: *mut veil_response_t,
    name: *const c_char,
) -> c_int {
    let name = tryc!(cstr(name));
    add(resp, Cookie::removal(name))
}

// ===================== 鍵 =====================

/// The key of signed and private cookies and sessions (the Rust kit's `veil_kit::cookie::Key`).
#[allow(non_camel_case_types)]
pub struct veil_kit_key_t {
    pub(crate) key: Arc<Key>,
}

fn new_key(key: Key) -> *mut veil_kit_key_t {
    Box::into_raw(Box::new(veil_kit_key_t { key: Arc::new(key) }))
}

/// A key from 64 bytes of secret material (the first half signs, the second half encrypts). NULL when `len` is
/// not 64.
///
/// # Safety
///
/// `material` points to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_key_from(material: *const u8, len: usize) -> *mut veil_kit_key_t {
    if material.is_null() || len != 64 {
        fail(VEIL_ERR_INVALID_ARG, "the key material must be 64 bytes");
        return std::ptr::null_mut();
    }
    let mut m = [0u8; 64];
    m.copy_from_slice(std::slice::from_raw_parts(material, 64));
    new_key(Key::from(&m))
}

/// A key derived from a secret of any length of at least 32 bytes (the same derivation as the Rust kit's
/// `Key::derive_from`, so both read each other's cookies). NULL when the secret is shorter.
///
/// # Safety
///
/// `secret` points to `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_key_derive(secret: *const u8, len: usize) -> *mut veil_kit_key_t {
    if secret.is_null() {
        fail(VEIL_ERR_INVALID_ARG, "NULL secret");
        return std::ptr::null_mut();
    }
    match Key::derive_from(std::slice::from_raw_parts(secret, len)) {
        Ok(k) => new_key(k),
        Err(e) => {
            fail(VEIL_ERR_INVALID_ARG, e.to_string());
            std::ptr::null_mut()
        }
    }
}

/// A random key (cookies made with it do not survive a restart).
///
/// # Safety
///
/// Always callable; free the key with `veil_kit_key_free`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_key_generate() -> *mut veil_kit_key_t {
    new_key(Key::generate())
}

/// Also accepts cookies made with `old` when reading (key rotation; new cookies use `key`). Call it before the key
/// is used by requests.
///
/// # Safety
///
/// `key` and `old` are valid keys.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_key_add_fallback(
    key: *mut veil_kit_key_t,
    old: *const veil_kit_key_t,
) -> c_int {
    let (Some(k), Some(o)) = (key.as_mut(), old.as_ref()) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL key");
    };
    k.key = Arc::new((*k.key).clone().with_fallback(&o.key));
    VEIL_OK
}

/// Frees a key (`NULL` does nothing). Sessions installed with it keep their own reference.
///
/// # Safety
///
/// `key` is NULL or a key that has not been freed; it is not used afterwards.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_key_free(key: *mut veil_kit_key_t) {
    if !key.is_null() {
        drop(Box::from_raw(key));
    }
}

// ===================== 署名・暗号化 Cookie =====================

unsafe fn raw_cookie<'a>(
    req: *const veil_request_t,
    key: *const veil_kit_key_t,
    name: *const c_char,
) -> Option<(&'a veil_kit_key_t, &'a str, bytes::Bytes)> {
    let r = internal::request(req)?;
    let k = key.as_ref()?;
    let name = cstr(name).ok()?;
    let v = CookieJar::from_request_headers(r).get_bytes(name)?.clone();
    Some((k, name, v))
}

/// The verified value of the signed cookie `name` (made by `veil_kit_signed_set` or by the Rust kit's
/// `SignedCookieJar` with the same key). NULL when it is absent or its signature does not match. Not
/// NUL-terminated; valid until the callback returns.
///
/// # Safety
///
/// `req` is a handle passed to the current callback, `key` a valid key, `name` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_signed_get(
    req: *const veil_request_t,
    key: *const veil_kit_key_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let Some((k, name, raw)) = raw_cookie(req, key, name) else {
        return out_none(len);
    };
    match k.key.verify_value(name, &raw) {
        Some(v) => out_kept(req, v, len),
        None => out_none(len),
    }
}

/// Adds a signed cookie: `cookie->value` is signed with `key` (the browser can read the value but not change it).
///
/// # Safety
///
/// `resp` is a handle passed to the current callback, `key` a valid key, `cookie` a valid `veil_kit_cookie_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_signed_set(
    resp: *mut veil_response_t,
    key: *const veil_kit_key_t,
    cookie: *const veil_kit_cookie_t,
) -> c_int {
    let Some(k) = key.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL key");
    };
    let c = tryc!(read_sized(cookie, "veil_kit_cookie_t"));
    let name = tryc!(cstr(c.name));
    let value = tryc!(opt_cstr(c.value)).unwrap_or("");
    let signed = k.key.sign_value(name, value);
    let ck = tryc!(build(&c, Some(signed)));
    add(resp, ck)
}

/// The decrypted value of the private cookie `name` (made by `veil_kit_private_set` or by the Rust kit's
/// `PrivateCookieJar` with the same key). NULL when it is absent, altered, made with another key or not UTF-8.
///
/// # Safety
///
/// Same as `veil_kit_signed_get`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_private_get(
    req: *const veil_request_t,
    key: *const veil_kit_key_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let Some((k, name, raw)) = raw_cookie(req, key, name) else {
        return out_none(len);
    };
    match k
        .key
        .decrypt_value(name, &raw)
        .and_then(|b| String::from_utf8(b).ok())
    {
        Some(v) => out_kept(req, v, len),
        None => out_none(len),
    }
}

/// Adds a private cookie: `cookie->value` is encrypted and authenticated with `key` (AES-256-GCM; the browser can
/// neither read nor change it).
///
/// # Safety
///
/// Same as `veil_kit_signed_set`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_private_set(
    resp: *mut veil_response_t,
    key: *const veil_kit_key_t,
    cookie: *const veil_kit_cookie_t,
) -> c_int {
    let Some(k) = key.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL key");
    };
    let c = tryc!(read_sized(cookie, "veil_kit_cookie_t"));
    let name = tryc!(cstr(c.name));
    let value = tryc!(opt_cstr(c.value)).unwrap_or("");
    let sealed = k.key.encrypt_value(name, value.as_bytes());
    let ck = tryc!(build(&c, Some(sealed)));
    add(resp, ck)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    fn set_cookies(r: &veil::api::Response) -> Vec<String> {
        r.headers()
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(b"set-cookie"))
            .map(|(_, v)| String::from_utf8(v.to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn get_and_set_cookies() {
        let req = request(&[("cookie", "a=1; b=\"two\""), ("cookie", "c=3")], "");
        let (n, resp) = with_handles(req, |rq, rs| unsafe {
            let mut len = 0;
            assert_eq!(
                take(veil_kit_cookie_get(rq, c("b").as_ptr(), &mut len), len),
                "two"
            );
            assert_eq!(
                take(veil_kit_cookie_get(rq, c("c").as_ptr(), &mut len), len),
                "3"
            );
            assert!(veil_kit_cookie_get(rq, c("zz").as_ptr(), &mut len).is_null());
            let (name, value) = (c("sid"), c("abc"));
            let mut ck: veil_kit_cookie_t = std::mem::zeroed();
            ck.name = name.as_ptr();
            ck.value = value.as_ptr();
            ck.http_only = 1;
            ck.same_site = VEIL_KIT_SAMESITE_LAX;
            ck.max_age_secs = 60;
            assert_eq!(veil_kit_set_cookie(rs, &ck), VEIL_OK);
            ck.max_age_secs = 0;
            ck.same_site = 0;
            ck.http_only = 0;
            let empty = c("");
            ck.path = empty.as_ptr();
            assert_eq!(veil_kit_set_cookie(rs, &ck), VEIL_OK);
            let bad = c("a;b");
            ck.value = bad.as_ptr();
            assert_eq!(veil_kit_set_cookie(rs, &ck), VEIL_ERR_INVALID_ARG);
            let host = c("__Host-x");
            ck.name = host.as_ptr();
            ck.value = value.as_ptr();
            assert_eq!(veil_kit_set_cookie(rs, &ck), VEIL_ERR_INVALID_ARG);
            assert_eq!(veil_kit_remove_cookie(rs, c("old").as_ptr()), VEIL_OK);
            ck.size = 4;
            assert_eq!(veil_kit_set_cookie(rs, &ck), VEIL_ERR_INVALID_ARG);
            3
        });
        assert_eq!(n, 3);
        assert_eq!(
            set_cookies(&resp),
            [
                "sid=abc; Path=/; Max-Age=60; HttpOnly; SameSite=Lax",
                "sid=abc",
                "old=; Path=/; Max-Age=0"
            ]
        );
    }

    // 署名・暗号化 Cookie は Rust の kit（veil_kit::cookie::Key）と同じ形式で、互いに読める
    #[test]
    fn signed_and_private_cookies_interoperate_with_the_rust_kit() {
        let secret = b"kit-example-cookie-secret-0123456789";
        let rust_key = Key::derive_from(secret).unwrap();
        let ck = unsafe { veil_kit_key_derive(secret.as_ptr(), secret.len()) };
        assert!(!ck.is_null());
        // Rust の kit が作った値を C が読む
        let signed = rust_key.sign_value("user", "alice");
        let sealed = rust_key.encrypt_value("secret", b"s3cr3t");
        let cookie = format!("user={signed}; secret={sealed}");
        let req = request(&[("cookie", &cookie)], "");
        let (_, resp) = with_handles(req, |rq, rs| unsafe {
            let mut len = 0;
            let v = veil_kit_signed_get(rq, ck, c("user").as_ptr(), &mut len);
            assert_eq!(take(v, len), "alice");
            let v = veil_kit_private_get(rq, ck, c("secret").as_ptr(), &mut len);
            assert_eq!(take(v, len), "s3cr3t");
            // 名前を取り違えた値・別の鍵は受けない
            let other = veil_kit_key_generate();
            assert!(veil_kit_signed_get(rq, other, c("user").as_ptr(), &mut len).is_null());
            assert!(veil_kit_private_get(rq, other, c("secret").as_ptr(), &mut len).is_null());
            // 鍵の入れ替え: 古い鍵を fallback にすれば読める
            assert_eq!(veil_kit_key_add_fallback(other, ck), VEIL_OK);
            assert!(!veil_kit_signed_get(rq, other, c("user").as_ptr(), &mut len).is_null());
            veil_kit_key_free(other);
            // C が作った値を Rust の kit が読む
            let (n, v) = (c("user"), c("bob"));
            let mut out: veil_kit_cookie_t = std::mem::zeroed();
            out.name = n.as_ptr();
            out.value = v.as_ptr();
            assert_eq!(veil_kit_signed_set(rs, ck, &out), VEIL_OK);
            let n2 = c("secret");
            out.name = n2.as_ptr();
            assert_eq!(veil_kit_private_set(rs, ck, &out), VEIL_OK);
        });
        let sc = set_cookies(&resp);
        let value = |s: &str| {
            s.split(';')
                .next()
                .unwrap()
                .split_once('=')
                .unwrap()
                .1
                .to_string()
        };
        assert_eq!(
            sc[0],
            format!("user={}; Path=/", rust_key.sign_value("user", "bob"))
        );
        assert_eq!(
            rust_key
                .verify_value("user", value(&sc[0]).as_bytes())
                .as_deref(),
            Some("bob")
        );
        assert_eq!(
            rust_key
                .decrypt_value("secret", value(&sc[1]).as_bytes())
                .as_deref(),
            Some(&b"bob"[..])
        );
        // 改ざんした値は読めない
        let tampered = format!("user={}", signed.replace("alice", "admin"));
        let (_, _) = with_handles(request(&[("cookie", &tampered)], ""), |rq, _| unsafe {
            let mut len = 0;
            assert!(veil_kit_signed_get(rq, ck, c("user").as_ptr(), &mut len).is_null());
        });
        unsafe {
            assert!(veil_kit_key_derive(b"short".as_ptr(), 5).is_null());
            assert!(veil_kit_key_from([0u8; 10].as_ptr(), 10).is_null());
            let m = [7u8; 64];
            let k = veil_kit_key_from(m.as_ptr(), 64);
            assert!(!k.is_null());
            assert_eq!(
                (*k).key.sign_value("a", "b"),
                Key::from(&m).sign_value("a", "b")
            );
            veil_kit_key_free(k);
            veil_kit_key_free(ck);
        }
    }
}
