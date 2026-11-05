//! C API の拡張（F-291 のコア FFI 第 1 弾）: 応答ヘッダの追加・要求ヘッダの列挙・TLS と HTTP の版・
//! 要求ごとの値・ミドルウェア・応答後の処理・起動と停止とワーカー開始のフック・CORS・静的ディレクトリ配信・
//! WebSocket のサブプロトコル・gRPC reflection。
//!
//! C のコールバックはワーカーで呼ばない（INLINE のルートとワーカー開始のフックを除く今の規則）。C のミドルウェアは
//! ルートのハンドラと同じ `call_blocking` の閉包の中で順に呼ぶ（要求あたりのスレッドの切り替えは 1 回のまま）。

use super::{
    cstr, fail, opt_cstr, out_bytes, out_none, tri, valid_header, veil_config_t, veil_handler_fn,
    veil_request_t, veil_response_t, UserData, VEIL_ERR_CONFIG, VEIL_ERR_INVALID_ARG, VEIL_OK,
};
use bytes::Bytes;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use std::sync::{Arc, Mutex};
use veil::api::{App, HttpVersion, Response};

// ===================== 大きさ付きの構造体 =====================

// 先頭に `size_t size` を持つ構造体を読む（B-209 と同じ前方互換: 0 はこの版の大きさ、それより小さいものは拒否、
// 大きいもの（後の版のヘッダ）は知っている分だけ読む）。
/// Reads a struct that starts with `size_t size`.
///
/// # Safety
///
/// `p` is NULL or points to a `T` with at least `size` readable bytes. `T` is `#[repr(C)]`, starts with a `usize`
/// and is valid when all bits are zero (pointers and integers only).
pub(crate) unsafe fn read_sized<T: Copy>(p: *const T, what: &str) -> Result<T, c_int> {
    if p.is_null() {
        return Err(fail(VEIL_ERR_INVALID_ARG, format!("NULL {}", what)));
    }
    let full = std::mem::size_of::<T>();
    let size = match ptr::read(p as *const usize) {
        0 => full,
        n => n,
    };
    if size < full {
        return Err(fail(
            VEIL_ERR_INVALID_ARG,
            format!("{}.size must be 0 or sizeof({})", what, what),
        ));
    }
    let mut v = std::mem::MaybeUninit::<T>::zeroed();
    ptr::copy_nonoverlapping(p as *const u8, v.as_mut_ptr() as *mut u8, full);
    // SAFETY: 全体を 0 で初期化してから呼び出し側の値を写した（契約により全ビット 0 も有効）
    Ok(v.assume_init())
}

/// Reads `n` C strings from a `const char *const *` (NULL is allowed when `n` is 0).
unsafe fn str_array(p: *const *const c_char, n: usize, what: &str) -> Result<Vec<String>, c_int> {
    if n == 0 {
        return Ok(Vec::new());
    }
    if p.is_null() {
        return Err(fail(
            VEIL_ERR_INVALID_ARG,
            format!("NULL {} with a non-zero count", what),
        ));
    }
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        out.push(cstr(*p.add(i))?.to_string());
    }
    Ok(out)
}

// ===================== 応答ヘッダの追加・要求ヘッダの列挙 =====================

/// Adds a response header without replacing one of the same name (for several `Set-Cookie`). Control characters such as CR/LF are rejected.
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `name` and `value` are NUL-terminated strings.
#[no_mangle]
pub unsafe extern "C" fn veil_response_add_header(
    resp: *mut veil_response_t,
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    let Some(r) = resp.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL response");
    };
    let name = tryc!(cstr(name));
    let value = tryc!(cstr(value));
    if !valid_header(name, value) {
        return fail(VEIL_ERR_INVALID_ARG, "invalid header name or value");
    }
    r.resp = std::mem::replace(&mut r.resp, Response::new(200)).with_header(
        Bytes::copy_from_slice(name.as_bytes()),
        Bytes::copy_from_slice(value.as_bytes()),
    );
    VEIL_OK
}

/// The number of request headers (repeated names count once each).
///
/// # Safety
///
/// `req` is a handle passed to the current callback, or NULL (the result is 0).
#[no_mangle]
pub unsafe extern "C" fn veil_request_header_count(req: *const veil_request_t) -> usize {
    req.as_ref().map(|r| r.req.headers().len()).unwrap_or(0)
}

/// The name and value of the `i`-th request header (in received order, not NUL-terminated). Out of range is `VEIL_ERR_INVALID_ARG`.
///
/// # Safety
///
/// `req` is a handle passed to the current callback; every output pointer may be NULL. The returned pointers are valid until the callback returns.
#[no_mangle]
pub unsafe extern "C" fn veil_request_header_at(
    req: *const veil_request_t,
    index: usize,
    name: *mut *const c_char,
    name_len: *mut usize,
    value: *mut *const c_char,
    value_len: *mut usize,
) -> c_int {
    let Some(r) = req.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL request");
    };
    let Some((n, v)) = r.req.headers().get(index) else {
        return fail(VEIL_ERR_INVALID_ARG, "header index out of range");
    };
    let np = out_bytes(n, name_len);
    let vp = out_bytes(v, value_len);
    if !name.is_null() {
        *name = np;
    }
    if !value.is_null() {
        *value = vp;
    }
    VEIL_OK
}

/// Returns the values of the header `name` one by one (case-insensitive). Start with `*iter = 0` and call until NULL
/// (to read repeated headers, such as `Cookie` split over several HTTP/2 fields).
///
/// # Safety
///
/// `req` is a handle passed to the current callback, `name` a NUL-terminated string, `iter` a writable `size_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_request_header_next(
    req: *const veil_request_t,
    name: *const c_char,
    iter: *mut usize,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Some(it)) = (req.as_ref(), iter.as_mut()) else {
        return out_none(len);
    };
    if name.is_null() {
        return out_none(len);
    }
    let name = CStr::from_ptr(name).to_bytes();
    let headers = r.req.headers();
    while *it < headers.len() {
        let (n, v) = &headers[*it];
        *it += 1;
        if n.eq_ignore_ascii_case(name) {
            return out_bytes(v, len);
        }
    }
    out_none(len)
}

/// HTTP/1.1 (and HTTP/1.0)
pub const VEIL_HTTP_1_1: c_int = 11;
/// HTTP/2
pub const VEIL_HTTP_2: c_int = 20;
/// HTTP/3
pub const VEIL_HTTP_3: c_int = 30;

/// 1 when the request came over TLS (always for HTTP/3). `X-Forwarded-Proto` is not consulted (clients can set it).
///
/// # Safety
///
/// `req` is a handle passed to the current callback, or NULL (the result is 0).
#[no_mangle]
pub unsafe extern "C" fn veil_request_is_tls(req: *const veil_request_t) -> c_int {
    req.as_ref().map(|r| r.req.is_tls() as c_int).unwrap_or(0)
}

/// The HTTP version of the request (`VEIL_HTTP_1_1`, `VEIL_HTTP_2`, `VEIL_HTTP_3`; 0 for NULL).
///
/// # Safety
///
/// `req` is a handle passed to the current callback, or NULL.
#[no_mangle]
pub unsafe extern "C" fn veil_request_version(req: *const veil_request_t) -> c_int {
    match req.as_ref().map(|r| r.req.version()) {
        Some(HttpVersion::Http11) => VEIL_HTTP_1_1,
        Some(HttpVersion::Http2) => VEIL_HTTP_2,
        Some(HttpVersion::Http3) => VEIL_HTTP_3,
        None => 0,
    }
}

// ===================== 要求ごとの値 =====================

/// A function that frees a value (passed to `veil_request_set_local` and others).
#[allow(non_camel_case_types)]
pub type veil_free_fn = unsafe extern "C" fn(*mut c_void);

struct Local {
    key: Box<[u8]>,
    val: *mut c_void,
    free: Option<veil_free_fn>,
}

impl Local {
    fn release(self) {
        if let Some(f) = self.free {
            // SAFETY: veil_request_set_local の契約（val は free に 1 回だけ渡してよい）
            unsafe { f(self.val) }
        }
    }
}

// 要求の `extensions` に 1 個だけ置く C の値の表（使った要求だけ確保する）。
#[derive(Default)]
struct CLocals(Mutex<Vec<Local>>);

// SAFETY: 値のポインタは C の利用者のもの。veil_request_set_local の契約により、要求が終わるまで有効で、
// 解放関数はどのスレッドから呼んでもよい。表そのものは Mutex で守る。
unsafe impl Send for CLocals {}
// SAFETY: 同上。
unsafe impl Sync for CLocals {}

impl Drop for CLocals {
    fn drop(&mut self) {
        let v = std::mem::take(self.0.get_mut().unwrap_or_else(|e| e.into_inner()));
        for l in v {
            l.release();
        }
    }
}

/// Stores a request-scoped value (from middleware to the handler and after-send work). A previous value of the same
/// `key` is freed and replaced. `free` (may be NULL) is called once when the request ends, from any thread.
///
/// # Safety
///
/// `req` is a handle passed to the current callback, `key` a NUL-terminated string; `val` stays valid until the request ends.
#[no_mangle]
pub unsafe extern "C" fn veil_request_set_local(
    req: *const veil_request_t,
    key: *const c_char,
    val: *mut c_void,
    free: Option<veil_free_fn>,
) -> c_int {
    let Some(r) = req.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL request");
    };
    if key.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL key");
    }
    let key = CStr::from_ptr(key).to_bytes();
    let locals = r.req.extensions().get_or_insert_with(CLocals::default);
    let old = {
        let mut g = locals.0.lock().unwrap_or_else(|e| e.into_inner());
        let new = Local {
            key: key.into(),
            val,
            free,
        };
        match g.iter_mut().find(|l| &*l.key == key) {
            Some(slot) => Some(std::mem::replace(slot, new)),
            None => {
                g.push(new);
                None
            }
        }
    };
    // 解放関数はロックの外で呼ぶ（中で veil_request_* を呼ばれてもよいように）
    if let Some(o) = old {
        o.release();
    }
    VEIL_OK
}

/// The value stored with `veil_request_set_local`, or NULL.
///
/// # Safety
///
/// `req` is a handle passed to the current callback, `key` a NUL-terminated string.
#[no_mangle]
pub unsafe extern "C" fn veil_request_get_local(
    req: *const veil_request_t,
    key: *const c_char,
) -> *mut c_void {
    let Some(r) = req.as_ref() else {
        return ptr::null_mut();
    };
    if key.is_null() {
        return ptr::null_mut();
    }
    let key = CStr::from_ptr(key).to_bytes();
    let Some(locals) = r.req.extensions().get::<CLocals>() else {
        return ptr::null_mut();
    };
    let g = locals.0.lock().unwrap_or_else(|e| e.into_inner());
    g.iter()
        .find(|l| &*l.key == key)
        .map(|l| l.val)
        .unwrap_or(ptr::null_mut())
}

// ===================== ミドルウェア =====================

/// Middleware before the handler: `VEIL_MW_CONTINUE` (0) goes on, `VEIL_MW_STOP` (1) answers with `resp`, a negative value gives 500.
#[allow(non_camel_case_types)]
pub type veil_before_fn =
    unsafe extern "C" fn(*const veil_request_t, *mut veil_response_t, *mut c_void) -> c_int;
/// Middleware after the handler (may change `resp`).
#[allow(non_camel_case_types)]
pub type veil_after_fn =
    unsafe extern "C" fn(*const veil_request_t, *mut veil_response_t, *mut c_void);

/// Go on to the next middleware or the handler.
pub const VEIL_MW_CONTINUE: c_int = 0;
/// Stop and answer with `resp`.
pub const VEIL_MW_STOP: c_int = 1;

/// One middleware's functions (copied into each route's chain).
#[derive(Clone, Copy)]
pub(crate) struct MwFn {
    before: Option<veil_before_fn>,
    after: Option<veil_after_fn>,
    ud: UserData,
}

/// A registered middleware.
pub(crate) struct CMiddleware {
    prefix: String,
    f: MwFn,
}

/// A function to call after the server stopped, with its user data.
pub(crate) type Hook = (unsafe extern "C" fn(*mut c_void), UserData);

// `prefix` がパスのセグメントの境界で前方一致するか（`"/"` は全部。`App::wrap_on` と同じ規則）
fn path_has_prefix(path: &str, prefix: &str) -> bool {
    let prefix = prefix.trim_end_matches('/');
    prefix.is_empty()
        || path == prefix
        || path
            .strip_prefix(prefix)
            .is_some_and(|r| r.starts_with('/'))
}

/// The middleware of the route `path`, outermost first (the last registered is outermost, as with `App::wrap_on`).
pub(crate) fn chain_for(mws: &[CMiddleware], path: &str) -> Arc<[MwFn]> {
    mws.iter()
        .rev()
        .filter(|m| path_has_prefix(path, &m.prefix))
        .map(|m| m.f)
        .collect()
}

/// Runs the middleware chain and the handler. A failing handler or `before` sets `resp` to 500. After the response
/// started streaming, the `after` functions are not called (the headers were sent).
///
/// # Safety
///
/// `req` and `resp` belong to this request; the C functions and user data follow their registration contracts.
pub(crate) unsafe fn run_chain(
    mw: &[MwFn],
    req: &veil_request_t,
    resp: &mut veil_response_t,
    func: veil_handler_fn,
    ud: UserData,
) {
    let mut passed = 0usize;
    let mut stopped = false;
    for m in mw {
        if let Some(b) = m.before {
            match b(req, resp, m.ud.get()) {
                VEIL_MW_CONTINUE => {}
                VEIL_MW_STOP => {
                    stopped = true;
                    break;
                }
                _ => {
                    resp.resp = Response::error(500, "middleware returned an error");
                    stopped = true;
                    break;
                }
            }
        }
        passed += 1;
    }
    if !stopped && func(req, resp, ud.get()) != 0 {
        resp.resp = Response::error(500, "handler returned an error");
    }
    if resp.ctx.as_ref().is_some_and(|c| c.is_streaming()) {
        return;
    }
    for m in mw[..passed].iter().rev() {
        if let Some(a) = m.after {
            a(req, resp, m.ud.get());
        }
    }
}

/// Registers C middleware for the C routes under `prefix` (NULL is `"/"`; not gRPC, static files or Rust
/// plugins). Either `before` or `after` may be NULL. They run on the handler's thread (the handler pool, or the
/// worker for INLINE routes). The last registered is outermost (its `before` runs first and its `after` last).
///
/// # Safety
///
/// `cfg` is a valid configuration handle, `prefix` NULL or a NUL-terminated string; `user_data` as for `veil_register_route`.
#[no_mangle]
pub unsafe extern "C" fn veil_register_middleware(
    cfg: *mut veil_config_t,
    prefix: *const c_char,
    before: Option<veil_before_fn>,
    after: Option<veil_after_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    if before.is_none() && after.is_none() {
        return fail(VEIL_ERR_INVALID_ARG, "middleware needs before or after");
    }
    let prefix = tryc!(opt_cstr(prefix)).unwrap_or("/");
    if !prefix.starts_with('/') {
        return fail(VEIL_ERR_INVALID_ARG, "prefix must start with '/'");
    }
    cfg.middleware.push(CMiddleware {
        prefix: prefix.to_string(),
        f: MwFn {
            before,
            after,
            ud: UserData(user_data),
        },
    });
    VEIL_OK
}

// ===================== 応答後の処理 =====================

/// Calls `func(user_data)` once on the handler pool after the response was sent (also when the client left first);
/// for a streamed response, after the body ended. Several calls run in registration order.
///
/// # Safety
///
/// `resp` is a handle passed to the current callback; `user_data` stays valid until `func` runs and may move to another thread.
#[no_mangle]
pub unsafe extern "C" fn veil_response_after_send(
    resp: *mut veil_response_t,
    func: Option<veil_free_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(r) = resp.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL response");
    };
    let Some(func) = func else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL function");
    };
    let ud = UserData(user_data);
    r.resp = std::mem::replace(&mut r.resp, Response::new(200)).after_send(move || {
        // after_send はワーカーで走るので、C の関数はハンドラ用プールへ渡す
        veil::runtime::offload::spawn_handler(move || {
            // SAFETY: 上の契約
            unsafe { func(ud.get()) }
        });
    });
    VEIL_OK
}

// ===================== ライフサイクル =====================

/// A startup function (before the listeners open, on the thread of `veil_run`). A non-zero result stops startup and
/// `veil_run` returns `VEIL_ERR_RUNTIME` (they run in registration order up to the first failure).
#[allow(non_camel_case_types)]
pub type veil_startup_fn = unsafe extern "C" fn(*mut c_void) -> c_int;

/// Adds a startup function.
///
/// # Safety
///
/// `cfg` is a valid configuration handle; `user_data` stays valid until `veil_run` returns.
#[no_mangle]
pub unsafe extern "C" fn veil_config_on_startup(
    cfg: *mut veil_config_t,
    func: Option<veil_startup_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let Some(func) = func else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL function");
    };
    let ud = UserData(user_data);
    cfg.extra.push(Box::new(move |app: App| {
        app.on_setup(move || {
            // SAFETY: 上の契約
            match catch_unwind(AssertUnwindSafe(|| unsafe { func(ud.get()) })) {
                Ok(0) => Ok(()),
                _ => Err("a startup function failed".to_string()),
            }
        })
    }));
    VEIL_OK
}

/// Adds a function called after the server stopped (after all workers ended, just before `veil_run` returns, on its
/// thread). They run in reverse registration order, only when the server had started.
///
/// # Safety
///
/// `cfg` is a valid configuration handle; `user_data` stays valid until `veil_run` returns.
#[no_mangle]
pub unsafe extern "C" fn veil_config_on_shutdown(
    cfg: *mut veil_config_t,
    func: Option<veil_free_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let Some(func) = func else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL function");
    };
    cfg.shutdown.push((func, UserData(user_data)));
    VEIL_OK
}

/// Calls the shutdown functions in reverse order.
pub(crate) fn run_shutdown(hooks: Vec<Hook>) {
    for (f, ud) in hooks.into_iter().rev() {
        // SAFETY: veil_config_on_shutdown の契約
        if catch_unwind(AssertUnwindSafe(|| unsafe { f(ud.get()) })).is_err() {
            ftlog::error!("[api] a shutdown function panicked");
        }
    }
}

/// A worker's information (passed to `veil_config_on_worker_start` functions, valid during the call).
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct veil_worker_info_t {
    /// `sizeof(veil_worker_info_t)`
    pub size: usize,
    /// The listener's name (not NUL-terminated).
    pub listener: *const c_char,
    pub listener_len: usize,
    /// The worker's number within its listener (`0..threads`).
    pub index: usize,
}

/// A worker-start function (called on the worker thread; must not block).
#[allow(non_camel_case_types)]
pub type veil_worker_start_fn = unsafe extern "C" fn(*const veil_worker_info_t, *mut c_void);

/// Adds a function called when each worker thread starts (each listener has its own workers). It runs on the
/// worker, so it must return quickly without blocking (for thread-local setup).
///
/// # Safety
///
/// `cfg` is a valid configuration handle; `user_data` stays valid while the server runs and is safe to use from several threads.
#[no_mangle]
pub unsafe extern "C" fn veil_config_on_worker_start(
    cfg: *mut veil_config_t,
    func: Option<veil_worker_start_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let Some(func) = func else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL function");
    };
    let ud = UserData(user_data);
    cfg.extra.push(Box::new(move |app: App| {
        app.on_worker_start(move |w: &veil::api::app::WorkerInfo| {
            let info = veil_worker_info_t {
                size: std::mem::size_of::<veil_worker_info_t>(),
                listener: w.listener.as_ptr() as *const c_char,
                listener_len: w.listener.len(),
                index: w.index,
            };
            // SAFETY: info はこの呼び出しの間だけ生きる。user_data は上の契約
            if catch_unwind(AssertUnwindSafe(|| unsafe { func(&info, ud.get()) })).is_err() {
                ftlog::error!("[api] a worker start function panicked");
            }
        })
    }));
    VEIL_OK
}

// ===================== CORS =====================

/// CORS settings for `veil_config_cors` (zero-filled means defaults; `size` is `sizeof(veil_cors_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_cors_t {
    pub size: usize,
    /// Only routes under this prefix (NULL is `"/"`).
    pub prefix: *const c_char,
    pub origins: *const *const c_char,
    pub n_origins: usize,
    /// Non-zero: every origin (not together with `credentials`).
    pub any_origin: c_int,
    /// None means the default (GET, HEAD, POST, PUT, PATCH, DELETE).
    pub methods: *const *const c_char,
    pub n_methods: usize,
    pub headers: *const *const c_char,
    pub n_headers: usize,
    /// Non-zero: allow every requested header.
    pub any_header: c_int,
    pub expose_headers: *const *const c_char,
    pub n_expose_headers: usize,
    /// Non-zero: `Access-Control-Allow-Credentials: true`.
    pub credentials: c_int,
    /// Seconds a preflight result may be cached (0 is the default).
    pub max_age_secs: u64,
}

/// Enables CORS (answers preflight OPTIONS requests and adds the headers to responses). The settings are checked
/// here (for example `any_origin` with `credentials` is `VEIL_ERR_CONFIG`). It applies to every route except static files.
///
/// # Safety
///
/// `cfg` is a valid configuration handle, `cors` a valid `veil_cors_t` (its strings and arrays are read during this call only).
#[no_mangle]
pub unsafe extern "C" fn veil_config_cors(
    cfg: *mut veil_config_t,
    cors: *const veil_cors_t,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let c = tryc!(read_sized(cors, "veil_cors_t"));
    let prefix = tryc!(opt_cstr(c.prefix)).unwrap_or("/");
    let mut b = veil_kit::cors::Cors::new()
        .allow_origins(tryc!(str_array(c.origins, c.n_origins, "origins")))
        .allow_headers(tryc!(str_array(c.headers, c.n_headers, "headers")))
        .expose_headers(tryc!(str_array(
            c.expose_headers,
            c.n_expose_headers,
            "expose_headers"
        )))
        .allow_credentials(c.credentials != 0)
        .on(prefix);
    if c.any_origin != 0 {
        b = b.allow_any_origin();
    }
    if c.any_header != 0 {
        b = b.allow_any_header();
    }
    if c.n_methods > 0 {
        b = b.allow_methods(tryc!(str_array(c.methods, c.n_methods, "methods")));
    }
    if c.max_age_secs > 0 {
        b = b.max_age(std::time::Duration::from_secs(c.max_age_secs));
    }
    match b.build() {
        Ok(policy) => {
            cfg.extra.push(Box::new(move |app: App| app.with(policy)));
            VEIL_OK
        }
        Err(e) => fail(VEIL_ERR_CONFIG, e.to_string()),
    }
}

// ===================== 静的ディレクトリ配信 =====================

/// Options for `veil_config_serve_dir` (zero-filled means defaults; `size` is `sizeof(veil_serve_options_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_serve_options_t {
    pub size: usize,
    /// The file for directory requests (NULL is `index.html`).
    pub index: *const c_char,
    /// `open_file_cache` and `static_file_cache` (tri-state, default on).
    pub cache: c_int,
    /// On-the-fly compression for HTTP/2 and HTTP/3 (tri-state, default off).
    pub compression: c_int,
    /// Extra keys for the `[[route]]` (the body of a TOML table; may be NULL).
    pub extra_toml: *const c_char,
}

/// Serves the directory `dir` under `prefix` with veil's file server (sendfile, kTLS, Range, ETag). Requests do not
/// go through handlers (nor C middleware). `opts` NULL means defaults.
///
/// # Safety
///
/// `cfg` is a valid configuration handle, `prefix` and `dir` NUL-terminated strings, `opts` NULL or a valid `veil_serve_options_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_config_serve_dir(
    cfg: *mut veil_config_t,
    prefix: *const c_char,
    dir: *const c_char,
    opts: *const veil_serve_options_t,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let prefix = tryc!(cstr(prefix));
    if !prefix.starts_with('/') {
        return fail(VEIL_ERR_INVALID_ARG, "prefix must start with '/'");
    }
    let dir = tryc!(cstr(dir));
    if dir.is_empty() {
        return fail(VEIL_ERR_INVALID_ARG, "empty directory");
    }
    let mut so = veil::api::ServeOptions::default();
    if !opts.is_null() {
        let o = tryc!(read_sized(opts, "veil_serve_options_t"));
        so.index = tryc!(opt_cstr(o.index)).map(str::to_string);
        if let Some(on) = tri(o.cache) {
            so.open_file_cache = on;
            so.static_file_cache = on;
        }
        if let Some(on) = tri(o.compression) {
            so.compression = on;
        }
        if let Some(t) = tryc!(opt_cstr(o.extra_toml)) {
            match t.parse::<toml::Table>() {
                Ok(t) => so.extra = Some(t),
                Err(e) => return fail(VEIL_ERR_CONFIG, format!("invalid route options: {}", e)),
            }
        }
    }
    let (prefix, dir) = (prefix.to_string(), dir.to_string());
    cfg.extra
        .push(Box::new(move |app: App| app.serve_dir(&prefix, &dir, so)));
    VEIL_OK
}

// ===================== gRPC reflection =====================

/// Registers gRPC server reflection (`grpc.reflection.v1` and `v1alpha`). `data` is a `FileDescriptorSet`
/// (`protoc --include_imports --descriptor_set_out`), read during this call only; an invalid one is `VEIL_ERR_INVALID_ARG`.
/// The types are also listed in OpenAPI (as with `veil_grpc_descriptors`).
///
/// # Safety
///
/// `cfg` is a valid configuration handle; `data` has `len` readable bytes.
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_reflection(
    cfg: *mut veil_config_t,
    data: *const u8,
    len: usize,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    if data.is_null() || len == 0 {
        return fail(VEIL_ERR_INVALID_ARG, "empty descriptor data");
    }
    let set = std::slice::from_raw_parts(data, len).to_vec();
    if let Err(e) = veil::api::grpc::reflection::check_descriptor_sets(&[&set]) {
        return fail(VEIL_ERR_INVALID_ARG, e);
    }
    cfg.extra
        .push(Box::new(move |app: App| app.grpc_reflection(&[&set])));
    VEIL_OK
}

// ===================== WebSocket の拡張 =====================

/// Options for `veil_ws_accept_ex` (zero-filled means defaults; `size` is `sizeof(veil_ws_options_t)` or 0).
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_ws_options_t {
    pub size: usize,
    /// Subprotocols the server speaks, comma-separated, preferred first (NULL is none).
    pub protocols: *const c_char,
    /// Accepted `Origin` values, comma-separated (others get 403; NULL skips the check).
    pub origins: *const c_char,
    /// Maximum assembled message size in bytes (0 is `[api] websocket_max_message_size`).
    pub max_message_size: usize,
    /// Answer pings with pongs automatically (tri-state, default on).
    pub auto_pong: c_int,
}

fn csv(s: &str) -> Vec<&str> {
    s.split(',')
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .collect()
}

/// `veil_ws_accept` with options (subprotocol selection, `Origin` check, message size limit). The chosen subprotocol
/// is returned by `veil_ws_protocol`. When the origin is not allowed, resp is 403 and the result is `VEIL_ERR_STATE`.
///
/// # Safety
///
/// As for `veil_ws_accept`; `opts` is NULL (defaults) or a valid `veil_ws_options_t`.
#[no_mangle]
pub unsafe extern "C" fn veil_ws_accept_ex(
    req: *const veil_request_t,
    resp: *mut veil_response_t,
    callbacks: *const super::ext::veil_ws_callbacks_t,
    user_data: *mut c_void,
    opts: *const veil_ws_options_t,
) -> c_int {
    let o = if opts.is_null() {
        None
    } else {
        Some(tryc!(read_sized(opts, "veil_ws_options_t")))
    };
    let (protocols, origins) = match &o {
        Some(o) => (
            tryc!(opt_cstr(o.protocols)).map(csv),
            tryc!(opt_cstr(o.origins)).map(csv),
        ),
        None => (None, None),
    };
    let max = o.map(|o| o.max_message_size).unwrap_or(0);
    let pong = o.and_then(|o| tri(o.auto_pong));
    super::ext::ws_accept(req, resp, callbacks, user_data, move |mut up| {
        if let Some(p) = protocols {
            up = up.protocols(p);
        }
        if let Some(os) = &origins {
            up = up.origin(os);
        }
        if max > 0 {
            up = up.max_message_size(max);
        }
        if let Some(on) = pong {
            up = up.auto_pong(on);
        }
        up
    })
}

/// The chosen subprotocol (not NUL-terminated), or NULL with length 0.
///
/// # Safety
///
/// `ws` is a handle valid from `on_open` until `on_close` returns.
#[no_mangle]
pub unsafe extern "C" fn veil_ws_protocol(
    ws: *const super::ext::veil_ws_t,
    len: *mut usize,
) -> *const c_char {
    match ws.as_ref().and_then(|w| w.protocol.as_deref()) {
        Some(p) => out_bytes(p.as_bytes(), len),
        None => out_none(len),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{veil_config_free, veil_config_new};
    use std::ffi::CString;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use veil::api::Request;

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn take(p: *const c_char, len: usize) -> String {
        assert!(!p.is_null());
        // SAFETY: テスト内でアクセサが返した (ptr, len)
        let b = unsafe { std::slice::from_raw_parts(p as *const u8, len) };
        String::from_utf8(b.to_vec()).unwrap()
    }

    fn req(headers: &[(&'static str, &'static str)]) -> Request {
        Request::new(
            "GET",
            "/a",
            headers
                .iter()
                .map(|(n, v)| {
                    (
                        Bytes::from_static(n.as_bytes()),
                        Bytes::from_static(v.as_bytes()),
                    )
                })
                .collect(),
            "",
        )
    }

    fn values(r: &Response, name: &str) -> Vec<String> {
        r.headers()
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| String::from_utf8(v.to_vec()).unwrap())
            .collect()
    }

    #[test]
    fn headers_versions_and_add_header() {
        let r = req(&[("X-Multi", "a"), ("host", "h"), ("x-multi", "b")]).with_tls(true);
        let ((), resp) = crate::internal::with_test_handles(r, |rq, rs| unsafe {
            assert_eq!(veil_request_header_count(rq), 3);
            let (mut n, mut nl, mut v, mut vl) = (ptr::null(), 0, ptr::null(), 0);
            assert_eq!(
                veil_request_header_at(rq, 1, &mut n, &mut nl, &mut v, &mut vl),
                VEIL_OK
            );
            assert_eq!((take(n, nl), take(v, vl)), ("host".into(), "h".into()));
            assert_eq!(
                veil_request_header_at(rq, 3, &mut n, &mut nl, &mut v, &mut vl),
                VEIL_ERR_INVALID_ARG
            );
            let (mut it, mut len, mut got) = (0usize, 0usize, vec![]);
            let name = c("x-multi");
            loop {
                let p = veil_request_header_next(rq, name.as_ptr(), &mut it, &mut len);
                if p.is_null() {
                    break;
                }
                got.push(take(p, len));
            }
            assert_eq!(got, ["a", "b"]);
            assert_eq!(veil_request_version(rq), VEIL_HTTP_1_1);
            assert_eq!(veil_request_is_tls(rq), 1);
            assert_eq!(veil_request_version(ptr::null()), 0);
            let sc = c("set-cookie");
            assert_eq!(
                veil_response_add_header(rs, sc.as_ptr(), c("a=1").as_ptr()),
                VEIL_OK
            );
            assert_eq!(
                veil_response_add_header(rs, sc.as_ptr(), c("b=2").as_ptr()),
                VEIL_OK
            );
            assert_eq!(
                veil_response_add_header(rs, c("x").as_ptr(), c("a\r\nb").as_ptr()),
                VEIL_ERR_INVALID_ARG
            );
        });
        assert_eq!(values(&resp, "set-cookie"), ["a=1", "b=2"]);
    }

    static FREED: AtomicUsize = AtomicUsize::new(0);
    unsafe extern "C" fn count_free(_: *mut c_void) {
        FREED.fetch_add(1, Ordering::SeqCst);
    }

    #[test]
    fn locals_are_replaced_and_freed_once() {
        let before = FREED.load(Ordering::SeqCst);
        let (a, b) = (1u8, 2u8);
        let pa = &a as *const u8 as *mut c_void;
        let pb = &b as *const u8 as *mut c_void;
        let k = c("k");
        crate::internal::with_test_handles(req(&[]), |rq, _| unsafe {
            assert!(veil_request_get_local(rq, k.as_ptr()).is_null());
            assert_eq!(
                veil_request_set_local(rq, k.as_ptr(), pa, Some(count_free)),
                VEIL_OK
            );
            assert_eq!(veil_request_get_local(rq, k.as_ptr()), pa);
            // 置き換えで前の値を解放する
            assert_eq!(
                veil_request_set_local(rq, k.as_ptr(), pb, Some(count_free)),
                VEIL_OK
            );
            assert_eq!(FREED.load(Ordering::SeqCst), before + 1);
            assert_eq!(veil_request_get_local(rq, k.as_ptr()), pb);
        });
        // 要求の終わりで残りを解放する
        assert_eq!(FREED.load(Ordering::SeqCst), before + 2);
    }

    // ミドルウェアの順序: user_data に名前を入れ、呼ばれた順に記録する
    static TRACE: Mutex<Vec<String>> = Mutex::new(Vec::new());
    fn name(ud: *mut c_void) -> &'static str {
        // SAFETY: テストの user_data は &'static str への参照
        unsafe { *(ud as *const &'static str) }
    }
    unsafe extern "C" fn mw_b(
        _: *const veil_request_t,
        rs: *mut veil_response_t,
        ud: *mut c_void,
    ) -> c_int {
        let n = name(ud);
        TRACE.lock().unwrap().push(format!("b:{}", n));
        match n {
            "stop" => {
                (*rs).resp = Response::error(418, "stopped");
                VEIL_MW_STOP
            }
            "fail" => -1,
            _ => VEIL_MW_CONTINUE,
        }
    }
    unsafe extern "C" fn mw_a(_: *const veil_request_t, _: *mut veil_response_t, ud: *mut c_void) {
        TRACE.lock().unwrap().push(format!("a:{}", name(ud)));
    }
    unsafe extern "C" fn handler(
        _: *const veil_request_t,
        _: *mut veil_response_t,
        _: *mut c_void,
    ) -> c_int {
        TRACE.lock().unwrap().push("h".into());
        0
    }

    #[test]
    fn middleware_order_stop_and_errors() {
        static OUTER: &str = "outer";
        static INNER: &str = "inner";
        static OTHER: &str = "other";
        static STOP: &str = "stop";
        static FAIL: &str = "fail";
        fn ud(s: &'static &'static str) -> *mut c_void {
            s as *const &str as *mut c_void
        }
        let (api, other, stop, fail) = (c("/api"), c("/other"), c("/api/stop"), c("/api/fail"));
        unsafe {
            let cfg = veil_config_new();
            let null = ptr::null_mut();
            assert_eq!(
                veil_register_middleware(cfg, ptr::null(), None, None, null),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_register_middleware(cfg, c("x").as_ptr(), Some(mw_b), None, null),
                VEIL_ERR_INVALID_ARG
            );
            let reg = |p: *const c_char, a: Option<veil_after_fn>, u| {
                veil_register_middleware(cfg, p, Some(mw_b), a, u)
            };
            assert_eq!(reg(api.as_ptr(), Some(mw_a), ud(&INNER)), VEIL_OK);
            assert_eq!(reg(ptr::null(), Some(mw_a), ud(&OUTER)), VEIL_OK);
            assert_eq!(reg(other.as_ptr(), None, ud(&OTHER)), VEIL_OK);
            assert_eq!(reg(stop.as_ptr(), None, ud(&STOP)), VEIL_OK);
            assert_eq!(reg(fail.as_ptr(), None, ud(&FAIL)), VEIL_OK);
            assert!(path_has_prefix("/api/x", "/api/") && !path_has_prefix("/apix", "/api"));

            let run = |path: &str| {
                let chain = chain_for(&(*cfg).middleware, path);
                TRACE.lock().unwrap().clear();
                let r = crate::invoke(
                    handler,
                    UserData(ptr::null_mut()),
                    req(&[]),
                    None,
                    ptr::null_mut(),
                    &chain,
                );
                (r.status_code(), TRACE.lock().unwrap().join(","))
            };
            assert_eq!(
                run("/api/users"),
                (200, "b:outer,b:inner,h,a:inner,a:outer".to_string())
            );
            // 外側（後に登録した stop）が打ち切ると、内側も後処理も呼ばない
            assert_eq!(run("/api/stop"), (418, "b:stop".to_string()));
            assert_eq!(run("/api/fail"), (500, "b:fail".to_string()));
            assert_eq!(run("/z"), (200, "b:outer,h,a:outer".to_string()));
            veil_config_free(cfg);
        }
    }

    #[test]
    fn after_send_is_attached() {
        unsafe extern "C" fn nop(_: *mut c_void) {}
        let ((), mut r) = crate::internal::with_test_handles(req(&[]), |_, rs| unsafe {
            assert_eq!(
                veil_response_after_send(rs, None, ptr::null_mut()),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_response_after_send(rs, Some(nop), ptr::null_mut()),
                VEIL_OK
            );
        });
        assert!(r.take_after_send().is_some());
    }

    #[test]
    fn config_helpers_check_their_arguments() {
        unsafe extern "C" fn nop(_: *mut c_void) {}
        unsafe extern "C" fn start(_: *mut c_void) -> c_int {
            0
        }
        unsafe extern "C" fn worker(_: *const veil_worker_info_t, _: *mut c_void) {}
        let null = ptr::null_mut();
        unsafe {
            let cfg = veil_config_new();
            assert_eq!(veil_config_on_startup(cfg, Some(start), null), VEIL_OK);
            assert_eq!(
                veil_config_on_startup(cfg, None, null),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(veil_config_on_shutdown(cfg, Some(nop), null), VEIL_OK);
            assert_eq!(
                veil_config_on_worker_start(cfg, Some(worker), null),
                VEIL_OK
            );
            assert_eq!(
                veil_config_on_worker_start(ptr::null_mut(), Some(worker), null),
                VEIL_ERR_INVALID_ARG
            );

            // CORS: 0 埋め（size 0）は既定、any_origin と credentials の併用は設定の誤り、小さすぎる size は拒否
            let o = c("https://app.example.com");
            let origins = [o.as_ptr()];
            let mut cors: veil_cors_t = std::mem::zeroed();
            cors.origins = origins.as_ptr();
            cors.n_origins = 1;
            assert_eq!(veil_config_cors(cfg, &cors), VEIL_OK);
            cors.any_origin = 1;
            cors.credentials = 1;
            assert_eq!(veil_config_cors(cfg, &cors), VEIL_ERR_CONFIG);
            cors.size = 4;
            assert_eq!(veil_config_cors(cfg, &cors), VEIL_ERR_INVALID_ARG);
            cors = std::mem::zeroed();
            cors.n_origins = 1; // NULL の配列に 1 個
            assert_eq!(veil_config_cors(cfg, &cors), VEIL_ERR_INVALID_ARG);

            // 静的ディレクトリ: 接頭辞は "/" で始まる
            let dir = c("/tmp");
            assert_eq!(
                veil_config_serve_dir(cfg, c("static").as_ptr(), dir.as_ptr(), ptr::null()),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_config_serve_dir(cfg, c("/static").as_ptr(), dir.as_ptr(), ptr::null()),
                VEIL_OK
            );

            // gRPC reflection: 壊れた FileDescriptorSet・NULL は拒否
            let junk = [0xffu8, 0xff, 0xff];
            assert_eq!(
                veil_grpc_reflection(cfg, junk.as_ptr(), junk.len()),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_grpc_reflection(cfg, ptr::null(), 0),
                VEIL_ERR_INVALID_ARG
            );
            veil_config_free(cfg);
        }
    }

    #[test]
    fn shutdown_hooks_run_in_reverse() {
        static ORDER: Mutex<Vec<usize>> = Mutex::new(Vec::new());
        unsafe extern "C" fn push(ud: *mut c_void) {
            ORDER.lock().unwrap().push(ud as usize);
        }
        let hooks: Vec<Hook> = (1..=3usize)
            .map(|i| {
                (
                    push as unsafe extern "C" fn(*mut c_void),
                    UserData(i as *mut c_void),
                )
            })
            .collect();
        run_shutdown(hooks);
        assert_eq!(*ORDER.lock().unwrap(), [3, 2, 1]);
    }
}
