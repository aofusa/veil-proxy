// F-291: セッション（veil_kit::session::Sessions をそのまま入れる。Cookie の形式は Rust の kit と同じ）。
// Sessions はワーカー上の Rust のミドルウェアで、C のハンドラ・ミドルウェアより外側で読み込み・書き戻す。
use crate::cookie::veil_kit_key_t;
use crate::{out_kept, out_none, tryc};
use kit::cookie::SameSite;
use kit::session::{MemoryStore, Session, Sessions};
use std::ffi::{c_char, c_int};
use std::time::Duration;
use veil::api::FromRequest;
use veil_capi::internal::{self, cstr, fail, opt_cstr, read_sized};
use veil_capi::{
    veil_config_t, veil_request_t, VEIL_ERR_INVALID_ARG, VEIL_ERR_STATE, VEIL_OFF, VEIL_OK,
};

/// Session settings (zero-filled means defaults; `size` is `sizeof(veil_kit_sessions_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_kit_sessions_t {
    pub size: usize,
    /// Only routes under this prefix (NULL is `"/"`).
    pub prefix: *const c_char,
    /// The cookie name (NULL is `veil_session`).
    pub cookie_name: *const c_char,
    /// Lifetime in seconds (0 is 24 hours; extended on every change).
    pub ttl_secs: u64,
    /// `Secure` (tri-state, default on; `VEIL_OFF` only for testing over plain HTTP).
    pub secure: c_int,
    /// `VEIL_KIT_SAMESITE_*` (0 is Lax).
    pub same_site: c_int,
    /// Non-zero: keep the data in server memory with a signed id in the cookie (0: the encrypted data in the cookie).
    pub memory_store: c_int,
    /// Maximum sessions in memory (0 is 100 000).
    pub max_entries: usize,
}

/// Installs cookie sessions (the Rust kit's `Sessions`, same cookie format) for the routes under `opts->prefix`.
/// `opts` may be NULL for the defaults: the whole session in one encrypted cookie `veil_session`, `HttpOnly`,
/// `Secure`, `SameSite=Lax`, 24 hours. The configuration keeps its own reference to `key`.
///
/// # Safety
///
/// `cfg` is a valid configuration handle, `key` a valid key, `opts` NULL or a valid `veil_kit_sessions_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_sessions_install(
    cfg: *mut veil_config_t,
    key: *const veil_kit_key_t,
    opts: *const veil_kit_sessions_t,
) -> c_int {
    let Some(k) = key.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL key");
    };
    let mut s = Sessions::new((*k.key).clone());
    if !opts.is_null() {
        let o = tryc!(read_sized(opts, "veil_kit_sessions_t"));
        if let Some(p) = tryc!(opt_cstr(o.prefix)) {
            if !p.starts_with('/') {
                return fail(VEIL_ERR_INVALID_ARG, "prefix must start with '/'");
            }
            s = s.on(p);
        }
        if let Some(n) = tryc!(opt_cstr(o.cookie_name)) {
            s = s.cookie_name(n);
        }
        if o.ttl_secs > 0 {
            s = s.ttl(Duration::from_secs(o.ttl_secs));
        }
        if o.secure == VEIL_OFF {
            s = s.secure(false);
        }
        s = s.same_site(match o.same_site {
            0 | crate::cookie::VEIL_KIT_SAMESITE_LAX => SameSite::Lax,
            crate::cookie::VEIL_KIT_SAMESITE_STRICT => SameSite::Strict,
            crate::cookie::VEIL_KIT_SAMESITE_NONE => SameSite::None,
            _ => return fail(VEIL_ERR_INVALID_ARG, "unknown same_site value"),
        });
        if o.memory_store != 0 {
            let mut m = MemoryStore::new();
            if o.max_entries > 0 {
                m = m.max_entries(o.max_entries);
            }
            s = s.store(m);
        }
    }
    if internal::push_plugin(cfg, move |app| app.with(s)) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_INVALID_ARG, "NULL config")
    }
}

unsafe fn session(req: *const veil_request_t) -> Result<Session, c_int> {
    let r = internal::request(req).ok_or_else(|| fail(VEIL_ERR_INVALID_ARG, "NULL request"))?;
    // Session::from_request は入っていなければ記録して 500 を作る。C には VEIL_ERR_STATE で返す
    Session::from_request(r).map_err(|_| {
        fail(
            VEIL_ERR_STATE,
            "no session on this route (install it with veil_kit_sessions_install)",
        )
    })
}

/// The value of `key` in the session as JSON text (NULL when absent). Not NUL-terminated; valid until the
/// callback returns.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `key` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_session_get(
    req: *const veil_request_t,
    key: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Ok(s), Ok(k)) = (session(req), cstr(key)) else {
        return out_none(len);
    };
    match s.get::<serde_json::Value>(k) {
        Some(v) => out_kept(req, v.to_string(), len),
        None => out_none(len),
    }
}

/// The value of `key` when it is a JSON string, unquoted (NULL when absent or not a string).
///
/// # Safety
///
/// Same as `veil_kit_session_get`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_session_get_str(
    req: *const veil_request_t,
    key: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Ok(s), Ok(k)) = (session(req), cstr(key)) else {
        return out_none(len);
    };
    match s.get::<serde_json::Value>(k) {
        Some(serde_json::Value::String(v)) => out_kept(req, v, len),
        _ => out_none(len),
    }
}

/// Sets `key` to a JSON value (`json` is checked; `VEIL_ERR_INVALID_ARG` when it is not JSON).
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `key` and `json` are NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_session_set(
    req: *const veil_request_t,
    key: *const c_char,
    json: *const c_char,
) -> c_int {
    let s = tryc!(session(req));
    let k = tryc!(cstr(key));
    match serde_json::from_str::<serde_json::Value>(tryc!(cstr(json))) {
        Ok(v) => {
            s.insert(k, v);
            VEIL_OK
        }
        Err(e) => fail(VEIL_ERR_INVALID_ARG, format!("invalid JSON: {}", e)),
    }
}

/// Sets `key` to a string.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `key` and `value` are NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_session_set_str(
    req: *const veil_request_t,
    key: *const c_char,
    value: *const c_char,
) -> c_int {
    let s = tryc!(session(req));
    s.insert(tryc!(cstr(key)), tryc!(cstr(value)));
    VEIL_OK
}

/// Removes `key`.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; `key` is a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_session_remove(
    req: *const veil_request_t,
    key: *const c_char,
) -> c_int {
    let s = tryc!(session(req));
    s.remove(tryc!(cstr(key)));
    VEIL_OK
}

/// Removes every value (the session stays).
///
/// # Safety
///
/// `req` is a handle passed to the current callback.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_session_clear(req: *const veil_request_t) -> c_int {
    tryc!(session(req)).clear();
    VEIL_OK
}

/// Ends the session (the cookie is deleted; with the memory store the data is removed too).
///
/// # Safety
///
/// `req` is a handle passed to the current callback.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_session_destroy(req: *const veil_request_t) -> c_int {
    tryc!(session(req)).destroy();
    VEIL_OK
}

/// Gives the session a new id and keeps its data (call it after a login, against session fixation).
///
/// # Safety
///
/// `req` is a handle passed to the current callback.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_session_cycle_id(req: *const veil_request_t) -> c_int {
    tryc!(session(req)).cycle_id();
    VEIL_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::*;

    #[test]
    fn no_session_is_a_state_error() {
        with_handles(request(&[], ""), |rq, _| unsafe {
            let mut len = 0;
            assert!(veil_kit_session_get(rq, c("a").as_ptr(), &mut len).is_null());
            assert_eq!(
                veil_kit_session_set_str(rq, c("a").as_ptr(), c("b").as_ptr()),
                VEIL_ERR_STATE
            );
            assert_eq!(veil_kit_session_destroy(rq), VEIL_ERR_STATE);
        });
    }
}
