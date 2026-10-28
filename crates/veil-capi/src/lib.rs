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
use veil::api::{App, Config, HandlerFn, Listener, OperationMeta, Request, Response};

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
/// フォームの本文が壊れている（400 を返す。F-266）。
pub const VEIL_ERR_FORM: c_int = -7;
/// フォームの本文が `[api] multipart_*` の上限を超えた（413 を返す。F-266）。
pub const VEIL_ERR_TOO_LARGE: c_int = -8;

/// コールバックをワーカースレッド上で直接実行する（既定はハンドラ用プール）。
pub const VEIL_ROUTE_INLINE: u32 = 1;
/// 要求本文を溜めずにコールバックを呼ぶ（`veil_request_body_read` で順に読む。F-210）。
pub const VEIL_ROUTE_STREAM_BODY: u32 = 2;
/// コールバックの前に本文を読み、`multipart/form-data` はパートに分ける（大きなファイルは名前の無い
/// 一時ファイル。`veil_multipart_next` の `fd`。F-266）。
pub const VEIL_ROUTE_FORM: u32 = 4;
/// このルートの応答を `[route.compression]`・`[api] compression` で圧縮しない（F-270）。
pub const VEIL_ROUTE_NO_COMPRESS: u32 = 8;

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
    form: bool,
    // F-270: 応答の圧縮を許す（`VEIL_ROUTE_NO_COMPRESS` が無い）
    compress: bool,
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
    /// multipart のパート（`VEIL_ROUTE_FORM` は先に読んだもの、他は最初の `veil_multipart_next` で解析。F-266）
    parts: RefCell<Option<Vec<veil::api::multipart::SpooledPart>>>,
    /// 次に返すパートの添字
    part_next: std::cell::Cell<usize>,
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
            parts: RefCell::new(None),
            part_next: std::cell::Cell::new(0),
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

// B-209: 0 で埋めたものが既定になるよう 3 値（0 = 既定、正 = true、負 = false）にし、先頭に大きさを置く
// （公開前の今だけ ABI を変えられる。後の版はフィールドを末尾に足し、`size` までしか読まない）。
/// `veil_listener_t` の 3 値: 既定（省略）。
pub const VEIL_DEFAULT: c_int = 0;
/// `veil_listener_t` の 3 値: true。
pub const VEIL_ON: c_int = 1;
/// `veil_listener_t` の 3 値: false。
pub const VEIL_OFF: c_int = -1;

/// 待ち受け 1 個（`[[listener]]`、F-260）。`veil_config_add_listener` に渡す。
///
/// 0 で埋めたものが既定。文字列は NULL なら省略。`http2`・`allow_http`・`allow_h2c`・`secure_redirect` は
/// `VEIL_DEFAULT`（0）で省略、正で true、負で false。`threads` は 0 なら省略。`size` は
/// `sizeof(veil_listener_t)`（0 はこの版の大きさとして読む）。
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_listener_t {
    /// `sizeof(veil_listener_t)`、または 0
    pub size: usize,
    pub name: *const c_char,
    /// 必須。`host:port` か `unix:<path>`
    pub listen: *const c_char,
    /// 0 = 平文（HTTP/1.1 + h2c）、非 0 = TLS（`veil_config_tls` の証明書）
    pub tls: c_int,
    pub http2: c_int,
    /// 非 0 = 同じポートの UDP で HTTP/3（TLS のみ）
    pub http3: c_int,
    pub http3_listen: *const c_char,
    pub allow_http: c_int,
    pub allow_h2c: c_int,
    pub secure_redirect: c_int,
    pub redirect_to: *const c_char,
    pub threads: u32,
    pub unix_socket_permissions: *const c_char,
}

/// この版（0.10）の `veil_listener_t` の大きさ（`size` が 0 のときに読む大きさ。後の版でも変えない）
const LISTENER_V1_SIZE: usize = std::mem::size_of::<veil_listener_t>();

/// 3 値（0 = 既定、正 = true、負 = false）
fn tri(v: c_int) -> Option<bool> {
    match v {
        0 => None,
        v => Some(v > 0),
    }
}

/// NULL なら `None`、それ以外は UTF-8 の文字列
unsafe fn opt_cstr<'a>(p: *const c_char) -> Result<Option<&'a str>, c_int> {
    if p.is_null() {
        Ok(None)
    } else {
        cstr(p).map(Some)
    }
}

/// 待ち受けを 1 個足す（`[[listener]]`、F-260）。ポートごとに 1 回呼ぶ。1 個でも足すと、ライブラリ既定の
/// `127.0.0.1:8080`（`listen_cleartext`）は `veil_config_listen_cleartext` で明示しない限り開かない。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`listener` は有効な `veil_listener_t` へのポインタ（文字列は NUL 終端か NULL）。
#[no_mangle]
pub unsafe extern "C" fn veil_config_add_listener(
    cfg: *mut veil_config_t,
    listener: *const veil_listener_t,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    if listener.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL listener");
    }
    // B-209: 呼び出し側の大きさ（0 = この版の大きさ）までだけ読み、残り（後の版で足すフィールド）は既定
    let full = std::mem::size_of::<veil_listener_t>();
    let size = match ptr::read(ptr::addr_of!((*listener).size)) {
        0 => LISTENER_V1_SIZE,
        n => n,
    };
    if size < LISTENER_V1_SIZE {
        return fail(
            VEIL_ERR_INVALID_ARG,
            "veil_listener_t.size must be 0 or sizeof(veil_listener_t)",
        );
    }
    let mut l = std::mem::MaybeUninit::<veil_listener_t>::zeroed();
    ptr::copy_nonoverlapping(
        listener as *const u8,
        l.as_mut_ptr() as *mut u8,
        size.min(full),
    );
    // SAFETY: 全体を 0 で初期化してから読める分を写した（ポインタは NULL、整数は 0 が既定の値）
    let l = l.assume_init();
    if l.listen.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "veil_listener_t.listen is required");
    }
    let listen = tryc!(cstr(l.listen));
    let mut b = if l.tls != 0 {
        Listener::tls(listen)
    } else {
        Listener::cleartext(listen)
    };
    if let Some(v) = tryc!(opt_cstr(l.name)) {
        b = b.name(v);
    }
    if let Some(v) = tri(l.http2) {
        b = b.http2(v);
    }
    if l.http3 != 0 {
        b = b.http3(true);
    }
    if let Some(v) = tryc!(opt_cstr(l.http3_listen)) {
        b = b.http3_listen(v);
    }
    if let Some(v) = tri(l.allow_http) {
        b = b.allow_http(v);
    }
    if let Some(v) = tri(l.allow_h2c) {
        b = b.allow_h2c(v);
    }
    if let Some(v) = tri(l.secure_redirect) {
        b = b.secure_redirect(v);
    }
    if let Some(v) = tryc!(opt_cstr(l.redirect_to)) {
        b = b.redirect_to(v);
    }
    if l.threads > 0 {
        b = b.threads(l.threads as usize);
    }
    if let Some(v) = tryc!(opt_cstr(l.unix_socket_permissions)) {
        b = b.unix_socket_permissions(v);
    }
    cfg.config.add_listener(b);
    VEIL_OK
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
    if flags
        & !(VEIL_ROUTE_INLINE | VEIL_ROUTE_STREAM_BODY | VEIL_ROUTE_FORM | VEIL_ROUTE_NO_COMPRESS)
        != 0
    {
        return fail(VEIL_ERR_INVALID_ARG, "unknown route flags");
    }
    if flags & VEIL_ROUTE_INLINE != 0 && flags & VEIL_ROUTE_STREAM_BODY != 0 {
        return fail(
            VEIL_ERR_INVALID_ARG,
            "VEIL_ROUTE_STREAM_BODY needs a handler-pool route (not VEIL_ROUTE_INLINE)",
        );
    }
    if flags & VEIL_ROUTE_FORM != 0 && flags & VEIL_ROUTE_STREAM_BODY != 0 {
        return fail(
            VEIL_ERR_INVALID_ARG,
            "VEIL_ROUTE_FORM reads the body before the callback (not with VEIL_ROUTE_STREAM_BODY)",
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
        form: flags & VEIL_ROUTE_FORM != 0,
        compress: flags & VEIL_ROUTE_NO_COMPRESS == 0,
        meta,
    });
    VEIL_OK
}

/// C コールバックを 1 回呼ぶ。0 以外の戻り値・panic は 500（ストリーミングを始めた後は使わない）。
fn invoke(
    func: veil_handler_fn,
    ud: UserData,
    req: Request,
    parts: Option<Vec<veil::api::multipart::SpooledPart>>,
    ctx: *mut HandlerContext,
) -> Response {
    let mut wreq = veil_request_t::new(req);
    wreq.ctx = ctx;
    *wreq.parts.borrow_mut() = parts;
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
fn make_handler(func: veil_handler_fn, ud: UserData, inline: bool, form: bool) -> HandlerFn {
    std::sync::Arc::new(move |req| {
        Box::pin(async move {
            // F-266: VEIL_ROUTE_FORM は本文を先に読む（大きなファイルは一時ファイルへ）
            let (req, parts) = if form {
                match veil::api::multipart::prepare_form(req).await {
                    Ok(x) => x,
                    Err(resp) => return resp,
                }
            } else {
                (req, None)
            };
            if inline {
                invoke(func, ud, req, parts, ptr::null_mut())
            } else {
                // ハンドラ用プールで呼ぶ。コールバックは応答をストリーミングにでき（`veil_response_stream`）、
                // ストリーミング受信のルートでは要求本文を順に読める（F-210）
                veil::api::bridge::call_blocking(req, move |req, ctx| {
                    invoke(func, ud, req, parts, ctx as *mut HandlerContext)
                })
                .await
            }
        })
    })
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
        let func = make_handler(r.func, r.user_data, r.inline, r.form);
        app = if r.stream_body || r.form {
            app.route_fn_streaming(&r.name, &methods, &r.path, func, r.meta)
        } else {
            app.route_fn(&r.name, &methods, &r.path, func, r.meta)
        }
        .route_compression(r.compress);
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

// ===================== multipart（F-266） =====================

/// `veil_multipart_next` が返す 1 パート。ポインタはコールバックの間だけ有効（NUL 終端ではない）。
#[repr(C)]
#[allow(non_camel_case_types)]
pub struct veil_part_t {
    /// `Content-Disposition` の `name`（無ければ NULL）
    pub name: *const c_char,
    pub name_len: usize,
    /// `filename`（ファイルでなければ NULL）
    pub filename: *const c_char,
    pub filename_len: usize,
    /// パートの `Content-Type`（無ければ NULL）
    pub content_type: *const c_char,
    pub content_type_len: usize,
    /// 中身（一時ファイルに書いたときは NULL）
    pub data: *const c_char,
    pub data_len: usize,
    /// 一時ファイルの fd（Windows はハンドル）。先頭に位置づけてある。メモリなら -1。
    /// コールバックから戻ると閉じる（残すなら dup する）
    pub fd: isize,
    /// 中身のバイト数
    pub size: u64,
}

unsafe fn put_str(s: Option<&str>, p: &mut *const c_char, l: &mut usize) {
    match s {
        Some(s) => {
            *p = s.as_ptr() as *const c_char;
            *l = s.len();
        }
        None => {
            *p = ptr::null();
            *l = 0;
        }
    }
}

/// 次の multipart のパートを `*out` に入れる。1 = 入れた、0 = 終わり、負 = エラー
/// （`VEIL_ERR_FORM` は 400、`VEIL_ERR_TOO_LARGE` は 413 を返す。`VEIL_ERR_STATE` は multipart の本文ではない）。
/// `VEIL_ROUTE_FORM` の無いルートは溜めた本文を解析する（ファイルもメモリ上）。
///
/// # Safety
///
/// `req` はコールバックに渡されたハンドル、`out` は書き込み可能な `veil_part_t`。
#[no_mangle]
pub unsafe extern "C" fn veil_multipart_next(
    req: *const veil_request_t,
    out: *mut veil_part_t,
) -> c_int {
    use veil::api::multipart::{SpooledData, SpooledPart};
    let (Some(r), Some(out)) = (req.as_ref(), out.as_mut()) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL request or part");
    };
    let mut parts = r.parts.borrow_mut();
    if parts.is_none() {
        if !veil::api::multipart::is_multipart(&r.req) {
            return fail(VEIL_ERR_STATE, "not a multipart/form-data body");
        }
        match veil::api::multipart::request_parts(&r.req) {
            Ok(v) => {
                *parts = Some(
                    v.into_iter()
                        .map(|p| SpooledPart {
                            info: p.info,
                            data: SpooledData::Memory(p.data),
                        })
                        .collect(),
                )
            }
            Err(e) => {
                let code = if e.status() == 413 {
                    VEIL_ERR_TOO_LARGE
                } else {
                    VEIL_ERR_FORM
                };
                return fail(code, e.message());
            }
        }
    }
    let v = parts.as_mut().expect("parts are set above");
    let i = r.part_next.get();
    let Some(p) = v.get_mut(i) else {
        return 0;
    };
    r.part_next.set(i + 1);
    put_str(p.info.name.as_deref(), &mut out.name, &mut out.name_len);
    put_str(
        p.info.filename.as_deref(),
        &mut out.filename,
        &mut out.filename_len,
    );
    put_str(
        p.info.content_type.as_deref(),
        &mut out.content_type,
        &mut out.content_type_len,
    );
    match &mut p.data {
        SpooledData::Memory(b) => {
            out.data = b.as_ptr() as *const c_char;
            out.data_len = b.len();
            out.fd = -1;
            out.size = b.len() as u64;
        }
        SpooledData::File { file, len } => {
            use std::io::Seek;
            if let Err(e) = file.seek(std::io::SeekFrom::Start(0)) {
                return fail(VEIL_ERR_RUNTIME, format!("seek temporary upload: {}", e));
            }
            out.data = ptr::null();
            out.data_len = 0;
            #[cfg(unix)]
            {
                use std::os::unix::io::AsRawFd;
                out.fd = file.as_raw_fd() as isize;
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::AsRawHandle;
                out.fd = file.as_raw_handle() as isize;
            }
            out.size = *len;
        }
    }
    1
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

/// この応答を圧縮しない（ルートが `[route.compression]`・`[api] compression` で圧縮するときも。F-270）。
///
/// # Safety
///
/// `resp` はコールバックに渡されたハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_response_no_compression(resp: *mut veil_response_t) -> c_int {
    let Some(r) = resp.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL response");
    };
    r.resp = std::mem::replace(&mut r.resp, Response::new(200)).no_compression();
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

    // F-260
    #[test]
    fn add_listener_builds_the_listener_array() {
        let listen = c("0.0.0.0:8081");
        let name = c("h1tls");
        let l = veil_listener_t {
            size: std::mem::size_of::<veil_listener_t>(),
            name: name.as_ptr(),
            listen: listen.as_ptr(),
            tls: 1,
            http2: VEIL_OFF,
            http3: 0,
            http3_listen: ptr::null(),
            allow_http: VEIL_DEFAULT,
            allow_h2c: VEIL_DEFAULT,
            secure_redirect: VEIL_DEFAULT,
            redirect_to: ptr::null(),
            threads: 2,
            unix_socket_permissions: ptr::null(),
        };
        unsafe {
            let cfg = veil_config_new();
            assert_eq!(veil_config_add_listener(cfg, &l), VEIL_OK);
            let plain = c("0.0.0.0:8082");
            let l2 = veil_listener_t {
                listen: plain.as_ptr(),
                tls: 0,
                name: ptr::null(),
                threads: 0,
                allow_http: VEIL_OFF,
                ..l
            };
            assert_eq!(veil_config_add_listener(cfg, &l2), VEIL_OK);
            assert_eq!(
                veil_config_add_listener(cfg, ptr::null()),
                VEIL_ERR_INVALID_ARG
            );
            let v = (*cfg).config.to_value().unwrap();
            let ls = v["listener"].as_array().unwrap();
            assert_eq!(ls.len(), 2);
            assert_eq!(ls[0]["name"].as_str(), Some("h1tls"));
            assert_eq!(ls[0]["tls"].as_bool(), Some(true));
            assert_eq!(ls[0]["http2"].as_bool(), Some(false));
            assert_eq!(ls[0]["threads"].as_integer(), Some(2));
            assert!(ls[0].get("allow_http").is_none());
            assert_eq!(ls[1]["allow_http"].as_bool(), Some(false));
            assert!(ls[1].get("threads").is_none());
            assert!(v["server"].get("listen_cleartext").is_none());
            veil_config_free(cfg);
        }
    }

    // B-209: 0 で埋めた veil_listener_t は既定（平文で HTTP/1.1 も h2c も受ける）。size は 0 か sizeof 以上
    #[test]
    fn zeroed_listener_means_defaults() {
        let listen = c("127.0.0.1:8083");
        unsafe {
            let cfg = veil_config_new();
            let mut l: veil_listener_t = std::mem::zeroed();
            l.listen = listen.as_ptr();
            assert_eq!(veil_config_add_listener(cfg, &l), VEIL_OK);
            l.size = std::mem::size_of::<veil_listener_t>();
            l.allow_h2c = VEIL_ON;
            l.secure_redirect = VEIL_OFF;
            assert_eq!(veil_config_add_listener(cfg, &l), VEIL_OK);
            // 古い（小さい）大きさは受けない
            l.size = 8;
            assert_eq!(veil_config_add_listener(cfg, &l), VEIL_ERR_INVALID_ARG);
            // 後の版の大きな構造体（末尾に知らないフィールド）でも、知っている分だけ読む
            #[repr(C)]
            struct Bigger {
                l: veil_listener_t,
                extra: u64,
            }
            let mut big: Bigger = std::mem::zeroed();
            big.l.size = std::mem::size_of::<Bigger>();
            big.l.listen = listen.as_ptr();
            big.l.allow_http = VEIL_OFF;
            big.extra = u64::MAX;
            assert_eq!(
                veil_config_add_listener(cfg, &big.l as *const veil_listener_t),
                VEIL_OK
            );
            let mut none: veil_listener_t = std::mem::zeroed();
            none.size = 0;
            assert_eq!(veil_config_add_listener(cfg, &none), VEIL_ERR_INVALID_ARG);
            let v = (*cfg).config.to_value().unwrap();
            let ls = v["listener"].as_array().unwrap();
            assert_eq!(ls.len(), 3);
            for k in [
                "allow_http",
                "allow_h2c",
                "secure_redirect",
                "http2",
                "threads",
            ] {
                assert!(ls[0].get(k).is_none(), "{k}");
            }
            assert_eq!(ls[0]["tls"].as_bool(), Some(false));
            assert_eq!(ls[1]["allow_h2c"].as_bool(), Some(true));
            assert_eq!(ls[1]["secure_redirect"].as_bool(), Some(false));
            assert_eq!(ls[2]["allow_http"].as_bool(), Some(false));
            veil_config_free(cfg);
        }
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
        // ctx 無し（INLINE と同じ。ストリーミングの関数は使えない）
        let r = invoke(
            echo_cb,
            ud,
            Request::new("POST", "/e", vec![], "body"),
            None,
            ptr::null_mut(),
        );
        assert_eq!(r.status_code(), 200);
        assert_eq!(&r.body()[..], b"body");
        assert_eq!(counter, 1);
        let r = invoke(
            fail_cb,
            UserData(ptr::null_mut()),
            Request::new("GET", "/", vec![], ""),
            None,
            ptr::null_mut(),
        );
        assert_eq!(r.status_code(), 500);
    }

    // F-266
    #[test]
    fn multipart_next_iterates_parts() {
        let body = "--b\r\ncontent-disposition: form-data; name=\"t\"\r\n\r\nv\r\n\
                    --b\r\ncontent-disposition: form-data; name=\"f\"; filename=\"a.txt\"\r\n\
                    content-type: text/plain\r\n\r\nhello\r\n--b--\r\n";
        let req = Request::new(
            "POST",
            "/u",
            vec![(
                Bytes::from_static(b"content-type"),
                Bytes::from_static(b"multipart/form-data; boundary=b"),
            )],
            body,
        );
        let r = veil_request_t::new(req);
        let s = |p: *const c_char, l: usize| unsafe {
            String::from_utf8(std::slice::from_raw_parts(p as *const u8, l).to_vec()).unwrap()
        };
        unsafe {
            let mut part: veil_part_t = std::mem::zeroed();
            assert_eq!(veil_multipart_next(&r, &mut part), 1);
            assert_eq!(s(part.name, part.name_len), "t");
            assert!(part.filename.is_null());
            assert_eq!(s(part.data, part.data_len), "v");
            assert_eq!(part.fd, -1);
            assert_eq!(veil_multipart_next(&r, &mut part), 1);
            assert_eq!(s(part.filename, part.filename_len), "a.txt");
            assert_eq!(s(part.content_type, part.content_type_len), "text/plain");
            assert_eq!(part.size, 5);
            assert_eq!(veil_multipart_next(&r, &mut part), 0);
            let plain = sample_request();
            assert_eq!(veil_multipart_next(&plain, &mut part), VEIL_ERR_STATE);
            let bad = veil_request_t::new(Request::new(
                "POST",
                "/u",
                vec![(
                    Bytes::from_static(b"content-type"),
                    Bytes::from_static(b"multipart/form-data; boundary=b"),
                )],
                "--b\r\nbroken",
            ));
            assert_eq!(veil_multipart_next(&bad, &mut part), VEIL_ERR_FORM);
        }
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
