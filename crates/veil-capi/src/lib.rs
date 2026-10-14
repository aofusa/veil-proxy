//! veil API サーバ基盤ライブラリの C API（F-193）。
//!
//! `libveil.so` / `libveil.a` と `include/veil.h` を提供する。C のコールバックを HTTP ハンドラとして
//! 登録し、`veil_run` で本体と同じ io_uring/epoll ランタイム上のサーバを起動する。
//!
//! # スレッドモデル
//!
//! - 既定ではコールバックを **ハンドラ用プール**（`veil::runtime::offload::offload_handler`）で実行する。
//!   ワーカースレッドをブロックしないが、コールバックは複数スレッドから同時に呼ばれ得る。
//! - `VEIL_ROUTE_INLINE` を付けたルートは **ワーカースレッド上** で直接実行する。短時間で終わる処理専用。
//!   ブロックするとそのワーカーの全接続が止まる。
//!
//! ホストプロセスのグローバルアロケータは置き換えない（`#[global_allocator]` を定義しない）。

use std::cell::RefCell;
use std::ffi::{c_char, c_int, c_void, CStr, CString};
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;

use bytes::Bytes;
use veil::api::bridge::HandlerContext;
use veil::api::{App, Config, HandlerFn, OperationMeta, Request, Response};

// ===================== BoringSSL 互換シンボルの橋渡し =====================

// quiche（BoringSSL 向けの宣言）は証明書チェーンの取得に `sk_num` / `sk_value` を参照するが、
// Linux で共有している aws-lc は `OPENSSL_sk_num` / `OPENSSL_sk_value` の名前でしか公開しない。
// 実行ファイルでは未使用コードとして落ちるが、ライブラリ（cdylib）は全シンボルを残すため、
// C プログラムとのリンクで未解決になる。ここで同名の薄い橋渡しを定義して解決する。
#[cfg(target_os = "linux")]
mod boringssl_compat {
    use std::ffi::c_void;

    extern "C" {
        fn OPENSSL_sk_num(stack: *const c_void) -> usize;
        fn OPENSSL_sk_value(stack: *const c_void, idx: usize) -> *mut c_void;
    }

    /// # Safety
    ///
    /// `stack` は aws-lc の `OPENSSL_STACK`（quiche が TLS ハンドシェイクで得たもの）。
    #[no_mangle]
    pub unsafe extern "C" fn sk_num(stack: *const c_void) -> usize {
        OPENSSL_sk_num(stack)
    }

    /// # Safety
    ///
    /// `stack` は aws-lc の `OPENSSL_STACK`、`idx` は範囲内。
    #[no_mangle]
    pub unsafe extern "C" fn sk_value(stack: *const c_void, idx: usize) -> *mut c_void {
        OPENSSL_sk_value(stack, idx)
    }
}

// ===================== エラーコード・フラグ =====================

/// 成功。
pub const VEIL_OK: c_int = 0;
/// 引数が NULL・不正（メソッド名・ヘッダ名・JSON など）。
pub const VEIL_ERR_INVALID_ARG: c_int = -1;
/// 設定の読み込み・上書きの失敗。
pub const VEIL_ERR_CONFIG: c_int = -2;
/// 起動・実行時のエラー。
pub const VEIL_ERR_RUNTIME: c_int = -3;
/// 文字列が UTF-8 ではない。
pub const VEIL_ERR_UTF8: c_int = -4;
/// 相手（クライアント）が切断した・ストリームが閉じている（F-210）。
pub const VEIL_ERR_CLOSED: c_int = -5;
/// 呼べない状態（INLINE のルートでのストリーミング・単項の 2 回目の送信等。F-210）。
pub const VEIL_ERR_STATE: c_int = -6;

/// コールバックをワーカースレッド上で直接実行する（既定はハンドラ用プール）。
pub const VEIL_ROUTE_INLINE: u32 = 1;
/// 要求本文を溜めずにコールバックを呼ぶ（`veil_request_body_read` で順に読む。F-210）。
pub const VEIL_ROUTE_STREAM_BODY: u32 = 2;

// ===================== 最後のエラー =====================

thread_local! {
    static LAST_ERROR: RefCell<Option<CString>> = const { RefCell::new(None) };
}

fn set_last_error(msg: impl AsRef<str>) {
    // NUL を含むメッセージは置換して必ず CString にする。
    let c = CString::new(msg.as_ref().replace('\0', " ")).unwrap_or_default();
    LAST_ERROR.with(|e| *e.borrow_mut() = Some(c));
}

fn fail(code: c_int, msg: impl AsRef<str>) -> c_int {
    set_last_error(msg);
    code
}

/// 呼び出したスレッドで最後に起きたエラーのメッセージ（NUL 終端）。無ければ NULL。
///
/// # Safety
///
/// 返るポインタは同じスレッドで次に本ライブラリの関数がエラーを起こすまで有効。解放してはならない。
#[no_mangle]
pub unsafe extern "C" fn veil_last_error() -> *const c_char {
    LAST_ERROR.with(|e| match &*e.borrow() {
        Some(c) => c.as_ptr(),
        None => ptr::null(),
    })
}

/// ライブラリのバージョン文字列（NUL 終端、静的）。
///
/// # Safety
///
/// 返るポインタは静的領域で解放してはならない。
#[no_mangle]
pub unsafe extern "C" fn veil_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr() as *const c_char
}

// ===================== 型 =====================

/// C のハンドラ関数。0 を返すと `resp` を送り、非 0 なら 500 を返す。
#[allow(non_camel_case_types)]
pub type veil_handler_fn =
    unsafe extern "C" fn(*const veil_request_t, *mut veil_response_t, *mut c_void) -> c_int;

/// 登録待ちの C ルート。
struct CRoute {
    name: String,
    path: String,
    methods: Vec<String>,
    func: veil_handler_fn,
    user_data: UserData,
    inline: bool,
    stream_body: bool,
    meta: OperationMeta,
}

/// 設定 + 登録待ちルート（不透明）。
#[allow(non_camel_case_types)]
pub struct veil_config_t {
    config: Config,
    routes: Vec<CRoute>,
    /// gRPC・L4 の登録（`veil_run` で `App` に足す。F-210）
    extra: Vec<Box<dyn FnOnce(App) -> App + Send>>,
}

/// リクエスト（不透明）。コールバックの間だけ有効。
#[allow(non_camel_case_types)]
pub struct veil_request_t {
    req: Request,
    /// 復号した文字列の置き場。`String` のヒープは `Vec` が伸びても動かないので、
    /// ここに積んだ文字列へのポインタはコールバックの間有効。
    arena: RefCell<Vec<String>>,
    /// ハンドラ用プールで動くときの文脈（要求本文の読み取り。INLINE は NULL。F-210）
    ctx: *mut HandlerContext,
    /// 読みかけの本文の断片（`veil_request_body_read` の残り）
    body_pending: RefCell<Bytes>,
}

/// レスポンス（不透明）。コールバックの間だけ有効。
#[allow(non_camel_case_types)]
pub struct veil_response_t {
    resp: Response,
    /// ハンドラ用プールで動くときの文脈（ストリーミング。INLINE は NULL。F-210）
    ctx: *mut HandlerContext,
}

impl veil_request_t {
    fn new(req: Request) -> veil_request_t {
        veil_request_t {
            req,
            arena: RefCell::new(Vec::new()),
            ctx: ptr::null_mut(),
            body_pending: RefCell::new(Bytes::new()),
        }
    }

    /// 文字列を置き場へ移してポインタを返す。
    fn keep(&self, s: String) -> (*const c_char, usize) {
        let mut a = self.arena.borrow_mut();
        a.push(s);
        let last = a.last().expect("just pushed");
        (last.as_ptr() as *const c_char, last.len())
    }
}

/// `user_data` を別スレッド（ハンドラ用プール）へ渡すための包み。
///
/// # Safety 契約
///
/// `veil_register_route*` に渡した `user_data` は、サーバが動いている間ずっと有効で、
/// **複数スレッドから同時に参照されても安全**でなければならない（C の呼び出し側が保証する）。
/// 本ライブラリはポインタを解釈せず、そのままコールバックへ返すだけ。
#[derive(Clone, Copy)]
struct UserData(*mut c_void);

// SAFETY: 上記の契約。呼び出し側が user_data のスレッド安全性を保証する。
unsafe impl Send for UserData {}
// SAFETY: 同上。
unsafe impl Sync for UserData {}

impl UserData {
    /// 閉包が構造体ごと捕捉するようにメソッド経由で取り出す（フィールド単位の捕捉を避ける）。
    fn get(self) -> *mut c_void {
        self.0
    }
}

// ===================== 引数の変換 =====================

/// NUL 終端の C 文字列を `&str` にする。
unsafe fn cstr<'a>(p: *const c_char) -> Result<&'a str, c_int> {
    if p.is_null() {
        return Err(fail(VEIL_ERR_INVALID_ARG, "NULL string argument"));
    }
    CStr::from_ptr(p)
        .to_str()
        .map_err(|_| fail(VEIL_ERR_UTF8, "string is not valid UTF-8"))
}

macro_rules! tryc {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(code) => return code,
        }
    };
}

// ===================== 設定 =====================

/// ライブラリ既定の設定を作る。`veil_run` か `veil_config_free` で解放する。
///
/// # Safety
///
/// 返るハンドルは `veil_run` か `veil_config_free` に 1 回だけ渡す。
#[no_mangle]
pub unsafe extern "C" fn veil_config_new() -> *mut veil_config_t {
    Box::into_raw(Box::new(veil_config_t {
        config: Config::new(),
        routes: Vec::new(),
        extra: Vec::new(),
    }))
}

/// config.toml を取り込んだ設定を作る。失敗したら NULL（`veil_last_error` に理由）。
///
/// # Safety
///
/// `path` は NUL 終端の有効な C 文字列。返るハンドルの扱いは `veil_config_new` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_config_from_file(path: *const c_char) -> *mut veil_config_t {
    let path = match cstr(path) {
        Ok(p) => p,
        Err(_) => return ptr::null_mut(),
    };
    match Config::from_file(path) {
        Ok(config) => Box::into_raw(Box::new(veil_config_t {
            config,
            routes: Vec::new(),
            extra: Vec::new(),
        })),
        Err(e) => {
            set_last_error(e.0);
            ptr::null_mut()
        }
    }
}

/// 任意のキーを上書きする。`path` は `"server.threads"` のようなドット区切り、
/// `toml_value` は TOML の値（文字列なら `"\"abc\""` のように引用符込み）。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`path` と `toml_value` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_config_set(
    cfg: *mut veil_config_t,
    path: *const c_char,
    toml_value: *const c_char,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let path = tryc!(cstr(path));
    let value = tryc!(cstr(toml_value));
    match cfg.config.set_override(&format!("{} = {}", path, value)) {
        Ok(()) => VEIL_OK,
        Err(e) => fail(VEIL_ERR_CONFIG, e.0),
    }
}

/// 文字列値をキーに設定する（TOML の基本文字列として quote する）。
fn set_string(cfg: &mut veil_config_t, path: &str, value: &str) -> c_int {
    // JSON の文字列エスケープは TOML の基本文字列として有効。
    let quoted = serde_json::to_string(value).unwrap_or_default();
    match cfg.config.set_override(&format!("{} = {}", path, quoted)) {
        Ok(()) => VEIL_OK,
        Err(e) => fail(VEIL_ERR_CONFIG, e.0),
    }
}

/// TLS の待ち受けアドレス（`[server].listen`、HTTP/1.1 + h2 ALPN）を設定する。
/// 例: `"0.0.0.0:8443"`。`veil_config_tls` で証明書も設定すること。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`addr` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_config_listen(cfg: *mut veil_config_t, addr: *const c_char) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let addr = tryc!(cstr(addr));
    set_string(cfg, "server.listen", addr)
}

/// 平文の待ち受けアドレス（`[server].listen_cleartext`、HTTP/1.1 + h2c）を設定する。
/// 例: `"127.0.0.1:8080"`（ライブラリ既定。外部に公開するときは `"0.0.0.0:8080"`）。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`addr` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_config_listen_cleartext(
    cfg: *mut veil_config_t,
    addr: *const c_char,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let addr = tryc!(cstr(addr));
    set_string(cfg, "server.listen_cleartext", addr)
}

/// TLS の証明書と秘密鍵（`[tls].cert_path` / `key_path`）を設定する。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`cert_path`・`key_path` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_config_tls(
    cfg: *mut veil_config_t,
    cert_path: *const c_char,
    key_path: *const c_char,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let cert = tryc!(cstr(cert_path));
    let key = tryc!(cstr(key_path));
    let r = set_string(cfg, "tls.cert_path", cert);
    if r != VEIL_OK {
        return r;
    }
    set_string(cfg, "tls.key_path", key)
}

/// ワーカースレッド数（`[server].threads`）を設定する。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_config_threads(cfg: *mut veil_config_t, n: u32) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    match cfg.config.set_override(&format!("server.threads = {}", n)) {
        Ok(()) => VEIL_OK,
        Err(e) => fail(VEIL_ERR_CONFIG, e.0),
    }
}

/// 設定を解放する（`veil_run` に渡した後は呼ばない。NULL は何もしない）。
///
/// # Safety
///
/// `cfg` は `veil_config_new` / `veil_config_from_file` の戻り値で、まだ解放・消費されていないもの。
#[no_mangle]
pub unsafe extern "C" fn veil_config_free(cfg: *mut veil_config_t) {
    if !cfg.is_null() {
        drop(Box::from_raw(cfg));
    }
}

// ===================== ルート登録 =====================

/// メソッドとパスから一意なハンドラ名を作る（例: `GET /users/{id}` → `c_get_users_id`）。
fn route_name(existing: &[CRoute], methods: &[String], path: &str) -> String {
    let mut base = String::from("c_");
    base.push_str(
        &methods
            .first()
            .map(|m| m.to_ascii_lowercase())
            .unwrap_or_else(|| "any".to_string()),
    );
    for c in path.chars() {
        if c.is_ascii_alphanumeric() {
            base.push(c.to_ascii_lowercase());
        } else if !base.ends_with('_') {
            base.push('_');
        }
    }
    while base.ends_with('_') {
        base.pop();
    }
    let mut name = base.clone();
    let mut n = 2;
    while existing.iter().any(|r| r.name == name) {
        name = format!("{}_{}", base, n);
        n += 1;
    }
    name
}

/// ルートを 1 件登録する（`method` が NULL・空・`"*"` なら全メソッド）。
/// コールバックはハンドラ用プールで実行される。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`path` と `method` は NUL 終端の C 文字列（`method` は NULL 可）。
/// `user_data` は **サーバ稼働中ずっと有効で、複数スレッドから同時に使っても安全**であること。
#[no_mangle]
pub unsafe extern "C" fn veil_register_route(
    cfg: *mut veil_config_t,
    path: *const c_char,
    method: *const c_char,
    handler: Option<veil_handler_fn>,
    user_data: *mut c_void,
) -> c_int {
    veil_register_route_ex(cfg, path, method, handler, user_data, 0, ptr::null())
}

/// ルートを 1 件登録する（拡張版）。
///
/// `methods_csv` はカンマ区切りのメソッド（`"GET,POST"`。NULL・空・`"*"` なら全メソッド）。
/// `flags` は `VEIL_ROUTE_INLINE` の OR。`openapi_json` は OpenAPI の Operation Object の JSON（NULL 可）。
///
/// # Safety
///
/// `veil_register_route` と同じ。`openapi_json` は NULL か NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_register_route_ex(
    cfg: *mut veil_config_t,
    path: *const c_char,
    methods_csv: *const c_char,
    handler: Option<veil_handler_fn>,
    user_data: *mut c_void,
    flags: u32,
    openapi_json: *const c_char,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let Some(func) = handler else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL handler");
    };
    if flags & !(VEIL_ROUTE_INLINE | VEIL_ROUTE_STREAM_BODY) != 0 {
        return fail(VEIL_ERR_INVALID_ARG, "unknown route flags");
    }
    if flags & VEIL_ROUTE_INLINE != 0 && flags & VEIL_ROUTE_STREAM_BODY != 0 {
        return fail(
            VEIL_ERR_INVALID_ARG,
            "VEIL_ROUTE_STREAM_BODY needs a handler-pool route (not VEIL_ROUTE_INLINE)",
        );
    }
    let path = tryc!(cstr(path));
    if !path.starts_with('/') {
        return fail(VEIL_ERR_INVALID_ARG, "path must start with '/'");
    }
    let methods: Vec<String> = if methods_csv.is_null() {
        Vec::new()
    } else {
        let csv = tryc!(cstr(methods_csv));
        csv.split(',')
            .map(|m| m.trim().to_ascii_uppercase())
            .filter(|m| !m.is_empty() && m != "*")
            .collect()
    };
    if methods
        .iter()
        .any(|m| !m.bytes().all(|b| b.is_ascii_uppercase()))
    {
        return fail(VEIL_ERR_INVALID_ARG, "invalid HTTP method");
    }
    let name = route_name(&cfg.routes, &methods, path);
    let mut meta = OperationMeta {
        operation_id: name.clone(),
        ..Default::default()
    };
    if !openapi_json.is_null() {
        let text = tryc!(cstr(openapi_json));
        match serde_json::from_str::<serde_json::Value>(text) {
            Ok(v) if v.is_object() => meta.raw = Some(v),
            Ok(_) => {
                return fail(VEIL_ERR_INVALID_ARG, "openapi_json must be a JSON object");
            }
            Err(e) => return fail(VEIL_ERR_INVALID_ARG, format!("openapi_json: {}", e)),
        }
    }
    cfg.routes.push(CRoute {
        name,
        path: path.to_string(),
        methods,
        func,
        user_data: UserData(user_data),
        inline: flags & VEIL_ROUTE_INLINE != 0,
        stream_body: flags & VEIL_ROUTE_STREAM_BODY != 0,
        meta,
    });
    VEIL_OK
}

/// C コールバックを 1 回呼ぶ。0 以外の戻り値・panic は 500（ストリーミングを始めた後は使わない）。
fn invoke(func: veil_handler_fn, ud: UserData, req: Request, ctx: *mut HandlerContext) -> Response {
    let mut wreq = veil_request_t::new(req);
    wreq.ctx = ctx;
    let mut wresp = veil_response_t {
        resp: Response::new(200),
        ctx,
    };
    let rc = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: wreq / wresp はこの呼び出しの間だけ生きる。user_data の契約は UserData を参照。
        unsafe { func(&wreq, &mut wresp, ud.get()) }
    }));
    match rc {
        Ok(0) => wresp.resp,
        Ok(_) => Response::error(500, "handler returned an error"),
        Err(_) => Response::error(500, "handler panicked"),
    }
}

/// ルートからハンドラ関数を作る。
fn make_handler(func: veil_handler_fn, ud: UserData, inline: bool) -> HandlerFn {
    if inline {
        std::sync::Arc::new(move |req| {
            Box::pin(async move { invoke(func, ud, req, ptr::null_mut()) })
        })
    } else {
        // ハンドラ用プールで呼ぶ。コールバックは応答をストリーミングにでき（`veil_response_stream`）、
        // ストリーミング受信のルートでは要求本文を順に読める（F-210）
        std::sync::Arc::new(move |req| {
            Box::pin(veil::api::bridge::call_blocking(req, move |req, ctx| {
                invoke(func, ud, req, ctx as *mut HandlerContext)
            }))
        })
    }
}

// ===================== 起動・停止 =====================

/// サーバを起動して停止（SIGINT / SIGTERM / `veil_shutdown`）までブロックする。
/// `cfg` は成功・失敗にかかわらず消費される（呼び出し後に `veil_config_free` しない）。
/// 成功で 0、失敗で負のエラーコード（`veil_last_error` に理由）。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル。登録した `user_data` の契約は `veil_register_route` を参照。
#[no_mangle]
pub unsafe extern "C" fn veil_run(cfg: *mut veil_config_t) -> c_int {
    if cfg.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    }
    let cfg = *Box::from_raw(cfg);
    let mut app = App::new().config(cfg.config).without_macro_routes();
    for r in cfg.routes {
        let methods: Vec<&str> = r.methods.iter().map(|m| m.as_str()).collect();
        let func = make_handler(r.func, r.user_data, r.inline);
        app = if r.stream_body {
            app.route_fn_streaming(&r.name, &methods, &r.path, func, r.meta)
        } else {
            app.route_fn(&r.name, &methods, &r.path, func, r.meta)
        };
    }
    for f in cfg.extra {
        app = f(app);
    }
    match catch_unwind(AssertUnwindSafe(|| app.run())) {
        Ok(Ok(())) => VEIL_OK,
        Ok(Err(e)) => fail(VEIL_ERR_RUNTIME, e.0),
        Err(_) => fail(VEIL_ERR_RUNTIME, "server panicked"),
    }
}

/// 実行中のサーバを graceful に停止する（別スレッドから呼べる）。
///
/// # Safety
///
/// 常に呼べる。
#[no_mangle]
pub unsafe extern "C" fn veil_shutdown() {
    veil::api::shutdown();
}

// ===================== リクエストのアクセサ =====================

/// 文字列スライスを `(ptr, *len)` で返す。
unsafe fn out_bytes(b: &[u8], len: *mut usize) -> *const c_char {
    if !len.is_null() {
        *len = b.len();
    }
    b.as_ptr() as *const c_char
}

/// 見つからないとき NULL と長さ 0。
unsafe fn out_none(len: *mut usize) -> *const c_char {
    if !len.is_null() {
        *len = 0;
    }
    ptr::null()
}

/// HTTP メソッド（NUL 終端ではない。長さは `*len`）。
///
/// # Safety
///
/// `req` はコールバックに渡されたハンドル。`len` は NULL か書き込み可能。返るポインタはコールバックの間だけ有効。
#[no_mangle]
pub unsafe extern "C" fn veil_request_method(
    req: *const veil_request_t,
    len: *mut usize,
) -> *const c_char {
    match req.as_ref() {
        Some(r) => out_bytes(r.req.method().as_bytes(), len),
        None => out_none(len),
    }
}

/// クエリを除いたパス（パーセントエンコードのまま。NUL 終端ではない）。
///
/// # Safety
///
/// `veil_request_method` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_request_path(
    req: *const veil_request_t,
    len: *mut usize,
) -> *const c_char {
    match req.as_ref() {
        Some(r) => out_bytes(r.req.path().as_bytes(), len),
        None => out_none(len),
    }
}

/// クエリ文字列（`?` の後ろ。無ければ長さ 0。NUL 終端ではない）。
///
/// # Safety
///
/// `veil_request_method` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_request_query(
    req: *const veil_request_t,
    len: *mut usize,
) -> *const c_char {
    match req.as_ref() {
        Some(r) => out_bytes(r.req.query().as_bytes(), len),
        None => out_none(len),
    }
}

/// 本文（バイナリ可。NUL 終端ではない）。
///
/// # Safety
///
/// `veil_request_method` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_request_body(
    req: *const veil_request_t,
    len: *mut usize,
) -> *const c_char {
    match req.as_ref() {
        Some(r) => out_bytes(r.req.body(), len),
        None => out_none(len),
    }
}

/// ヘッダの値（名前は大文字小文字を区別しない）。無ければ NULL。
///
/// # Safety
///
/// `veil_request_method` と同じ。`name` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_request_header(
    req: *const veil_request_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(name)) = (req.as_ref(), cstr(name)) else {
        return out_none(len);
    };
    match r.req.header(name) {
        Some(v) => out_bytes(v.as_bytes(), len),
        None => out_none(len),
    }
}

/// `Cow` を出力する（借用ならそのまま、所有なら置き場へ移す）。
unsafe fn out_cow(
    r: &veil_request_t,
    v: Option<std::borrow::Cow<'_, str>>,
    len: *mut usize,
) -> *const c_char {
    match v {
        Some(std::borrow::Cow::Borrowed(s)) => out_bytes(s.as_bytes(), len),
        Some(std::borrow::Cow::Owned(s)) => {
            let (p, n) = r.keep(s);
            if !len.is_null() {
                *len = n;
            }
            p
        }
        None => out_none(len),
    }
}

/// パスパラメータ（`{name}`）の値。パーセント復号済み。無ければ NULL。
///
/// # Safety
///
/// `veil_request_method` と同じ。`name` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_request_param(
    req: *const veil_request_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(name)) = (req.as_ref(), cstr(name)) else {
        return out_none(len);
    };
    out_cow(r, r.req.param(name), len)
}

/// クエリパラメータの値（最初の出現。`+` と `%XX` を復号済み）。無ければ NULL。
///
/// # Safety
///
/// `veil_request_method` と同じ。`name` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_request_query_param(
    req: *const veil_request_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(r), Ok(name)) = (req.as_ref(), cstr(name)) else {
        return out_none(len);
    };
    out_cow(r, r.req.query_param(name), len)
}

/// 送信元 IP の文字列表現（NUL 終端ではない）。
///
/// # Safety
///
/// `veil_request_method` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_request_client_ip(
    req: *const veil_request_t,
    len: *mut usize,
) -> *const c_char {
    let Some(r) = req.as_ref() else {
        return out_none(len);
    };
    let (p, n) = r.keep(r.req.client_ip().to_string());
    if !len.is_null() {
        *len = n;
    }
    p
}

// ===================== レスポンスのセッター =====================

/// ステータスコードを設定する（100〜599）。
///
/// # Safety
///
/// `resp` はコールバックに渡されたハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_response_set_status(
    resp: *mut veil_response_t,
    status: u16,
) -> c_int {
    let Some(r) = resp.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL response");
    };
    if !(100..=599).contains(&status) {
        return fail(VEIL_ERR_INVALID_ARG, "status out of range");
    }
    r.resp = std::mem::replace(&mut r.resp, Response::new(200)).with_status(status);
    VEIL_OK
}

/// ヘッダを設定する（同名は置き換える）。名前・値に制御文字（CR/LF 等）を含むと拒否する。
///
/// # Safety
///
/// `resp` はコールバックに渡されたハンドル。`name` と `value` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_response_set_header(
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
    r.resp
        .set_header(name, Bytes::copy_from_slice(value.as_bytes()));
    VEIL_OK
}

/// ヘッダ（トレーラ・gRPC のメタデータも）の名前と値として送ってよいか（CR/LF 等の制御文字を拒否）。
fn valid_header(name: &str, value: &str) -> bool {
    let name_ok = !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_graphic() && b != b':' && b != b' ');
    let value_ok = value
        .bytes()
        .all(|b| b == b'\t' || (b >= 0x20 && b != 0x7f));
    name_ok && value_ok
}

/// 本文を設定する（コピーする。Content-Type は変えない）。
///
/// # Safety
///
/// `resp` はコールバックに渡されたハンドル。`data` は `len` バイト読めること（`len == 0` なら NULL 可）。
#[no_mangle]
pub unsafe extern "C" fn veil_response_set_body(
    resp: *mut veil_response_t,
    data: *const u8,
    len: usize,
) -> c_int {
    let Some(r) = resp.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL response");
    };
    if data.is_null() && len != 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL body with non-zero length");
    }
    let body = if len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(data, len))
    };
    r.resp = std::mem::replace(&mut r.resp, Response::new(200)).with_body(body);
    VEIL_OK
}

/// `text/plain; charset=utf-8` の本文を設定する（NUL 終端の UTF-8）。
///
/// # Safety
///
/// `resp` はコールバックに渡されたハンドル。`text` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_response_set_text(
    resp: *mut veil_response_t,
    text: *const c_char,
) -> c_int {
    set_typed(resp, text, "text/plain; charset=utf-8")
}

/// `application/json` の本文を設定する（呼び出し側が正しい JSON を渡す。検証しない）。
///
/// # Safety
///
/// `veil_response_set_text` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_response_set_json(
    resp: *mut veil_response_t,
    json: *const c_char,
) -> c_int {
    set_typed(resp, json, "application/json")
}

unsafe fn set_typed(resp: *mut veil_response_t, text: *const c_char, ctype: &'static str) -> c_int {
    let Some(r) = resp.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL response");
    };
    let text = tryc!(cstr(text));
    r.resp = std::mem::replace(&mut r.resp, Response::new(200)).with_body(text.to_string());
    r.resp
        .set_header("content-type", Bytes::from_static(ctype.as_bytes()));
    VEIL_OK
}

/// ストリーミング・gRPC・L4（F-210）。`tryc!` 等の後に置く（マクロはテキスト順で見える）。
mod ext;

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn c(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    fn take(p: *const c_char, len: usize) -> String {
        assert!(!p.is_null());
        // SAFETY: テスト内でアクセサが返した (ptr, len)。
        let b = unsafe { std::slice::from_raw_parts(p as *const u8, len) };
        String::from_utf8(b.to_vec()).unwrap()
    }

    fn sample_request() -> veil_request_t {
        let req = Request::new(
            "POST",
            "/users/42?name=a+b&x=%41",
            vec![(
                Bytes::from_static(b"Content-Type"),
                Bytes::from_static(b"application/json"),
            )],
            "{\"a\":1}",
        )
        .with_params(vec![(Arc::from("id"), Bytes::from_static(b"4%202"))]);
        veil_request_t::new(req)
    }

    #[test]
    fn request_accessors() {
        let r = sample_request();
        let rp: *const veil_request_t = &r;
        let mut len = 0usize;
        unsafe {
            assert_eq!(take(veil_request_method(rp, &mut len), len), "POST");
            assert_eq!(take(veil_request_path(rp, &mut len), len), "/users/42");
            assert_eq!(
                take(veil_request_query(rp, &mut len), len),
                "name=a+b&x=%41"
            );
            assert_eq!(take(veil_request_body(rp, &mut len), len), "{\"a\":1}");
            let n = c("content-type");
            assert_eq!(
                take(veil_request_header(rp, n.as_ptr(), &mut len), len),
                "application/json"
            );
            let n = c("nope");
            assert!(veil_request_header(rp, n.as_ptr(), &mut len).is_null());
            assert_eq!(len, 0);
            let n = c("id");
            assert_eq!(
                take(veil_request_param(rp, n.as_ptr(), &mut len), len),
                "4 2"
            );
            let n = c("name");
            assert_eq!(
                take(veil_request_query_param(rp, n.as_ptr(), &mut len), len),
                "a b"
            );
            let n = c("x");
            assert_eq!(
                take(veil_request_query_param(rp, n.as_ptr(), &mut len), len),
                "A"
            );
            assert_eq!(take(veil_request_client_ip(rp, &mut len), len), "127.0.0.1");
            // NULL の len と NULL のリクエストは安全
            assert!(!veil_request_method(rp, ptr::null_mut()).is_null());
            assert!(veil_request_method(ptr::null(), &mut len).is_null());
        }
    }

    #[test]
    fn arena_pointers_stay_valid() {
        let r = sample_request();
        let rp: *const veil_request_t = &r;
        let mut l1 = 0usize;
        let n = c("id");
        unsafe {
            let p1 = veil_request_param(rp, n.as_ptr(), &mut l1);
            // 置き場を大きく伸ばしても最初のポインタは動かない
            for _ in 0..100 {
                let mut l = 0usize;
                veil_request_client_ip(rp, &mut l);
            }
            assert_eq!(take(p1, l1), "4 2");
        }
    }

    #[test]
    fn response_setters() {
        let mut r = veil_response_t {
            resp: Response::new(200),
            ctx: ptr::null_mut(),
        };
        let rp: *mut veil_response_t = &mut r;
        unsafe {
            assert_eq!(veil_response_set_status(rp, 201), VEIL_OK);
            assert_eq!(veil_response_set_status(rp, 99), VEIL_ERR_INVALID_ARG);
            assert_eq!(
                veil_response_set_header(rp, c("X-A").as_ptr(), c("1").as_ptr()),
                VEIL_OK
            );
            assert_eq!(
                veil_response_set_header(rp, c("X-A").as_ptr(), c("2").as_ptr()),
                VEIL_OK
            );
            assert_eq!(
                veil_response_set_header(rp, c("X-B").as_ptr(), c("a\r\nb").as_ptr()),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(veil_response_set_text(rp, c("hi").as_ptr()), VEIL_OK);
            assert_eq!(r.resp.status_code(), 201);
            assert_eq!(r.resp.header("x-a"), Some(&b"2"[..]));
            assert_eq!(&r.resp.body()[..], b"hi");
            assert_eq!(
                r.resp.header("content-type"),
                Some(&b"text/plain; charset=utf-8"[..])
            );
            assert_eq!(veil_response_set_json(rp, c("{\"a\":1}").as_ptr()), VEIL_OK);
            assert_eq!(
                r.resp.header("content-type"),
                Some(&b"application/json"[..])
            );
            assert_eq!(veil_response_set_body(rp, b"xyz".as_ptr(), 3), VEIL_OK);
            assert_eq!(&r.resp.body()[..], b"xyz");
            assert_eq!(
                veil_response_set_body(rp, ptr::null(), 1),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_response_set_text(ptr::null_mut(), c("x").as_ptr()),
                VEIL_ERR_INVALID_ARG
            );
            let bad = [0xffu8, 0];
            assert_eq!(
                veil_response_set_text(rp, bad.as_ptr() as *const c_char),
                VEIL_ERR_UTF8
            );
            assert!(!veil_last_error().is_null());
        }
    }

    unsafe extern "C" fn echo_cb(
        req: *const veil_request_t,
        resp: *mut veil_response_t,
        ud: *mut c_void,
    ) -> c_int {
        let mut len = 0usize;
        let b = veil_request_body(req, &mut len);
        veil_response_set_body(resp, b as *const u8, len);
        *(ud as *mut i32) += 1;
        0
    }

    unsafe extern "C" fn fail_cb(
        _: *const veil_request_t,
        _: *mut veil_response_t,
        _: *mut c_void,
    ) -> c_int {
        7
    }

    #[test]
    fn invoke_maps_return_codes() {
        let mut counter = 0i32;
        let ud = UserData(&mut counter as *mut i32 as *mut c_void);
        let r = invoke(echo_cb, ud, Request::new("POST", "/e", vec![], "body"));
        assert_eq!(r.status_code(), 200);
        assert_eq!(&r.body()[..], b"body");
        assert_eq!(counter, 1);
        let r = invoke(
            fail_cb,
            UserData(ptr::null_mut()),
            Request::new("GET", "/", vec![], ""),
        );
        assert_eq!(r.status_code(), 500);
    }

    #[test]
    fn config_and_routes() {
        unsafe {
            let cfg = veil_config_new();
            assert_eq!(
                veil_config_listen_cleartext(cfg, c("127.0.0.1:1").as_ptr()),
                VEIL_OK
            );
            assert_eq!(veil_config_listen(cfg, c("127.0.0.1:2").as_ptr()), VEIL_OK);
            assert_eq!(
                veil_config_tls(cfg, c("/tmp/c.pem").as_ptr(), c("/tmp/k.pem").as_ptr()),
                VEIL_OK
            );
            assert_eq!(veil_config_threads(cfg, 2), VEIL_OK);
            assert_eq!(
                veil_config_set(cfg, c("logging.level").as_ptr(), c("\"warn\"").as_ptr()),
                VEIL_OK
            );
            assert_eq!(
                veil_config_set(cfg, c("server.threads").as_ptr(), c("").as_ptr()),
                VEIL_ERR_CONFIG
            );
            assert_eq!(
                veil_register_route(
                    cfg,
                    c("/u/{id}").as_ptr(),
                    c("GET").as_ptr(),
                    Some(echo_cb),
                    ptr::null_mut()
                ),
                VEIL_OK
            );
            assert_eq!(
                veil_register_route_ex(
                    cfg,
                    c("/u/{id}").as_ptr(),
                    c("get, post").as_ptr(),
                    Some(echo_cb),
                    ptr::null_mut(),
                    VEIL_ROUTE_INLINE,
                    c("{\"summary\":\"s\"}").as_ptr()
                ),
                VEIL_OK
            );
            assert_eq!(
                veil_register_route_ex(
                    cfg,
                    c("/x").as_ptr(),
                    ptr::null(),
                    Some(echo_cb),
                    ptr::null_mut(),
                    0,
                    c("[1]").as_ptr()
                ),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_register_route(
                    cfg,
                    c("x").as_ptr(),
                    ptr::null(),
                    Some(echo_cb),
                    ptr::null_mut()
                ),
                VEIL_ERR_INVALID_ARG
            );
            assert_eq!(
                veil_register_route(cfg, c("/x").as_ptr(), ptr::null(), None, ptr::null_mut()),
                VEIL_ERR_INVALID_ARG
            );
            let cr = &*cfg;
            let names: Vec<_> = cr.routes.iter().map(|r| r.name.clone()).collect();
            assert_eq!(names, ["c_get_u_id", "c_get_u_id_2"]);
            assert_eq!(cr.routes[1].methods, ["GET", "POST"]);
            assert!(cr.routes[1].inline);
            assert!(cr.routes[1].meta.raw.is_some());
            veil_config_free(cfg);
            veil_config_free(ptr::null_mut());
            assert!(veil_config_from_file(c("/nonexistent/x.toml").as_ptr()).is_null());
            assert!(!veil_last_error().is_null());
            assert!(CStr::from_ptr(veil_version())
                .to_str()
                .unwrap()
                .contains('.'));
        }
    }
}
