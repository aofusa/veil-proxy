//! Stream Control Host Functions

use wasmtime::{Caller, Linker};

// B-19: マップ直列化は SDK 互換の共通モジュールを使用
use super::abi::deserialize_headers;
use crate::wasm::constants::*;
use crate::wasm::context::HostState;
use crate::wasm::types::LocalResponse;

/// Add stream control functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_continue_stream
    linker.func_wrap(
        "env",
        "proxy_continue_stream",
        |mut caller: Caller<'_, HostState>, _stream_type: i32| -> i32 {
            // B-320: Pause の後で再開したかを記録する（`FilterEngine::settle` が見る）
            caller.data_mut().http_ctx.stream_resumed = true;
            PROXY_RESULT_OK
        },
    )?;

    // proxy_close_stream
    // F-133: network filter（L4）が接続クローズを要求する経路。Proxy-Wasm ABI の
    // Action（Continue/Pause）には Close が無いため、host 関数呼び出しでフラグを立て、
    // FilterEngine 側で `HttpContext::close_requested` を確認してクローズを決定する。
    linker.func_wrap(
        "env",
        "proxy_close_stream",
        |mut caller: Caller<'_, HostState>, _stream_type: i32| -> i32 {
            let state = caller.data_mut();
            state.http_ctx.close_requested = true;
            PROXY_RESULT_OK
        },
    )?;

    // proxy_send_local_response
    linker.func_wrap(
        "env",
        "proxy_send_local_response",
        |mut caller: Caller<'_, HostState>,
         status_code: i32,
         _status_msg_ptr: i32,
         _status_msg_size: i32,
         body_ptr: i32,
         body_size: i32,
         headers_ptr: i32,
         headers_size: i32,
         _grpc_status: i32|
         -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_send_local_response {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            // Read body from WASM memory
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data(&caller);

            // Read body
            let body = if body_size > 0 {
                let start = body_ptr as u32 as usize;
                let end = start + body_size as u32 as usize;
                if end > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
                data[start..end].to_vec()
            } else {
                Vec::new()
            };

            // Read headers
            let headers = if headers_size > 0 {
                let start = headers_ptr as u32 as usize;
                let end = start + headers_size as u32 as usize;
                if end > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
                deserialize_headers(&data[start..end]).unwrap_or_default()
            } else {
                Vec::new()
            };
            // モジュールの返すヘッダはそのままクライアントへ書かれる。名前は小文字にし（HTTP/2・HTTP/3 は大文字を
            // PROTOCOL_ERROR にする）、CR・LF・NUL を含むもの・空の名前は捨てる（HTTP/1.1 のヘッダの注入を防ぐ）
            let headers = sanitize_local_response_headers(headers);
            // 範囲外のステータスは 500（u16 への切り詰めで 0 や別の値にしない）
            let status_code = if (100..=599).contains(&status_code) {
                status_code as u16
            } else {
                500
            };

            // Set local response
            let state = caller.data_mut();
            state.http_ctx.local_response = Some(LocalResponse {
                status_code,
                headers,
                body,
            });

            PROXY_RESULT_OK
        },
    )?;

    // proxy_set_effective_context
    linker.func_wrap(
        "env",
        "proxy_set_effective_context",
        |mut caller: Caller<'_, HostState>, context_id: i32| -> i32 {
            let state = caller.data_mut();
            state.http_ctx.context_id = context_id;
            PROXY_RESULT_OK
        },
    )?;

    // proxy_done
    linker.func_wrap(
        "env",
        "proxy_done",
        |_caller: Caller<'_, HostState>| -> i32 { PROXY_RESULT_OK },
    )?;

    // proxy_set_tick_period_milliseconds
    // Sets the timer period for periodic on_tick callbacks
    // Currently stores the value but tick callbacks are not yet implemented
    linker.func_wrap(
        "env",
        "proxy_set_tick_period_milliseconds",
        |mut caller: Caller<'_, HostState>, period_ms: i32| -> i32 {
            let state = caller.data_mut();
            state.http_ctx.tick_period_ms = period_ms as u32;
            if period_ms > 0 {
                ftlog::debug!("WASM module requested tick period: {}ms", period_ms);
            }
            PROXY_RESULT_OK
        },
    )?;

    // proxy_get_status
    // Returns the status code and message from an HTTP call or gRPC response
    // Typically called in proxy_on_http_call_response or proxy_on_grpc_close callbacks
    linker.func_wrap(
        "env",
        "proxy_get_status",
        |mut caller: Caller<'_, HostState>,
         return_status_code_ptr: i32,
         return_status_msg_ptr: i32,
         return_status_msg_size_ptr: i32|
         -> i32 {
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Get the current HTTP call response status if available
            let status_code = {
                let state = caller.data();
                if let Some(token) = state.http_ctx.current_http_call_token {
                    state
                        .http_ctx
                        .http_call_responses
                        .get(&token)
                        .map(|r| r.status_code as u32)
                        .unwrap_or(0)
                } else {
                    state.http_ctx.response_status as u32
                }
            };

            // Write status code
            let data = memory.data_mut(&mut caller);
            let ptr = return_status_code_ptr as u32 as usize;
            if ptr + 4 > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }
            data[ptr..ptr + 4].copy_from_slice(&status_code.to_le_bytes());

            // For message, write empty string (no message stored currently)
            let _msg_ptr = return_status_msg_ptr as u32 as usize;
            let msg_size_ptr = return_status_msg_size_ptr as u32 as usize;
            if msg_size_ptr + 4 > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }
            // Write size = 0 (no message)
            data[msg_size_ptr..msg_size_ptr + 4].copy_from_slice(&0u32.to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    // Note: proxy_get_current_time_nanoseconds is defined in logging.rs

    Ok(())
}

/// `proxy_send_local_response` のヘッダを、どのプロトコルでもそのまま書ける形にする
fn sanitize_local_response_headers(headers: Vec<(Vec<u8>, Vec<u8>)>) -> Vec<(Vec<u8>, Vec<u8>)> {
    let bad = |b: &u8| matches!(*b, b'\r' | b'\n' | 0);
    headers
        .into_iter()
        .filter(|(n, v)| !n.is_empty() && !n.iter().any(bad) && !v.iter().any(bad))
        .map(|(mut n, v)| {
            n.make_ascii_lowercase();
            (n, v)
        })
        .collect()
}

#[cfg(test)]
mod local_response_tests {
    use super::sanitize_local_response_headers;

    #[test]
    fn local_response_headers_are_lowercased_and_injection_free() {
        let out = sanitize_local_response_headers(vec![
            (b"X-Blocked".to_vec(), b"1".to_vec()),
            (b"x-evil".to_vec(), b"a\r\nset-cookie: x".to_vec()),
            (b"bad\nname".to_vec(), b"v".to_vec()),
            (Vec::new(), b"v".to_vec()),
        ]);
        assert_eq!(out, vec![(b"x-blocked".to_vec(), b"1".to_vec())]);
    }
}
