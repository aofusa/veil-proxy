//! C API の拡張（F-210）: 応答のストリーミング・要求本文のストリーミング受信・gRPC・L4。
//!
//! どれも Rust 側の `veil::api::bridge`（ワーカーとハンドラ用プールの間の有界キュー）の上に作る。
//! メッセージ・データはバイト列のまま受け渡し、直列化（protobuf 等）は C の利用者が選ぶ。

use super::{
    cstr, fail, out_bytes, out_none, veil_config_t, veil_request_t, veil_response_t, UserData,
    VEIL_ERR_CLOSED, VEIL_ERR_INVALID_ARG, VEIL_ERR_STATE, VEIL_OK,
};
use bytes::Bytes;
use std::ffi::{c_char, c_int, c_void};
use std::net::SocketAddr;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::ptr;
use veil::api::bridge::ResponseWriter;
use veil::api::grpc::blocking::GrpcCall;
use veil::api::grpc::{Code, MethodKind, Status};
use veil::api::l4::{L4Out, L4Session};
use veil::api::App;

// ===================== 応答のストリーミング =====================

/// ストリーミングの応答の本文（不透明）。`veil_stream_close` で閉じて解放する。
#[allow(non_camel_case_types)]
pub struct veil_stream_t {
    writer: Option<ResponseWriter>,
    trailers: Vec<(Bytes, Bytes)>,
}

/// 応答をストリーミングにする。`resp` に設定済みのステータスとヘッダを先に送り、本文は返るハンドルで書く。
/// ハンドラ用プールのルート（INLINE でない）でだけ使える。2 回目・INLINE は NULL（`veil_last_error` に理由）。
/// ハンドルはコールバックから戻った後も別スレッドで使え、`veil_stream_close` で本文を終えて解放する。
///
/// # Safety
///
/// `resp` はコールバックに渡されたハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_response_stream(resp: *mut veil_response_t) -> *mut veil_stream_t {
    let Some(r) = resp.as_mut() else {
        fail(VEIL_ERR_INVALID_ARG, "NULL response");
        return ptr::null_mut();
    };
    let Some(ctx) = r.ctx.as_mut() else {
        fail(
            VEIL_ERR_STATE,
            "streaming needs a handler-pool route (not VEIL_ROUTE_INLINE)",
        );
        return ptr::null_mut();
    };
    let head = std::mem::replace(&mut r.resp, veil::api::Response::new(200));
    match ctx.start_stream(head) {
        Some(w) => Box::into_raw(Box::new(veil_stream_t {
            writer: Some(w),
            trailers: Vec::new(),
        })),
        None => {
            fail(
                VEIL_ERR_STATE,
                "the response is already streaming or the client is gone",
            );
            ptr::null_mut()
        }
    }
}

/// 本文の断片を送る（コピーする。満杯なら待つ）。クライアントが切断していたら `VEIL_ERR_CLOSED`。
///
/// # Safety
///
/// `s` は `veil_response_stream` が返したハンドル、`data` は `len` バイト読める。
#[no_mangle]
pub unsafe extern "C" fn veil_stream_write(
    s: *mut veil_stream_t,
    data: *const u8,
    len: usize,
) -> c_int {
    let Some(s) = s.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL stream");
    };
    if data.is_null() && len > 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL data");
    }
    let b = if len == 0 {
        return VEIL_OK;
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(data, len))
    };
    match &s.writer {
        Some(w) if w.write(b) => VEIL_OK,
        _ => fail(VEIL_ERR_CLOSED, "client closed the stream"),
    }
}

/// 本文の後に送るトレーラを足す（HTTP/2・HTTP/3 のトレーラ、HTTP/1.1 は chunked のトレーラ）。
///
/// # Safety
///
/// `s` は有効なハンドル、`name`・`value` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_stream_set_trailer(
    s: *mut veil_stream_t,
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    let Some(s) = s.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL stream");
    };
    let n = tryc!(cstr(name));
    let v = tryc!(cstr(value));
    if !super::valid_header(n, v) {
        return fail(VEIL_ERR_INVALID_ARG, "invalid trailer");
    }
    s.trailers.push((
        Bytes::copy_from_slice(n.to_ascii_lowercase().as_bytes()),
        Bytes::copy_from_slice(v.as_bytes()),
    ));
    VEIL_OK
}

/// 本文を終えて（トレーラがあれば送って）ハンドルを解放する。NULL は何もしない。
///
/// # Safety
///
/// `s` は `veil_response_stream` が返したハンドルか NULL。以後使わない。
#[no_mangle]
pub unsafe extern "C" fn veil_stream_close(s: *mut veil_stream_t) {
    if s.is_null() {
        return;
    }
    let mut s = Box::from_raw(s);
    if let Some(w) = s.writer.take() {
        if !s.trailers.is_empty() {
            w.finish_with_trailers(std::mem::take(&mut s.trailers));
        }
    }
}

/// 要求本文の次の断片を `buf` へ読む（`VEIL_ROUTE_STREAM_BODY` のルート。それ以外は全量の本文を順に返す）。
/// 戻り値は読んだバイト数、0 は終わり、負はエラー。届くまで待つ（ハンドラ用プールのスレッドで）。
///
/// # Safety
///
/// `req` はコールバックに渡されたハンドル（コールバックの間だけ）、`buf` は `cap` バイト書ける。
#[no_mangle]
pub unsafe extern "C" fn veil_request_body_read(
    req: *const veil_request_t,
    buf: *mut u8,
    cap: usize,
) -> i64 {
    let Some(r) = req.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL request") as i64;
    };
    if buf.is_null() || cap == 0 {
        return fail(VEIL_ERR_INVALID_ARG, "empty buffer") as i64;
    }
    let mut pending = r.body_pending.borrow_mut();
    if pending.is_empty() {
        match r.ctx.as_mut() {
            Some(ctx) if ctx.has_body_stream() => match ctx.read_body() {
                Some(b) => *pending = b,
                None => return 0,
            },
            // ストリーミング受信でないルート: 全量の本文を 1 回で
            _ => {
                if r.arena.borrow().iter().any(|s| s == "\0body-read") {
                    return 0;
                }
                r.arena.borrow_mut().push("\0body-read".to_string());
                *pending = Bytes::copy_from_slice(r.req.body());
                if pending.is_empty() {
                    return 0;
                }
            }
        }
    }
    let n = cap.min(pending.len());
    ptr::copy_nonoverlapping(pending.as_ptr(), buf, n);
    let _ = pending.split_to(n);
    n as i64
}

// ===================== gRPC =====================

/// 単項（1 要求 → 1 応答）
pub const VEIL_GRPC_UNARY: u32 = 0;
/// サーバストリーミング（1 要求 → 応答の列）
pub const VEIL_GRPC_SERVER_STREAMING: u32 = 1;
/// クライアントストリーミング（要求の列 → 1 応答）
pub const VEIL_GRPC_CLIENT_STREAMING: u32 = 2;
/// 双方向ストリーミング
pub const VEIL_GRPC_BIDI: u32 = 3;

/// gRPC の 1 回の呼び出し（不透明。コールバックの間だけ有効）。
#[allow(non_camel_case_types)]
pub struct veil_grpc_call_t {
    call: *mut GrpcCall,
    kind: u32,
    /// 単項の入力（先に 1 通だけ読んでおく）
    unary_in: Option<Bytes>,
    unary_taken: bool,
    /// 最後に返した受信メッセージ（ポインタを次の recv まで生かす）
    last: Bytes,
    status: Option<Status>,
}

/// gRPC のハンドラ。0 を返すと `veil_grpc_set_status` の状態（未設定なら OK）で終える。非 0 は INTERNAL。
#[allow(non_camel_case_types)]
pub type veil_grpc_fn = unsafe extern "C" fn(*mut veil_grpc_call_t, *mut c_void) -> c_int;

fn single_in(kind: u32) -> bool {
    kind == VEIL_GRPC_UNARY || kind == VEIL_GRPC_SERVER_STREAMING
}

fn single_out(kind: u32) -> bool {
    kind == VEIL_GRPC_UNARY || kind == VEIL_GRPC_CLIENT_STREAMING
}

/// gRPC のメソッドを登録する（`path` は `/<package>.<Service>/<Method>`、`kind` は `VEIL_GRPC_*`）。
/// コールバックはハンドラ用プールで呼ばれる。メッセージは符号化済みのバイト列（protobuf-c・nanopb 等で）。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`path` は NUL 終端の C 文字列。`user_data` の契約は `veil_register_route` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_register_grpc(
    cfg: *mut veil_config_t,
    path: *const c_char,
    kind: u32,
    handler: Option<veil_grpc_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let Some(func) = handler else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL handler");
    };
    let path = tryc!(cstr(path));
    if !path.starts_with('/') || path.matches('/').count() != 2 {
        return fail(
            VEIL_ERR_INVALID_ARG,
            "path must be /<package>.<Service>/<Method>",
        );
    }
    if kind > VEIL_GRPC_BIDI {
        return fail(VEIL_ERR_INVALID_ARG, "unknown gRPC kind");
    }
    let path = path.to_string();
    let ud = UserData(user_data);
    // F-265: OpenAPI に載せる種類
    let method_kind = MethodKind::from_streaming(!single_in(kind), !single_out(kind));
    cfg.extra.push(Box::new(move |app: App| {
        app.grpc_blocking(&path, move |call: &mut GrpcCall| {
            grpc_invoke(func, ud, kind, call)
        })
        .grpc_kind(&path, method_kind)
    }));
    VEIL_OK
}

// F-265
/// gRPC のメソッドの型を OpenAPI に載せるための `FileDescriptorSet`（`protoc --include_imports
/// --descriptor_set_out` の出力）を渡す。`data` はこの呼び出しの間だけ読む（中で複製する）。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`data` は `len` バイト読める（`len` が 0 なら NULL 可）。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_descriptors(
    cfg: *mut veil_config_t,
    data: *const u8,
    len: usize,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    if data.is_null() && len != 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL descriptor data");
    }
    let set = if len == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(data, len).to_vec()
    };
    cfg.extra
        .push(Box::new(move |app: App| app.grpc_descriptors(&[&set])));
    VEIL_OK
}

fn grpc_invoke(func: veil_grpc_fn, ud: UserData, kind: u32, call: &mut GrpcCall) -> Status {
    // 単項の入力はちょうど 1 通（0 通・2 通以上は INTERNAL。tonic・grpc-go と同じ）
    let unary_in = if single_in(kind) {
        let first = match call.recv() {
            Ok(Some(b)) => b,
            Ok(None) => return Status::internal("missing request message"),
            Err(s) => return s,
        };
        match call.recv() {
            Ok(None) => {}
            Ok(Some(_)) => {
                return Status::internal("unary call received more than one request message")
            }
            Err(s) => return s,
        }
        Some(first)
    } else {
        None
    };
    let mut c = veil_grpc_call_t {
        call,
        kind,
        unary_in,
        unary_taken: false,
        last: Bytes::new(),
        status: None,
    };
    let rc = catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: c はこの呼び出しの間だけ生きる。user_data の契約は UserData を参照。
        unsafe { func(&mut c, ud.get()) }
    }));
    match rc {
        Ok(0) => {
            let st = c.status.take().unwrap_or_else(|| Status::new(Code::Ok, ""));
            // SAFETY: call はこの関数の引数（&mut）
            let sent = unsafe { (*c.call).counts().1 };
            if st.code() == Code::Ok && single_out(kind) && sent == 0 {
                return Status::internal("no response message");
            }
            st
        }
        Ok(_) => c
            .status
            .take()
            .filter(|s| s.code() != Code::Ok)
            .unwrap_or_else(|| Status::internal("handler returned an error")),
        Err(_) => Status::internal("handler panicked"),
    }
}

/// 次の受信メッセージ。1 = 受け取った（`*data`・`*len`。次の recv かコールバックの終わりまで有効）、0 = 終わり、
/// 負 = エラー（壊れた・大きすぎるメッセージ。状態は自動で設定される）。単項・サーバストリーミングは 1 通だけ返す。
///
/// # Safety
///
/// `call` はコールバックに渡されたハンドル、`data`・`len` は書き込める。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_recv(
    call: *mut veil_grpc_call_t,
    data: *mut *const u8,
    len: *mut usize,
) -> c_int {
    let Some(c) = call.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL call");
    };
    if data.is_null() || len.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL out pointer");
    }
    let msg = if single_in(c.kind) {
        if c.unary_taken {
            None
        } else {
            c.unary_taken = true;
            c.unary_in.take()
        }
    } else {
        match (*c.call).recv() {
            Ok(m) => m,
            Err(s) => {
                let msg = s.message().to_string();
                c.status = Some(s);
                return fail(VEIL_ERR_CLOSED, msg);
            }
        }
    };
    match msg {
        Some(b) => {
            c.last = b;
            *data = c.last.as_ptr();
            *len = c.last.len();
            1
        }
        None => {
            *data = ptr::null();
            *len = 0;
            0
        }
    }
}

/// メッセージを送る（コピーする）。単項・クライアントストリーミングは 1 回だけ（2 回目は `VEIL_ERR_STATE`）。
/// クライアントが切断していたら `VEIL_ERR_CLOSED`。
///
/// # Safety
///
/// `call` は有効なハンドル、`data` は `len` バイト読める。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_send(
    call: *mut veil_grpc_call_t,
    data: *const u8,
    len: usize,
) -> c_int {
    let Some(c) = call.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL call");
    };
    if data.is_null() && len > 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL data");
    }
    if single_out(c.kind) && (*c.call).counts().1 > 0 {
        return fail(VEIL_ERR_STATE, "a unary response is sent only once");
    }
    let b = if len == 0 {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(data, len))
    };
    if (*c.call).send(b) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_CLOSED, "client cancelled the call")
    }
}

/// 終えるときの状態（gRPC のステータスコード 0〜16 と、NULL 可のメッセージ）。
///
/// # Safety
///
/// `call` は有効なハンドル、`message` は NULL か NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_set_status(
    call: *mut veil_grpc_call_t,
    code: c_int,
    message: *const c_char,
) -> c_int {
    let Some(c) = call.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL call");
    };
    let Some(code) = u8::try_from(code).ok().and_then(Code::from_u8) else {
        return fail(VEIL_ERR_INVALID_ARG, "gRPC status code must be 0..=16");
    };
    let msg = if message.is_null() {
        ""
    } else {
        tryc!(cstr(message))
    };
    c.status = Some(Status::new(code, msg));
    VEIL_OK
}

/// 応答ヘッダ（メタデータ）を足す（最初の `veil_grpc_send` より前だけ）。
///
/// # Safety
///
/// `call` は有効なハンドル、`name`・`value` は NUL 終端の C 文字列。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_set_header(
    call: *mut veil_grpc_call_t,
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    let Some(c) = call.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL call");
    };
    let (n, v) = (tryc!(cstr(name)), tryc!(cstr(value)));
    if !super::valid_header(n, v) {
        return fail(VEIL_ERR_INVALID_ARG, "invalid header");
    }
    if (*c.call).add_header(
        Bytes::copy_from_slice(n.to_ascii_lowercase().as_bytes()),
        Bytes::copy_from_slice(v.as_bytes()),
    ) {
        VEIL_OK
    } else {
        fail(VEIL_ERR_STATE, "headers are already sent")
    }
}

/// 正常終了のトレーラを足す。
///
/// # Safety
///
/// `veil_grpc_set_header` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_set_trailer(
    call: *mut veil_grpc_call_t,
    name: *const c_char,
    value: *const c_char,
) -> c_int {
    let Some(c) = call.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL call");
    };
    let (n, v) = (tryc!(cstr(name)), tryc!(cstr(value)));
    if !super::valid_header(n, v) {
        return fail(VEIL_ERR_INVALID_ARG, "invalid trailer");
    }
    (*c.call).add_trailer(
        Bytes::copy_from_slice(n.to_ascii_lowercase().as_bytes()),
        Bytes::copy_from_slice(v.as_bytes()),
    );
    VEIL_OK
}

/// 要求メタデータの値（名前は大文字小文字を区別しない。NUL 終端ではない）。無ければ NULL。
///
/// # Safety
///
/// `call` は有効なハンドル、`name` は NUL 終端の C 文字列、`len` は NULL か書き込める。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_metadata(
    call: *const veil_grpc_call_t,
    name: *const c_char,
    len: *mut usize,
) -> *const c_char {
    let (Some(c), Ok(name)) = (call.as_ref(), cstr(name)) else {
        return out_none(len);
    };
    match (*c.call)
        .metadata()
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
    {
        Some((_, v)) => out_bytes(v, len),
        None => out_none(len),
    }
}

/// 呼ばれたメソッド（`/<package>.<Service>/<Method>`。NUL 終端ではない）。
///
/// # Safety
///
/// `call` は有効なハンドル、`len` は NULL か書き込める。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_method(
    call: *const veil_grpc_call_t,
    len: *mut usize,
) -> *const c_char {
    match call.as_ref() {
        Some(c) => out_bytes((*c.call).method().as_bytes(), len),
        None => out_none(len),
    }
}

/// 期限までの残り（ミリ秒。期限が無ければ -1、過ぎていれば 0）。
///
/// # Safety
///
/// `call` は有効なハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_grpc_deadline_ms(call: *const veil_grpc_call_t) -> i64 {
    match call.as_ref().and_then(|c| (*c.call).deadline()) {
        Some(d) => d
            .saturating_duration_since(std::time::Instant::now())
            .as_millis() as i64,
        None => -1,
    }
}

/// 標準のヘルスチェック（`grpc.health.v1.Health` の Check・Watch）を登録する。サーバ全体（`""`）は SERVING。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_register_grpc_health(cfg: *mut veil_config_t) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    cfg.extra.push(Box::new(|app: App| app.grpc_health()));
    VEIL_OK
}

// ===================== L4 =====================

/// L4 の TCP 接続（不透明。`on_connect` から `on_close` まで有効）。
#[allow(non_camel_case_types)]
pub struct veil_l4_conn_t {
    peer: String,
    data: *mut c_void,
    /// コールバックの間だけ指す出力
    out: *mut L4Out,
}

/// TCP のコールバック（どれも NULL 可。ハンドラ用プールで、接続ごとに 1 つずつ順に呼ばれる）。
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_l4_tcp_callbacks_t {
    /// 接続。非 0 なら閉じる。
    pub on_connect: Option<unsafe extern "C" fn(*mut veil_l4_conn_t, *mut c_void) -> c_int>,
    /// 受信。非 0 なら（書いたデータを送ってから）閉じる。
    pub on_data:
        Option<unsafe extern "C" fn(*mut veil_l4_conn_t, *const u8, usize, *mut c_void) -> c_int>,
    /// 接続が閉じた（`veil_l4_conn_data` の後始末はここで）。
    pub on_close: Option<unsafe extern "C" fn(*mut veil_l4_conn_t, *mut c_void)>,
}

struct CSession {
    cb: veil_l4_tcp_callbacks_t,
    ud: UserData,
    conn: Box<veil_l4_conn_t>,
}

// SAFETY: conn.data は C の利用者の値で、同じ接続のコールバックは 1 つずつ順に呼ばれる（並行しない）。
// cb の関数ポインタと user_data の契約は veil_register_route と同じ。
unsafe impl Send for CSession {}

impl CSession {
    fn with_out<R>(
        &mut self,
        out: &mut L4Out,
        f: impl FnOnce(*mut veil_l4_conn_t, *mut c_void) -> R,
    ) -> R {
        self.conn.out = out;
        let r = f(&mut *self.conn, self.ud.get());
        self.conn.out = ptr::null_mut();
        r
    }
}

impl L4Session for CSession {
    fn on_data(&mut self, data: &[u8], out: &mut L4Out) {
        let Some(f) = self.cb.on_data else { return };
        let rc = self.with_out(out, |c, ud| {
            catch_unwind(AssertUnwindSafe(|| unsafe {
                f(c, data.as_ptr(), data.len(), ud)
            }))
        });
        if !matches!(rc, Ok(0)) {
            out.close();
        }
    }

    fn on_close(&mut self) {
        if let Some(f) = self.cb.on_close {
            let ud = self.ud.get();
            let c: *mut veil_l4_conn_t = &mut *self.conn;
            let _ = catch_unwind(AssertUnwindSafe(|| unsafe { f(c, ud) }));
        }
    }
}

/// L4 の TCP リスナー `listen`（例 `"0.0.0.0:7000"`）を開き、接続をコールバックで処理する。
/// 受信のたびにハンドラ用プールで `on_data` が呼ばれ、その中で `veil_l4_write` したデータが戻った後に送られる。
///
/// # Safety
///
/// `cfg` は有効な設定ハンドル、`name`・`listen` は NUL 終端の C 文字列、`cb` は有効な構造体（コピーされる）。
/// `user_data` の契約は `veil_register_route` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_register_l4_tcp(
    cfg: *mut veil_config_t,
    name: *const c_char,
    listen: *const c_char,
    cb: *const veil_l4_tcp_callbacks_t,
    user_data: *mut c_void,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let Some(cb) = cb.as_ref().copied() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL callbacks");
    };
    let (name, listen) = (
        tryc!(cstr(name)).to_string(),
        tryc!(cstr(listen)).to_string(),
    );
    if listen.parse::<SocketAddr>().is_err() {
        return fail(VEIL_ERR_INVALID_ARG, "listen must be host:port");
    }
    let ud = UserData(user_data);
    cfg.extra.push(Box::new(move |app: App| {
        app.l4_tcp_blocking(&name, &listen, move |peer, out| {
            let mut s = CSession {
                cb,
                ud,
                conn: Box::new(veil_l4_conn_t {
                    peer: peer.to_string(),
                    data: ptr::null_mut(),
                    out: ptr::null_mut(),
                }),
            };
            if let Some(f) = cb.on_connect {
                let rc = s.with_out(out, |c, ud| {
                    catch_unwind(AssertUnwindSafe(|| unsafe { f(c, ud) }))
                });
                if !matches!(rc, Ok(0)) {
                    out.close();
                }
            }
            Some(Box::new(s) as Box<dyn L4Session>)
        })
    }));
    VEIL_OK
}

/// 応答を `101 Switching Protocols`（`protocol` へのアップグレード。HTTP/1.1 だけ。F-261）にする。
/// 101 を書いた後の接続は L4 の TCP ハンドラと同じコールバックで受ける（`on_connect` → `on_data` … →
/// `on_close`。ハンドラ用プールで 1 つずつ順に呼ばれ、`veil_l4_write`・`veil_l4_close` が使える）。
/// `resp` に設定済みのヘッダは 101 と一緒に送る。要求がアップグレードでない・HTTP/2・HTTP/3 では 400 になる。
///
/// # Safety
///
/// `resp` はコールバックに渡されたハンドル、`protocol` は NUL 終端の C 文字列、`cb` は有効な構造体（コピーされる）。
/// `user_data` の契約は `veil_register_route` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_response_upgrade(
    resp: *mut veil_response_t,
    protocol: *const c_char,
    cb: *const veil_l4_tcp_callbacks_t,
    user_data: *mut c_void,
) -> c_int {
    let Some(r) = resp.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL response");
    };
    let Some(cb) = cb.as_ref().copied() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL callbacks");
    };
    let protocol = tryc!(cstr(protocol)).to_string();
    let ud = UserData(user_data);
    let mut up = veil::api::Response::switching_protocols(&protocol, move |up| async move {
        let peer = up.client_ip().to_string();
        up.serve_blocking(move |out| {
            let mut s = CSession {
                cb,
                ud,
                conn: Box::new(veil_l4_conn_t {
                    peer,
                    data: ptr::null_mut(),
                    out: ptr::null_mut(),
                }),
            };
            if let Some(f) = cb.on_connect {
                let rc = s.with_out(out, |c, ud| {
                    catch_unwind(AssertUnwindSafe(|| unsafe { f(c, ud) }))
                });
                if !matches!(rc, Ok(0)) {
                    out.close();
                }
            }
            Some(Box::new(s) as Box<dyn L4Session>)
        })
        .await
    });
    for (n, v) in r.resp.headers() {
        up = up.with_header(n.clone(), v.clone());
    }
    r.resp = up;
    VEIL_OK
}

/// 送るデータを足す（コピーする。コールバック（`on_connect`・`on_data`）の中でだけ呼べる）。
///
/// # Safety
///
/// `conn` はコールバックに渡されたハンドル、`data` は `len` バイト読める。
#[no_mangle]
pub unsafe extern "C" fn veil_l4_write(
    conn: *mut veil_l4_conn_t,
    data: *const u8,
    len: usize,
) -> c_int {
    let Some(c) = conn.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL connection");
    };
    let Some(out) = c.out.as_mut() else {
        return fail(
            VEIL_ERR_STATE,
            "veil_l4_write is only valid inside a callback",
        );
    };
    if data.is_null() && len > 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL data");
    }
    if len > 0 {
        out.write(Bytes::copy_from_slice(std::slice::from_raw_parts(
            data, len,
        )));
    }
    VEIL_OK
}

/// 送り終えたら接続を閉じる（コールバックの中で）。
///
/// # Safety
///
/// `conn` はコールバックに渡されたハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_l4_close(conn: *mut veil_l4_conn_t) {
    if let Some(out) = conn.as_mut().and_then(|c| c.out.as_mut()) {
        out.close();
    }
}

/// 相手のアドレス（`ip:port`。NUL 終端ではない）。
///
/// # Safety
///
/// `conn` は有効なハンドル、`len` は NULL か書き込める。
#[no_mangle]
pub unsafe extern "C" fn veil_l4_peer(
    conn: *const veil_l4_conn_t,
    len: *mut usize,
) -> *const c_char {
    match conn.as_ref() {
        Some(c) => out_bytes(c.peer.as_bytes(), len),
        None => out_none(len),
    }
}

/// 接続ごとの利用者のデータを置く（`on_close` で解放する）。
///
/// # Safety
///
/// `conn` は有効なハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_l4_set_conn_data(conn: *mut veil_l4_conn_t, data: *mut c_void) {
    if let Some(c) = conn.as_mut() {
        c.data = data;
    }
}

/// 接続ごとの利用者のデータ（未設定は NULL）。
///
/// # Safety
///
/// `conn` は有効なハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_l4_conn_data(conn: *const veil_l4_conn_t) -> *mut c_void {
    conn.as_ref().map(|c| c.data).unwrap_or(ptr::null_mut())
}

/// UDP の返信（不透明。コールバックの間だけ有効）。
#[allow(non_camel_case_types)]
pub struct veil_l4_reply_t {
    reply: Option<Bytes>,
}

/// UDP のコールバック。`peer` は `ip:port` の NUL 終端文字列。`veil_l4_reply` で返信する。
#[allow(non_camel_case_types)]
pub type veil_l4_udp_fn = unsafe extern "C" fn(
    *const u8,
    usize,
    *const c_char,
    *mut veil_l4_reply_t,
    *mut c_void,
) -> c_int;

/// L4 の UDP リスナーを開き、データグラムごとにハンドラ用プールで `f` を呼ぶ。
///
/// # Safety
///
/// `veil_register_l4_tcp` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_register_l4_udp(
    cfg: *mut veil_config_t,
    name: *const c_char,
    listen: *const c_char,
    f: Option<veil_l4_udp_fn>,
    user_data: *mut c_void,
) -> c_int {
    let Some(cfg) = cfg.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL config");
    };
    let Some(f) = f else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL handler");
    };
    let (name, listen) = (
        tryc!(cstr(name)).to_string(),
        tryc!(cstr(listen)).to_string(),
    );
    if listen.parse::<SocketAddr>().is_err() {
        return fail(VEIL_ERR_INVALID_ARG, "listen must be host:port");
    }
    let ud = UserData(user_data);
    cfg.extra.push(Box::new(move |app: App| {
        app.l4_udp_blocking(&name, &listen, move |data, peer| {
            let peer = std::ffi::CString::new(peer.to_string()).unwrap_or_default();
            let mut reply = veil_l4_reply_t { reply: None };
            let rc = catch_unwind(AssertUnwindSafe(|| unsafe {
                f(
                    data.as_ptr(),
                    data.len(),
                    peer.as_ptr(),
                    &mut reply,
                    ud.get(),
                )
            }));
            match rc {
                Ok(0) => reply.reply,
                _ => None,
            }
        })
    }));
    VEIL_OK
}

/// UDP の返信を設定する（コピーする。最後に設定したものを送る）。
///
/// # Safety
///
/// `reply` はコールバックに渡されたハンドル、`data` は `len` バイト読める。
#[no_mangle]
pub unsafe extern "C" fn veil_l4_reply(
    reply: *mut veil_l4_reply_t,
    data: *const u8,
    len: usize,
) -> c_int {
    let Some(r) = reply.as_mut() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL reply");
    };
    if data.is_null() && len > 0 {
        return fail(VEIL_ERR_INVALID_ARG, "NULL data");
    }
    r.reply = Some(Bytes::copy_from_slice(std::slice::from_raw_parts(
        data, len,
    )));
    VEIL_OK
}

// ===================== WebSocket（F-262） =====================

/// テキストのメッセージ（`veil_ws_send`・`on_message` の opcode）。
pub const VEIL_WS_TEXT: c_int = 1;
/// バイナリのメッセージ。
pub const VEIL_WS_BINARY: c_int = 2;

/// WebSocket の接続（不透明。`on_open` から `on_close` が戻るまで有効。`veil_ws_send`・`veil_ws_close` は
/// その間ならどのスレッドからでも呼べる）。
#[allow(non_camel_case_types)]
pub struct veil_ws_t {
    tx: veil::api::ws::WsSender,
    /// 相手の IP アドレス（NUL 終端で持つ。返す長さは NUL を含まない）
    peer: String,
    /// 利用者のデータ（他のスレッドの `veil_ws_send` と同時に触れられるので原子的に置く）
    data: std::sync::atomic::AtomicPtr<c_void>,
}

/// WebSocket のコールバック（どれも NULL 可。ハンドラ用プールで、接続ごとに 1 つずつ順に呼ばれる）。
#[repr(C)]
#[allow(non_camel_case_types)]
#[derive(Clone, Copy)]
pub struct veil_ws_callbacks_t {
    /// 接続した。非 0 なら閉じる（1000）。
    pub on_open: Option<unsafe extern "C" fn(*mut veil_ws_t, *mut c_void) -> c_int>,
    /// メッセージ（`opcode` は `VEIL_WS_TEXT` か `VEIL_WS_BINARY`。テキストは UTF-8 検査済み）。非 0 なら閉じる（1000）。
    pub on_message:
        Option<unsafe extern "C" fn(*mut veil_ws_t, c_int, *const u8, usize, *mut c_void) -> c_int>,
    /// 閉じた（`code` は相手の Close のコード。Close 無しで切れたら 1006）。`veil_ws_t` はこの後無効になる。
    pub on_close: Option<unsafe extern "C" fn(*mut veil_ws_t, u16, *mut c_void)>,
}

struct CWsSession {
    cb: veil_ws_callbacks_t,
    ud: UserData,
    ws: Box<veil_ws_t>,
}

// SAFETY: ws.data は C の利用者の値で、同じ接続のコールバックは 1 つずつ順に呼ばれる（並行しない）。
// cb の関数ポインタと user_data の契約は veil_register_route と同じ。
unsafe impl Send for CWsSession {}

impl veil::api::ws::WsSession for CWsSession {
    fn on_message(&mut self, msg: veil::api::ws::Message) {
        let Some(f) = self.cb.on_message else { return };
        let (op, data): (c_int, &[u8]) = match &msg {
            veil::api::ws::Message::Text(t) => (VEIL_WS_TEXT, t.as_bytes()),
            veil::api::ws::Message::Binary(b) => (VEIL_WS_BINARY, &b[..]),
            _ => return,
        };
        let ud = self.ud.get();
        let ws = &*self.ws as *const veil_ws_t as *mut veil_ws_t;
        let rc = catch_unwind(AssertUnwindSafe(|| unsafe {
            f(ws, op, data.as_ptr(), data.len(), ud)
        }));
        if !matches!(rc, Ok(0)) {
            let _ = self.ws.tx.send(veil::api::ws::Message::Close(Some(
                veil::api::ws::CloseFrame {
                    code: 1000,
                    reason: veil::api::ws::Utf8Bytes::from_static(""),
                },
            )));
        }
    }

    fn on_close(&mut self, code: u16) {
        if let Some(f) = self.cb.on_close {
            let ud = self.ud.get();
            let ws = &*self.ws as *const veil_ws_t as *mut veil_ws_t;
            let _ = catch_unwind(AssertUnwindSafe(|| unsafe { f(ws, code, ud) }));
        }
    }
}

/// 要求を WebSocket にアップグレードする（ハンドラの中で呼ぶ）。ハンドシェイクを検査し、正しければ `resp` を
/// `101 Switching Protocols` にして 0 を返す。その後の接続はコールバックで受ける（ハンドラ用プールで 1 つずつ順に）。
/// 正しくなければ `resp` を 400（版違いは 426）にして `VEIL_ERR_STATE` を返す（ハンドラはそのまま 0 を返してよい）。
///
/// # Safety
///
/// `req`・`resp` はコールバックに渡されたハンドル、`cb` は有効な構造体（コピーされる）。
/// `user_data` の契約は `veil_register_route` と同じ。
#[no_mangle]
pub unsafe extern "C" fn veil_ws_accept(
    req: *const veil_request_t,
    resp: *mut veil_response_t,
    cb: *const veil_ws_callbacks_t,
    user_data: *mut c_void,
) -> c_int {
    let (Some(q), Some(r)) = (req.as_ref(), resp.as_mut()) else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL request or response");
    };
    let Some(cb) = cb.as_ref().copied() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL callbacks");
    };
    use veil::api::FromRequest as _;
    let up = match veil::api::ws::WebSocketUpgrade::from_request(&q.req) {
        Ok(u) => u,
        Err(e) => {
            r.resp = e;
            return fail(VEIL_ERR_STATE, "not a valid WebSocket handshake");
        }
    };
    let ud = UserData(user_data);
    let mut peer = q.req.client_ip().to_string();
    peer.push('\0');
    let mut ws = up.on_upgrade_blocking(move |tx| {
        let s = CWsSession {
            cb,
            ud,
            ws: Box::new(veil_ws_t {
                tx,
                peer,
                data: std::sync::atomic::AtomicPtr::new(ptr::null_mut()),
            }),
        };
        if let Some(f) = cb.on_open {
            let ws = &*s.ws as *const veil_ws_t as *mut veil_ws_t;
            let rc = catch_unwind(AssertUnwindSafe(|| unsafe { f(ws, ud.get()) }));
            if !matches!(rc, Ok(0)) {
                return None;
            }
        }
        Some(Box::new(s) as Box<dyn veil::api::ws::WsSession>)
    });
    for (n, v) in r.resp.headers() {
        ws = ws.with_header(n.clone(), v.clone());
    }
    r.resp = ws;
    VEIL_OK
}

/// メッセージを送る（`data` はコピーされる。`opcode` は `VEIL_WS_TEXT`（UTF-8 であること）か `VEIL_WS_BINARY`）。
/// 送信キューに積んで戻る（待たない）。閉じた後は `VEIL_ERR_CLOSED`、キューが一杯（16 MiB）なら `VEIL_ERR_STATE`。
///
/// # Safety
///
/// `ws` は `on_open` から `on_close` が戻るまでの有効なハンドル、`data` は `len` バイト読める（`len` が 0 なら NULL 可）。
#[no_mangle]
pub unsafe extern "C" fn veil_ws_send(
    ws: *mut veil_ws_t,
    opcode: c_int,
    data: *const u8,
    len: usize,
) -> c_int {
    let Some(w) = ws.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL websocket");
    };
    let bytes = if len == 0 {
        Bytes::new()
    } else if data.is_null() {
        return fail(VEIL_ERR_INVALID_ARG, "NULL data");
    } else {
        Bytes::copy_from_slice(std::slice::from_raw_parts(data, len))
    };
    let msg = match opcode {
        VEIL_WS_TEXT => match veil::api::ws::Utf8Bytes::try_from(bytes) {
            Ok(t) => veil::api::ws::Message::Text(t),
            Err(_) => return fail(super::VEIL_ERR_UTF8, "text message is not valid UTF-8"),
        },
        VEIL_WS_BINARY => veil::api::ws::Message::Binary(bytes),
        _ => {
            return fail(
                VEIL_ERR_INVALID_ARG,
                "opcode must be VEIL_WS_TEXT or VEIL_WS_BINARY",
            )
        }
    };
    match w.tx.send(msg) {
        Ok(()) => VEIL_OK,
        Err(veil::api::ws::WsError::Full) => fail(VEIL_ERR_STATE, "websocket send queue is full"),
        Err(_) => fail(VEIL_ERR_CLOSED, "websocket is closed"),
    }
}

/// 閉じる手順を始める（`code` の Close を送る。相手の Close を受けると `on_close` が呼ばれる）。どのスレッドからでも。
///
/// # Safety
///
/// `ws` は `on_open` から `on_close` が戻るまでの有効なハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_ws_close(ws: *mut veil_ws_t, code: u16) -> c_int {
    let Some(w) = ws.as_ref() else {
        return fail(VEIL_ERR_INVALID_ARG, "NULL websocket");
    };
    let frame = veil::api::ws::CloseFrame {
        code,
        reason: veil::api::ws::Utf8Bytes::from_static(""),
    };
    match w.tx.send(veil::api::ws::Message::Close(Some(frame))) {
        Ok(()) => VEIL_OK,
        Err(_) => fail(VEIL_ERR_CLOSED, "websocket is closed"),
    }
}

/// 相手の IP アドレス（NUL 終端。`veil_ws_t` が有効な間だけ有効）。
///
/// # Safety
///
/// `ws` は有効なハンドル、`len` は NULL か書き込める。
#[no_mangle]
pub unsafe extern "C" fn veil_ws_peer(ws: *const veil_ws_t, len: *mut usize) -> *const c_char {
    match ws.as_ref() {
        Some(w) => out_bytes(&w.peer.as_bytes()[..w.peer.len() - 1], len),
        None => out_none(len),
    }
}

/// 接続ごとの利用者のデータを置く。
///
/// # Safety
///
/// `ws` は有効なハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_ws_set_data(ws: *mut veil_ws_t, data: *mut c_void) {
    if let Some(w) = ws.as_ref() {
        w.data.store(data, std::sync::atomic::Ordering::Release);
    }
}

/// `veil_ws_set_data` で置いたデータ（無ければ NULL）。
///
/// # Safety
///
/// `ws` は有効なハンドル。
#[no_mangle]
pub unsafe extern "C" fn veil_ws_data(ws: *const veil_ws_t) -> *mut c_void {
    ws.as_ref()
        .map(|w| w.data.load(std::sync::atomic::Ordering::Acquire))
        .unwrap_or(ptr::null_mut())
}
