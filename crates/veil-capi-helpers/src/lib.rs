// F-291: C 向けの kit（libveil_kit）。veil-proxy-kit（Rust の kit）を C ABI で出し、libveil の全関数も再公開する。
// 暗号・JWT・Cookie の形式は Rust の kit そのもの（C で書き直さない）。
//! The C helpers kit `libveil_kit` (`include/veil_kit.h`): everything of `libveil` plus `veil_kit_*` functions
//! built on `veil-proxy-kit` (cookies, signed and private cookies, sessions, Basic / Bearer / API key / JWT
//! authentication, guards, redirects, single files, form fields, JSON strings and app tasks).
//!
//! Link a C program with either `libveil` or `libveil_kit`, never both.

pub use veil_capi::*;

mod auth;
mod client;
mod cookie;
mod di;
mod graphql;
mod grpc;
mod jwt;
mod misc;
mod session;
mod tasks;
mod testkit;

use std::ffi::{c_char, c_int, c_void, CString};

/// The token is missing, malformed, expired or signed with an unknown key (`veil_kit_jwt_verify`).
pub const VEIL_KIT_ERR_TOKEN: c_int = -20;

/// A pointer that C code promised may be used from any thread (see the `user_data` rules of `veil.h`).
#[derive(Clone, Copy)]
pub(crate) struct UserData(pub(crate) *mut c_void);

// SAFETY: 呼び出し側（C）が user_data のスレッド安全性を保証する（veil.h の規則と同じ）。
unsafe impl Send for UserData {}
// SAFETY: 同上。
unsafe impl Sync for UserData {}

impl UserData {
    /// 閉包が構造体ごと捕捉するようにメソッド経由で取り出す。
    pub(crate) fn get(self) -> *mut c_void {
        self.0
    }
}

macro_rules! tryc {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(code) => return code,
        }
    };
}
pub(crate) use tryc;

/// `(ptr, *len)` を返す。
pub(crate) unsafe fn out_bytes(b: &[u8], len: *mut usize) -> *const c_char {
    if !len.is_null() {
        *len = b.len();
    }
    b.as_ptr() as *const c_char
}

/// 見つからないとき NULL と長さ 0。
pub(crate) unsafe fn out_none(len: *mut usize) -> *const c_char {
    if !len.is_null() {
        *len = 0;
    }
    std::ptr::null()
}

/// 文字列を要求の置き場に移して `(ptr, *len)` を返す（コールバックの間有効）。
pub(crate) unsafe fn out_kept(
    req: *const veil_request_t,
    s: String,
    len: *mut usize,
) -> *const c_char {
    match internal::keep(req, s) {
        Some((p, n)) => {
            if !len.is_null() {
                *len = n;
            }
            p
        }
        None => out_none(len),
    }
}

/// 呼び出し側が `veil_kit_free` で解放する NUL 終端の文字列にする。
pub(crate) fn alloc_string(s: String) -> *mut c_char {
    match CString::new(s) {
        Ok(c) => c.into_raw(),
        Err(_) => {
            internal::fail(VEIL_ERR_RUNTIME, "string contains a NUL byte");
            std::ptr::null_mut()
        }
    }
}

/// `*out` に `veil_kit_free` で解放する文字列を書く。
pub(crate) unsafe fn put_string(out: *mut *mut c_char, s: String) -> c_int {
    if out.is_null() {
        return internal::fail(VEIL_ERR_INVALID_ARG, "NULL output pointer");
    }
    let p = alloc_string(s);
    *out = p;
    if p.is_null() {
        VEIL_ERR_RUNTIME
    } else {
        VEIL_OK
    }
}

/// Frees a string returned by a `veil_kit_*` function (`NULL` does nothing).
///
/// # Safety
///
/// `p` is NULL or a string returned by this library that has not been freed.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_free(p: *mut c_char) {
    if !p.is_null() {
        drop(CString::from_raw(p));
    }
}

/// The version of the kit (NUL-terminated, static; the same as `veil_version`). Calling it from a program
/// confirms that it is linked with `libveil_kit` rather than `libveil`.
///
/// # Safety
///
/// The pointer is static and must not be freed.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

/// 定数時間の比較（長さが違えば偽。中身の位置で時間を変えない）。
pub(crate) fn ct_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
pub(crate) mod testutil {
    use super::*;
    use bytes::Bytes;
    use veil::api::{Request, Response};

    pub(crate) fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    pub(crate) fn take(p: *const c_char, len: usize) -> String {
        assert!(!p.is_null());
        // SAFETY: テストの中でアクセサが返した (ptr, len)
        String::from_utf8(unsafe { std::slice::from_raw_parts(p as *const u8, len) }.to_vec())
            .unwrap()
    }

    pub(crate) fn owned(p: *mut c_char) -> String {
        assert!(!p.is_null());
        // SAFETY: このライブラリが返した文字列
        let s = unsafe { std::ffi::CStr::from_ptr(p) }
            .to_str()
            .unwrap()
            .to_string();
        unsafe { veil_kit_free(p) };
        s
    }

    pub(crate) fn request(headers: &[(&str, &str)], body: &str) -> Request {
        Request::new(
            "POST",
            "/x",
            headers
                .iter()
                .map(|(n, v)| (Bytes::from(n.to_string()), Bytes::from(v.to_string())))
                .collect(),
            body.to_string(),
        )
    }

    /// Request を C のハンドラと同じ形で包み、`f` に渡す（veil-capi-core の試験用の口）。
    pub(crate) fn with_handles<R>(
        req: Request,
        f: impl FnOnce(*const veil_request_t, *mut veil_response_t) -> R,
    ) -> (R, Response) {
        internal::with_test_handles(req, f)
    }
}
