//! Stream Control Host Functions

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;
use crate::wasm::types::LocalResponse;

/// Add stream control functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_continue_stream
    linker.func_wrap(
        "env",
        "proxy_continue_stream",
        |_caller: Caller<'_, HostState>, _stream_type: i32| -> i32 {
            // This is typically handled by the filter engine
            // by checking the return value of callbacks
            PROXY_RESULT_OK
        },
    )?;

    // proxy_close_stream
    linker.func_wrap(
        "env",
        "proxy_close_stream",
        |_caller: Caller<'_, HostState>, _stream_type: i32| -> i32 {
            // Stream closing is handled at a higher level
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
                let start = body_ptr as usize;
                let end = start + body_size as usize;
                if end > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
                data[start..end].to_vec()
            } else {
                Vec::new()
            };

            // Read headers
            let headers = if headers_size > 0 {
                let start = headers_ptr as usize;
                let end = start + headers_size as usize;
                if end > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
                deserialize_headers(&data[start..end]).unwrap_or_default()
            } else {
                Vec::new()
            };

            // Set local response
            let state = caller.data_mut();
            state.http_ctx.local_response = Some(LocalResponse {
                status_code: status_code as u16,
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
    linker.func_wrap("env", "proxy_done", |_caller: Caller<'_, HostState>| -> i32 {
        PROXY_RESULT_OK
    })?;

    Ok(())
}

/// Deserialize headers from Proxy-Wasm format
fn deserialize_headers(data: &[u8]) -> Option<Vec<(String, String)>> {
    if data.len() < 4 {
        return None;
    }

    let num_pairs = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut headers = Vec::with_capacity(num_pairs);
    let mut pos = 4;

    for _ in 0..num_pairs {
        if pos + 4 > data.len() {
            return None;
        }
        let key_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + key_len > data.len() {
            return None;
        }
        let key = String::from_utf8_lossy(&data[pos..pos + key_len]).to_string();
        pos += key_len;

        if pos + 4 > data.len() {
            return None;
        }
        let val_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + val_len > data.len() {
            return None;
        }
        let value = String::from_utf8_lossy(&data[pos..pos + val_len]).to_string();
        pos += val_len;

        headers.push((key, value));
    }

    Some(headers)
}
