//! Header Map Host Functions

use crate::wasm::exports::GuestMemory;
use wasmtime::{Caller, Linker};

use std::borrow::Cow;

use crate::wasm::constants::*;
use crate::wasm::context::{HostState, HttpContext};

// B-19: マップ直列化は SDK 互換のワイヤ形式を実装する共通モジュールへ集約
use super::abi::{deserialize_headers, serialize_pairs};

/// ゲストに見せる疑似ヘッダの名前（B-408）。Envoy と同じく、要求ヘッダの表には `:authority`・`:path`・`:method`・
/// `:scheme`、応答ヘッダの表には `:status` がある（Envoy 向けのフィルタは `:path` 等をヘッダとして読む）。
/// 値は context の要求・応答から作り、`request_headers`・`response_headers` には入れない（上流・クライアントへ
/// 送るヘッダと混ざらない）。
const REQUEST_PSEUDO: [&[u8]; 4] = [b":authority", b":path", b":method", b":scheme"];
const RESPONSE_PSEUDO: [&[u8]; 1] = [b":status"];

/// ホストが疑似ヘッダを context から作る context か（B-408）。プロトコルの経路は要求の属性（[`HttpContext::request_info`]）を
/// 置く。置かない呼び出し元（メッシュのデータプレーン）は疑似ヘッダを自分で表に入れ、ゲストが書き換えた値も受け取るので、
/// 表をそのまま見せて書かせる
fn virtual_pseudo(ctx: &HttpContext) -> bool {
    ctx.request_info.is_some()
}

/// この表に見せる疑似ヘッダの名前（B-408）。要求を持つ context（要求の段）・応答を持つ context（応答の段）だけ
fn pseudo_names(ctx: &HttpContext, map_type: i32) -> &'static [&'static [u8]] {
    if !virtual_pseudo(ctx) {
        return &[];
    }
    match map_type {
        HTTP_REQUEST_HEADERS if !ctx.request_method.is_empty() => &REQUEST_PSEUDO,
        HTTP_RESPONSE_HEADERS if ctx.response_status != 0 => &RESPONSE_PSEUDO,
        _ => &[],
    }
}

/// 疑似ヘッダの値（B-408）。この表に見せないもの・値の無いもの（`Host` の無い HTTP/1.0 の `:authority`）は `None`
fn pseudo_value<'a>(ctx: &'a HttpContext, map_type: i32, name: &[u8]) -> Option<Cow<'a, [u8]>> {
    let name = *pseudo_names(ctx, map_type)
        .iter()
        .find(|n| n.eq_ignore_ascii_case(name))?;
    match name {
        b":authority" => ctx.request_authority().map(Cow::Borrowed),
        b":path" => Some(Cow::Borrowed(ctx.request_path.as_bytes())),
        b":method" => Some(Cow::Borrowed(ctx.request_method.as_bytes())),
        b":scheme" => Some(Cow::Borrowed(ctx.request_scheme().as_bytes())),
        b":status" => Some(Cow::Owned(ctx.response_status.to_string().into_bytes())),
        _ => None,
    }
}

/// 表の組（疑似ヘッダを先頭に）を Proxy-Wasm ABI 形式へ直列化する（B-408）。疑似ヘッダを見せる表では、保持している
/// ヘッダのうち `:` で始まるものは並べない（疑似ヘッダが 2 つにならない）
fn serialize_map(ctx: &HttpContext, map_type: i32, headers: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let names = pseudo_names(ctx, map_type);
    let pseudo: Vec<(&[u8], Cow<'_, [u8]>)> = names
        .iter()
        .filter_map(|n| pseudo_value(ctx, map_type, n).map(|v| (*n, v)))
        .collect();
    serialize_pairs(
        pseudo.iter().map(|(n, v)| (*n, &v[..])).chain(
            headers
                .iter()
                .filter(move |(k, _)| names.is_empty() || !k.starts_with(b":"))
                .map(|(k, v)| (&k[..], &v[..])),
        ),
    )
}

/// ゲストが `:` で始まる名前を書き込もうとしたときの判断（B-408）。疑似ヘッダを今と同じ値に置くこと（表の組の
/// 読み書きの往復）は `Some(Ok)`（何も変えない）、それ以外は `Some(BadArgument)`（パス・メソッド・状態コードの
/// 書き換えはできない。保持するヘッダに `:` で始まる名前を入れると、HTTP/1.1 の上流への要求に不正な行として書かれる）。
/// `:` で始まらない名前と、疑似ヘッダを作らない context では `None`（ふつうのヘッダとして扱う）
fn pseudo_write(ctx: &HttpContext, map_type: i32, name: &[u8], value: &[u8]) -> Option<i32> {
    if !name.starts_with(b":") || !virtual_pseudo(ctx) {
        return None;
    }
    match pseudo_value(ctx, map_type, name) {
        Some(v) if v.as_ref() == value => Some(PROXY_RESULT_OK),
        _ => Some(PROXY_RESULT_BAD_ARGUMENT),
    }
}

/// ゲストの書くヘッダが名前は token（擬似ヘッダの `:` は許す）、値に CR・LF・NUL が無いか。
/// B-318: 満たさないものは BadArgument にする（HTTP/1.1 の上流への要求・クライアントへの応答へそのまま
/// 書かれ、要求の密輸・応答の分割になっていた。F40）
fn guest_field_ok(name: &[u8], value: &[u8]) -> bool {
    let n = name.strip_prefix(b":").unwrap_or(name);
    crate::http_utils::is_valid_header_name(n) && crate::http_utils::is_valid_header_value(value)
}

/// Get headers by map type
fn get_headers(state: &HostState, map_type: i32) -> Option<&Vec<(Vec<u8>, Vec<u8>)>> {
    match map_type {
        HTTP_REQUEST_HEADERS => Some(&state.http_ctx.request_headers),
        HTTP_REQUEST_TRAILERS => Some(&state.http_ctx.request_trailers),
        HTTP_RESPONSE_HEADERS => Some(&state.http_ctx.response_headers),
        HTTP_RESPONSE_TRAILERS => Some(&state.http_ctx.response_trailers),
        HTTP_CALL_RESPONSE_HEADERS => {
            if let Some(token) = state.http_ctx.current_http_call_token {
                state
                    .http_ctx
                    .http_call_responses
                    .get(&token)
                    .map(|r| &r.headers)
            } else {
                None
            }
        }
        HTTP_CALL_RESPONSE_TRAILERS => {
            if let Some(token) = state.http_ctx.current_http_call_token {
                state
                    .http_ctx
                    .http_call_responses
                    .get(&token)
                    .map(|r| &r.trailers)
            } else {
                None
            }
        }
        // F-134: 従来は未認識の MapType として BadArgument になっていた。
        // `proxy_on_grpc_receive_initial_metadata`/`proxy_on_grpc_receive_trailing_metadata`
        // 呼び出し前に engine.rs が書き込む（grpc feature 無効時はそもそも
        // ゲストへ配送されないため、その場合はここへも到達せず空のまま）。
        #[cfg(feature = "grpc")]
        GRPC_RECEIVE_INITIAL_METADATA => Some(&state.http_ctx.grpc_receive_initial_metadata),
        #[cfg(feature = "grpc")]
        GRPC_RECEIVE_TRAILING_METADATA => Some(&state.http_ctx.grpc_receive_trailing_metadata),
        _ => None,
    }
}

/// Get mutable headers by map type
fn get_headers_mut(state: &mut HostState, map_type: i32) -> Option<&mut Vec<(Vec<u8>, Vec<u8>)>> {
    match map_type {
        // F-330: 本文の段で上流へ送る表を見せているなら、書き換える前に取っておく
        HTTP_REQUEST_HEADERS => Some(state.http_ctx.request_headers_mut()),
        HTTP_REQUEST_TRAILERS => Some(&mut state.http_ctx.request_trailers),
        HTTP_RESPONSE_HEADERS => Some(&mut state.http_ctx.response_headers),
        HTTP_RESPONSE_TRAILERS => Some(&mut state.http_ctx.response_trailers),
        _ => None,
    }
}

/// Check read capability for map type
fn check_read_capability(state: &HostState, map_type: i32) -> bool {
    match map_type {
        HTTP_REQUEST_HEADERS | HTTP_REQUEST_TRAILERS => {
            state.http_ctx.capabilities.allow_request_headers_read
        }
        HTTP_RESPONSE_HEADERS | HTTP_RESPONSE_TRAILERS => {
            state.http_ctx.capabilities.allow_response_headers_read
        }
        HTTP_CALL_RESPONSE_HEADERS | HTTP_CALL_RESPONSE_TRAILERS => {
            state.http_ctx.capabilities.allow_http_calls
        }
        // F-134: gRPC 受信メタデータも「外向き呼び出しの結果を読む」権限という点で
        // proxy_http_call のレスポンスヘッダ読み取りと同じ性質のため、既存の
        // `allow_http_calls` を流用する（gRPC 専用の capability は新設しない）。
        #[cfg(feature = "grpc")]
        GRPC_RECEIVE_INITIAL_METADATA | GRPC_RECEIVE_TRAILING_METADATA => {
            state.http_ctx.capabilities.allow_http_calls
        }
        _ => false,
    }
}

/// Check write capability for map type
fn check_write_capability(state: &HostState, map_type: i32) -> bool {
    match map_type {
        HTTP_REQUEST_HEADERS | HTTP_REQUEST_TRAILERS => {
            state.http_ctx.capabilities.allow_request_headers_write
        }
        HTTP_RESPONSE_HEADERS | HTTP_RESPONSE_TRAILERS => {
            state.http_ctx.capabilities.allow_response_headers_write
        }
        _ => false,
    }
}

/// F-134 適合度修正: 書き込み可能な MapType（0-3）かどうか。
///
/// Proxy-Wasm ABI v0.2.1 の Status には veil 独自拡張の `NOT_ALLOWED(13)` は
/// 存在せず、13 は「capability による拒否」専用として veil が予約した値
/// （`src/wasm/constants.rs` 参照）。読み取り専用 MapType（GrpcReceiveInitialMetadata=4 /
/// GrpcReceiveTrailingMetadata=5 / HttpCallResponseHeaders=6 / HttpCallResponseTrailers=7）
/// や未知の MapType への書き込みは capability の有無に関わらず仕様上 `BadArgument(2)`
/// であるべきなので、capability チェックより前にこの判定を行う。
fn is_writable_map_type(map_type: i32) -> bool {
    matches!(
        map_type,
        HTTP_REQUEST_HEADERS
            | HTTP_REQUEST_TRAILERS
            | HTTP_RESPONSE_HEADERS
            | HTTP_RESPONSE_TRAILERS
    )
}

/// F-134 適合度修正: host が認識する MapType（0-7）かどうか。
///
/// `get_headers`/`get_headers_mut` が `None` を返すのは 2 通りの意味を持つ:
/// (1) 完全に未知の MapType（`BadArgument` が正しい）、
/// (2) 認識はしている読み取り専用型だが、まだデータが無い（例:
/// `proxy_http_call`/gRPC 呼び出しの応答がまだ届いていない場合の
/// HttpCallResponseHeaders/GrpcReceiveInitialMetadata 等）。
/// 後者は ABI 上 `NotFound(1)` が適切（型自体は不正ではない）。
/// この判定を capability チェックの後・データ取得の後で使い分ける。
fn is_known_map_type(map_type: i32) -> bool {
    #[cfg(feature = "grpc")]
    if matches!(
        map_type,
        GRPC_RECEIVE_INITIAL_METADATA | GRPC_RECEIVE_TRAILING_METADATA
    ) {
        return true;
    }
    matches!(
        map_type,
        HTTP_REQUEST_HEADERS
            | HTTP_REQUEST_TRAILERS
            | HTTP_RESPONSE_HEADERS
            | HTTP_RESPONSE_TRAILERS
            | HTTP_CALL_RESPONSE_HEADERS
            | HTTP_CALL_RESPONSE_TRAILERS
    )
}

/// Helper to read string from WASM memory
fn read_string(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Option<String> {
    let memory = caller.guest_memory()?;
    let data = memory.data(caller);

    let start = ptr as u32 as usize;
    let end = start + len as u32 as usize;

    if end > data.len() {
        return None;
    }

    String::from_utf8(data[start..end].to_vec()).ok()
}

/// Helper to write to WASM memory
fn write_to_wasm(caller: &mut Caller<'_, HostState>, ptr: i32, data: &[u8]) -> bool {
    let memory = match caller.guest_memory() {
        Some(mem) => mem,
        _ => return false,
    };

    let mem_data = memory.data_mut(caller);
    let start = ptr as u32 as usize;
    let end = start + data.len();

    if end > mem_data.len() {
        return false;
    }

    mem_data[start..end].copy_from_slice(data);
    true
}

/// Add header functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_get_header_map_pairs
    // ゲストのアロケータ（proxy_on_memory_allocate）を今のファイバーの上で呼ぶ同期のホスト関数（F-318。`exports::allocate_guest_memory`）
    linker.func_wrap(
        "env",
        "proxy_get_header_map_pairs",
        |mut caller: Caller<'_, HostState>,
         map_type: i32,
         return_map_ptr: i32,
         return_map_size: i32|
         -> anyhow::Result<i32> {
            let state = caller.data();

            // Check capability
            if !check_read_capability(state, map_type) {
                return Ok(PROXY_RESULT_NOT_ALLOWED);
            }

            let serialized = match get_headers(state, map_type) {
                // B-408: 疑似ヘッダを先頭に足す
                Some(h) => serialize_map(&state.http_ctx, map_type, h),
                // F-134: 認識済みの読み取り専用型（gRPC 受信メタデータ/HTTP call
                // 応答）はデータ未到着なら NotFound、完全に未知の型のみ BadArgument。
                None if is_known_map_type(map_type) => return Ok(PROXY_RESULT_NOT_FOUND),
                None => return Ok(PROXY_RESULT_BAD_ARGUMENT),
            };
            let size = serialized.len();

            // Allocate memory in WASM
            let ptr = match crate::wasm::exports::allocate_guest_memory(&mut caller, size)? {
                Some(p) => p,
                None => return Ok(PROXY_RESULT_INTERNAL_FAILURE),
            };

            // Write serialized headers
            if !write_to_wasm(&mut caller, ptr, &serialized) {
                return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS);
            }

            // Write return values
            let memory = match caller.guest_memory() {
                Some(mem) => mem,
                _ => return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS),
            };

            let data = memory.data_mut(&mut caller);

            let ptr_offset = return_map_ptr as u32 as usize;
            let size_offset = return_map_size as u32 as usize;

            if ptr_offset + 4 > data.len() || size_offset + 4 > data.len() {
                return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS);
            }

            data[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr.to_le_bytes());
            data[size_offset..size_offset + 4].copy_from_slice(&(size as i32).to_le_bytes());

            Ok(PROXY_RESULT_OK)
        },
    )?;

    // proxy_set_header_map_pairs
    linker.func_wrap(
        "env",
        "proxy_set_header_map_pairs",
        |mut caller: Caller<'_, HostState>, map_type: i32, map_ptr: i32, map_size: i32| -> i32 {
            // F-134: 読み取り専用/未知の MapType は capability に関わらず BadArgument
            if !is_writable_map_type(map_type) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }
            // Check write capability
            {
                let state = caller.data();
                if !check_write_capability(state, map_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            // Read serialized headers from WASM memory
            let memory = match caller.guest_memory() {
                Some(mem) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data(&caller);
            let start = map_ptr as u32 as usize;
            let end = start + map_size as u32 as usize;

            if end > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            let headers = match deserialize_headers(&data[start..end]) {
                Some(h) => h,
                None => return PROXY_RESULT_PARSE_FAILURE,
            };
            if !headers.iter().all(|(k, v)| guest_field_ok(k, v)) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }
            // B-408: 読んだ組をそのまま戻す往復では疑似ヘッダも並ぶ。今と同じ値なら受け付けて保持しない
            if headers.iter().any(|(k, v)| {
                pseudo_write(&caller.data().http_ctx, map_type, k, v)
                    == Some(PROXY_RESULT_BAD_ARGUMENT)
            }) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }
            let mut headers = headers;
            if virtual_pseudo(&caller.data().http_ctx) {
                headers.retain(|(k, _)| !k.starts_with(b":"));
            }

            // Set headers
            let state = caller.data_mut();
            match map_type {
                HTTP_REQUEST_HEADERS => {
                    state.http_ctx.replace_request_headers(headers);
                    state.http_ctx.request_headers_modified = true;
                }
                HTTP_RESPONSE_HEADERS => {
                    state.http_ctx.response_headers = headers;
                    state.http_ctx.response_headers_modified = true;
                }
                _ => return PROXY_RESULT_BAD_ARGUMENT,
            }

            PROXY_RESULT_OK
        },
    )?;

    // proxy_get_header_map_value
    linker.func_wrap(
        "env",
        "proxy_get_header_map_value",
        // ゲストのアロケータを今のファイバーの上で呼ぶ同期のホスト関数（F-318。`exports::allocate_guest_memory`）
        |mut caller: Caller<'_, HostState>,
         map_type: i32,
         key_ptr: i32,
         key_size: i32,
         return_value_ptr: i32,
         return_value_size: i32|
         -> anyhow::Result<i32> {
            let state = caller.data();

            // Check capability
            if !check_read_capability(state, map_type) {
                return Ok(PROXY_RESULT_NOT_ALLOWED);
            }

            let key = match read_string(&mut caller, key_ptr, key_size) {
                Some(k) => k,
                None => return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS),
            };

            let state = caller.data();
            let headers = match get_headers(state, map_type) {
                Some(h) => h,
                // F-134: 認識済みの読み取り専用型はデータ未到着なら NotFound。
                None if is_known_map_type(map_type) => return Ok(PROXY_RESULT_NOT_FOUND),
                None => return Ok(PROXY_RESULT_BAD_ARGUMENT),
            };

            // B-408: 疑似ヘッダ（`:path` 等）は context の要求・応答から
            let value = match pseudo_value(&state.http_ctx, map_type, key.as_bytes()) {
                Some(v) => Some(v.into_owned()),
                // Find header value (case-insensitive)
                None => headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key.as_bytes()))
                    .map(|(_, v)| v.clone()),
            };

            let value = match value {
                Some(v) => v,
                None => return Ok(PROXY_RESULT_NOT_FOUND),
            };

            // Allocate memory and write value
            let ptr = match crate::wasm::exports::allocate_guest_memory(&mut caller, value.len())? {
                Some(p) => p,
                None => return Ok(PROXY_RESULT_INTERNAL_FAILURE),
            };

            if !write_to_wasm(&mut caller, ptr, &value) {
                return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS);
            }

            // Write return pointers
            let memory = match caller.guest_memory() {
                Some(mem) => mem,
                _ => return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS),
            };

            let data = memory.data_mut(&mut caller);

            let ptr_offset = return_value_ptr as u32 as usize;
            let size_offset = return_value_size as u32 as usize;

            if ptr_offset + 4 > data.len() || size_offset + 4 > data.len() {
                return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS);
            }

            data[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr.to_le_bytes());
            data[size_offset..size_offset + 4].copy_from_slice(&(value.len() as i32).to_le_bytes());

            Ok(PROXY_RESULT_OK)
        },
    )?;

    // proxy_add_header_map_value

    linker.func_wrap(
        "env",
        "proxy_add_header_map_value",
        |mut caller: Caller<'_, HostState>,
         map_type: i32,
         key_ptr: i32,
         key_size: i32,
         value_ptr: i32,
         value_size: i32|
         -> i32 {
            // F-134: 読み取り専用/未知の MapType は capability に関わらず BadArgument
            if !is_writable_map_type(map_type) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }
            // Check capability
            {
                let state = caller.data();
                if !check_write_capability(state, map_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            let key = match read_string(&mut caller, key_ptr, key_size) {
                Some(k) => k,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let value = match read_string(&mut caller, value_ptr, value_size) {
                Some(v) => v,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };
            if !guest_field_ok(key.as_bytes(), value.as_bytes()) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }
            // B-408: 疑似ヘッダは足せない（もうある。2 つ目の値は不正）
            if key.starts_with(':') && virtual_pseudo(&caller.data().http_ctx) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }

            let state = caller.data_mut();
            let headers = match get_headers_mut(state, map_type) {
                Some(h) => h,
                None => return PROXY_RESULT_BAD_ARGUMENT,
            };

            headers.push((key.into_bytes(), value.into_bytes()));

            // Mark as modified
            match map_type {
                HTTP_REQUEST_HEADERS | HTTP_REQUEST_TRAILERS => {
                    state.http_ctx.request_headers_modified = true;
                }
                HTTP_RESPONSE_HEADERS | HTTP_RESPONSE_TRAILERS => {
                    state.http_ctx.response_headers_modified = true;
                }
                _ => {}
            }

            PROXY_RESULT_OK
        },
    )?;

    // proxy_replace_header_map_value
    linker.func_wrap(
        "env",
        "proxy_replace_header_map_value",
        |mut caller: Caller<'_, HostState>,
         map_type: i32,
         key_ptr: i32,
         key_size: i32,
         value_ptr: i32,
         value_size: i32|
         -> i32 {
            // F-134: 読み取り専用/未知の MapType は capability に関わらず BadArgument（他の書き込みと同じ。B-408）
            if !is_writable_map_type(map_type) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }
            // Check capability
            {
                let state = caller.data();
                if !check_write_capability(state, map_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            let key = match read_string(&mut caller, key_ptr, key_size) {
                Some(k) => k,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let value = match read_string(&mut caller, value_ptr, value_size) {
                Some(v) => v,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };
            if !guest_field_ok(key.as_bytes(), value.as_bytes()) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }
            // B-408: 疑似ヘッダは今と同じ値に置くことだけを受け付ける（何も変えない）
            if let Some(status) = pseudo_write(
                &caller.data().http_ctx,
                map_type,
                key.as_bytes(),
                value.as_bytes(),
            ) {
                return status;
            }

            let state = caller.data_mut();
            let headers = match get_headers_mut(state, map_type) {
                Some(h) => h,
                None => return PROXY_RESULT_BAD_ARGUMENT,
            };

            // Remove existing and add new
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(key.as_bytes()));
            headers.push((key.into_bytes(), value.into_bytes()));

            // Mark as modified
            match map_type {
                HTTP_REQUEST_HEADERS | HTTP_REQUEST_TRAILERS => {
                    state.http_ctx.request_headers_modified = true;
                }
                HTTP_RESPONSE_HEADERS | HTTP_RESPONSE_TRAILERS => {
                    state.http_ctx.response_headers_modified = true;
                }
                _ => {}
            }

            PROXY_RESULT_OK
        },
    )?;

    // proxy_remove_header_map_value
    linker.func_wrap(
        "env",
        "proxy_remove_header_map_value",
        |mut caller: Caller<'_, HostState>, map_type: i32, key_ptr: i32, key_size: i32| -> i32 {
            // F-134: 読み取り専用/未知の MapType は capability に関わらず BadArgument
            if !is_writable_map_type(map_type) {
                return PROXY_RESULT_BAD_ARGUMENT;
            }
            // Check capability
            {
                let state = caller.data();
                if !check_write_capability(state, map_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            let key = match read_string(&mut caller, key_ptr, key_size) {
                Some(k) => k,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };
            // B-408: 見せている疑似ヘッダは消せない（それ以外の `:` で始まる名前は保持していないので何もしない）
            if key.starts_with(':') && virtual_pseudo(&caller.data().http_ctx) {
                return if pseudo_value(&caller.data().http_ctx, map_type, key.as_bytes()).is_some()
                {
                    PROXY_RESULT_BAD_ARGUMENT
                } else {
                    PROXY_RESULT_OK
                };
            }

            let state = caller.data_mut();
            let headers = match get_headers_mut(state, map_type) {
                Some(h) => h,
                None => return PROXY_RESULT_BAD_ARGUMENT,
            };

            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(key.as_bytes()));

            // Mark as modified
            match map_type {
                HTTP_REQUEST_HEADERS | HTTP_REQUEST_TRAILERS => {
                    state.http_ctx.request_headers_modified = true;
                }
                HTTP_RESPONSE_HEADERS | HTTP_RESPONSE_TRAILERS => {
                    state.http_ctx.response_headers_modified = true;
                }
                _ => {}
            }

            PROXY_RESULT_OK
        },
    )?;

    Ok(())
}

#[cfg(test)]
mod tests {
    /// B-318: ゲストの書くヘッダは CR・LF・NUL を含めない（擬似ヘッダの名前は許す）
    #[test]
    fn guest_field_ok_rejects_crlf() {
        assert!(super::guest_field_ok(b"x-a", b"v"));
        assert!(super::guest_field_ok(b":path", b"/a"));
        assert!(!super::guest_field_ok(b"x-a", b"v\r\nx: y"));
        assert!(!super::guest_field_ok(b"x-a", b"v\0"));
        assert!(!super::guest_field_ok(b"x\na", b"v"));
    }
}
