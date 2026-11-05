// F-291: gRPC の Balanced（Rust の kit の grpc::Balanced と同じ規則: 順番に回し、単項の呼び出しが UNAVAILABLE で
// 失敗したら次のサーバで試す（1 サーバにつき 1 回まで））。C のメッセージはバイト列なので、Rust の Codec ではなく
// コアの呼び出し（veil::api::grpc::client_call）で同じ規則を組む（prost を足さない）。
//! A round-robin gRPC channel with failover on UNAVAILABLE.

use crate::{tryc, UserData};
use bytes::Bytes;
use std::ffi::{c_char, c_int, c_void};
use std::ptr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use veil::api::grpc::client_call::BlockingCall;
use veil::api::grpc::Channel;
use veil_capi::internal::{self, cstr, fail};
use veil_capi::{
    veil_grpc_call_options_t, veil_grpc_channel_new, veil_grpc_channel_options_t,
    veil_grpc_channel_t, veil_grpc_client_call_t, veil_grpc_done_fn, VEIL_ERR_INVALID_ARG, VEIL_OK,
};

/// gRPC `UNAVAILABLE`.
const UNAVAILABLE: u8 = 14;

/// Channels to several servers.
#[allow(non_camel_case_types)]
pub struct veil_kit_grpc_balanced_t {
    handles: Vec<*mut veil_grpc_channel_t>,
    channels: Arc<Vec<Channel>>,
    next: AtomicUsize,
}

// SAFETY: handles は veil_grpc_channel_t（中身はスレッド安全な Channel）を指し、解放は veil_kit_grpc_balanced_free だけ
unsafe impl Send for veil_kit_grpc_balanced_t {}
// SAFETY: 同上（共有して読むだけ）
unsafe impl Sync for veil_kit_grpc_balanced_t {}

impl Drop for veil_kit_grpc_balanced_t {
    fn drop(&mut self) {
        for h in self.handles.drain(..) {
            // SAFETY: veil_grpc_channel_new が返したもので、ここだけが解放する
            unsafe { veil_capi::veil_grpc_channel_free(h) };
        }
    }
}

/// Channels to `n` URLs (at least one), with the same `opts` for each (`opts` may be NULL). NULL on error.
///
/// # Safety
///
/// `urls` points to `n` NUL-terminated strings; `opts` NULL or a valid `veil_grpc_channel_options_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_grpc_balanced_new(
    urls: *const *const c_char,
    n: usize,
    opts: *const veil_grpc_channel_options_t,
) -> *mut veil_kit_grpc_balanced_t {
    if urls.is_null() || n == 0 {
        fail(
            VEIL_ERR_INVALID_ARG,
            "a balanced channel needs at least one URL",
        );
        return ptr::null_mut();
    }
    let mut b = veil_kit_grpc_balanced_t {
        handles: Vec::with_capacity(n),
        channels: Arc::new(Vec::new()),
        next: AtomicUsize::new(0),
    };
    let mut chans = Vec::with_capacity(n);
    for i in 0..n {
        let h = veil_grpc_channel_new(*urls.add(i), opts);
        if h.is_null() {
            return ptr::null_mut(); // b の Drop が作った分を解放する
        }
        b.handles.push(h);
        chans.push(internal::grpc_channel(h).expect("just created"));
    }
    b.channels = Arc::new(chans);
    Box::into_raw(Box::new(b))
}

/// Frees a balanced channel (calls in flight keep their own reference).
///
/// # Safety
///
/// `b` is NULL or from `veil_kit_grpc_balanced_new`, not used afterwards (the channels of `pick` included).
#[no_mangle]
pub unsafe extern "C" fn veil_kit_grpc_balanced_free(b: *mut veil_kit_grpc_balanced_t) {
    if !b.is_null() {
        drop(Box::from_raw(b));
    }
}

/// The next channel in turn, for streaming calls with `veil_grpc_channel_call` (owned by `b`; do not free it).
///
/// # Safety
///
/// `b` is a valid balanced channel.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_grpc_balanced_pick(
    b: *const veil_kit_grpc_balanced_t,
) -> *const veil_grpc_channel_t {
    match b.as_ref() {
        Some(b) => {
            let i = b.next.fetch_add(1, Ordering::Relaxed) % b.handles.len();
            b.handles[i]
        }
        None => ptr::null(),
    }
}

/// 呼び出しの材料（失敗したら次のサーバで同じものを送る）。
struct Unary {
    channels: Arc<Vec<Channel>>,
    start: usize,
    path: String,
    msg: Bytes,
    opts: OwnedOpts,
}

/// 呼び出しの設定の写し（非同期で次のサーバへ回すときに C の領域を見ない）。
struct OwnedOpts {
    opts: veil_grpc_call_options_t,
    _strings: Vec<std::ffi::CString>,
    _ptrs: Vec<*const c_char>,
}

// SAFETY: opts のポインタは _ptrs・_strings（この構造体が持つ）だけを指す
unsafe impl Send for OwnedOpts {}
// SAFETY: 作った後は読むだけ
unsafe impl Sync for OwnedOpts {}

unsafe fn own_opts(opts: *const veil_grpc_call_options_t) -> Result<Option<OwnedOpts>, c_int> {
    if opts.is_null() {
        return Ok(None);
    }
    let o = internal::read_sized(opts, "veil_grpc_call_options_t")?;
    // 検査を兼ねて一度ヘッダの組にする
    let md = internal::header_pairs(o.metadata, o.n_metadata, "metadata")?;
    let mut strings = Vec::with_capacity(md.len() * 2);
    for (n, v) in &md {
        for s in [n, v] {
            strings.push(
                std::ffi::CString::new(s.to_vec())
                    .map_err(|_| fail(VEIL_ERR_INVALID_ARG, "metadata contains a NUL byte"))?,
            );
        }
    }
    let ptrs: Vec<*const c_char> = strings.iter().map(|s| s.as_ptr()).collect();
    let mut copy = o;
    copy.size = std::mem::size_of::<veil_grpc_call_options_t>();
    copy.metadata = if ptrs.is_empty() {
        ptr::null()
    } else {
        ptrs.as_ptr()
    };
    copy.n_metadata = md.len();
    Ok(Some(OwnedOpts {
        opts: copy,
        _strings: strings,
        _ptrs: ptrs,
    }))
}

impl Unary {
    fn opts_ptr(&self) -> *const veil_grpc_call_options_t {
        if self.opts.opts.size == 0 {
            ptr::null()
        } else {
            &self.opts.opts
        }
    }

    /// `k` 番目のサーバで始める。
    fn start(&self, k: usize) -> Result<BlockingCall, c_int> {
        let ch = &self.channels[(self.start + k) % self.channels.len()];
        // SAFETY: opts_ptr は self が持つ写し
        unsafe { internal::grpc_start_one(ch, &self.path, self.msg.clone(), self.opts_ptr()) }
    }
}

unsafe fn prepare(
    b: *const veil_kit_grpc_balanced_t,
    path: *const c_char,
    msg: *const u8,
    len: usize,
    opts: *const veil_grpc_call_options_t,
) -> Result<Unary, c_int> {
    let Some(b) = b.as_ref() else {
        return Err(fail(VEIL_ERR_INVALID_ARG, "NULL balanced channel"));
    };
    let path = cstr(path)?;
    if !path.starts_with('/') {
        return Err(fail(
            VEIL_ERR_INVALID_ARG,
            "method path must look like /package.Service/Method",
        ));
    }
    if msg.is_null() && len > 0 {
        return Err(fail(
            VEIL_ERR_INVALID_ARG,
            "NULL message with a non-zero length",
        ));
    }
    let m = if len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(msg, len))
    };
    let opts = own_opts(opts)?.unwrap_or(OwnedOpts {
        // size 0 = 設定無し
        opts: std::mem::zeroed(),
        _strings: Vec::new(),
        _ptrs: Vec::new(),
    });
    Ok(Unary {
        channels: Arc::clone(&b.channels),
        start: b.next.fetch_add(1, Ordering::Relaxed),
        path: path.to_string(),
        msg: m,
        opts,
    })
}

/// A unary call on the next server, moving on to the following server when it fails with `UNAVAILABLE` (once per
/// server). Waits; `*out` is the finished call of the last attempt (as for `veil_grpc_channel_unary`).
/// `VEIL_ERR_STATE` on a worker.
///
/// # Safety
///
/// As for `veil_grpc_channel_unary`, with a balanced channel.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_grpc_balanced_unary(
    b: *const veil_kit_grpc_balanced_t,
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
    tryc!(internal::check_blocking());
    let u = tryc!(prepare(b, path, msg, len, opts));
    let n = u.channels.len();
    for k in 0..n {
        let call = tryc!(u.start(k));
        let st = call.status();
        if st.code != UNAVAILABLE || k + 1 == n {
            *out = internal::grpc_finished(Arc::new(call), st);
            return VEIL_OK;
        }
    }
    unreachable!("the loop returns on the last server")
}

/// 非同期: k 番目で始め、UNAVAILABLE なら次のサーバへ。終わったらハンドラ用プールで `done` を呼ぶ。
fn attempt(u: Arc<Unary>, k: usize, done: veil_grpc_done_fn, ud: UserData) {
    let call = match u.start(k) {
        Ok(c) => Arc::new(c),
        Err(_) => {
            // 始められない（クライアントのスレッドが無い等）: NULL で知らせる
            veil::runtime::offload::spawn_handler(move || {
                let ud = ud;
                fail(
                    veil_capi::VEIL_ERR_CLIENT,
                    "the call could not be started on the next server",
                );
                // SAFETY: veil_kit_grpc_balanced_unary_async の契約
                unsafe { done(ptr::null_mut(), ud.get()) };
            });
            return;
        }
    };
    let keep = Arc::clone(&call);
    call.status_then(move |st| {
        if st.code == UNAVAILABLE && k + 1 < u.channels.len() {
            attempt(u, k + 1, done, ud);
            return;
        }
        let ud = ud;
        // SAFETY: veil_kit_grpc_balanced_unary_async の契約
        unsafe { done(internal::grpc_finished(keep, st), ud.get()) };
    });
}

/// The asynchronous form of `veil_kit_grpc_balanced_unary`: `done` is called on the handler pool with the finished
/// call of the last attempt (NULL when a later attempt could not be started; then see `veil_last_error`).
///
/// # Safety
///
/// As for `veil_grpc_channel_unary_async`, with a balanced channel.
#[no_mangle]
pub unsafe extern "C" fn veil_kit_grpc_balanced_unary_async(
    b: *const veil_kit_grpc_balanced_t,
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
    let u = Arc::new(tryc!(prepare(b, path, msg, len, opts)));
    // 最初のサーバで始められなければ同期的に誤りを返す
    let first = Arc::new(tryc!(u.start(0)));
    let keep = Arc::clone(&first);
    let ud = UserData(user_data);
    first.status_then(move |st| {
        if st.code == UNAVAILABLE && u.channels.len() > 1 {
            attempt(u, 1, done, ud);
            return;
        }
        let ud = ud;
        // SAFETY: veil_kit_grpc_balanced_unary_async の契約
        unsafe { done(internal::grpc_finished(keep, st), ud.get()) };
    });
    VEIL_OK
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testutil::c;

    #[test]
    fn pick_rotates_and_arguments_are_checked() {
        unsafe {
            let (a, b2) = (c("http://127.0.0.1:1"), c("http://127.0.0.1:2"));
            let urls = [a.as_ptr(), b2.as_ptr()];
            assert!(veil_kit_grpc_balanced_new(urls.as_ptr(), 0, ptr::null()).is_null());
            let bad = [c("ftp://x").into_raw() as *const c_char];
            assert!(veil_kit_grpc_balanced_new(bad.as_ptr(), 1, ptr::null()).is_null());
            drop(std::ffi::CString::from_raw(bad[0] as *mut c_char));
            let b = veil_kit_grpc_balanced_new(urls.as_ptr(), 2, ptr::null());
            assert!(!b.is_null());
            let p1 = veil_kit_grpc_balanced_pick(b);
            let p2 = veil_kit_grpc_balanced_pick(b);
            let p3 = veil_kit_grpc_balanced_pick(b);
            assert_ne!(p1, p2);
            assert_eq!(p1, p3);
            let mut out = ptr::null_mut();
            assert_eq!(
                veil_kit_grpc_balanced_unary(
                    b,
                    c("x").as_ptr(),
                    ptr::null(),
                    0,
                    ptr::null(),
                    &mut out
                ),
                VEIL_ERR_INVALID_ARG
            );
            veil_kit_grpc_balanced_free(b);
        }
    }
}
