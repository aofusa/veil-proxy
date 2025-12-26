//! HTTP Call Host Functions

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;
use crate::wasm::types::PendingHttpCall;

/// Helper to read string from WASM memory
fn read_string(caller: &Caller<'_, HostState>, ptr: i32, len: i32) -> Option<String> {
    let memory = caller.get_export("memory")?;
    let memory = memory.into_memory()?;
    let data = memory.data(caller);

    let start = ptr as usize;
    let end = start + len as usize;

    if end > data.len() {
        return None;
    }

    String::from_utf8(data[start..end].to_vec()).ok()
}

/// Add HTTP call functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_http_call
    linker.func_wrap(
        "env",
        "proxy_http_call",
        |mut caller: Caller<'_, HostState>,
         upstream_ptr: i32,
         upstream_size: i32,
         headers_ptr: i32,
         headers_size: i32,
         body_ptr: i32,
         body_size: i32,
         trailers_ptr: i32,
         trailers_size: i32,
         timeout_ms: i32,
         return_token: i32|
         -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_http_calls {
                    log::warn!(
                        "[wasm:{}] HTTP call denied: allow_http_calls=false",
                        state.http_ctx.plugin_name
                    );
                    return PROXY_RESULT_NOT_ALLOWED;
                }

                // Check max calls
                if state.http_ctx.pending_http_calls.len()
                    >= state.http_ctx.capabilities.max_http_calls
                {
                    log::warn!(
                        "[wasm:{}] HTTP call denied: max_http_calls exceeded",
                        state.http_ctx.plugin_name
                    );
                    return PROXY_RESULT_INTERNAL_FAILURE;
                }
            }

            // Read upstream name
            let upstream = match read_string(&caller, upstream_ptr, upstream_size) {
                Some(u) => u,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Check upstream whitelist
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.is_upstream_allowed(&upstream) {
                    log::warn!(
                        "[wasm:{}] HTTP call to '{}' denied: not in allowed_upstreams",
                        state.http_ctx.plugin_name,
                        upstream
                    );
                    return PROXY_RESULT_BAD_ARGUMENT;
                }
            }

            // Read headers
            let _headers = if headers_size > 0 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                };
                let data = memory.data(&caller);
                let start = headers_ptr as usize;
                let end = start + headers_size as usize;
                if end > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
                data[start..end].to_vec()
            } else {
                Vec::new()
            };

            // Read body
            let _body = if body_size > 0 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                };
                let data = memory.data(&caller);
                let start = body_ptr as usize;
                let end = start + body_size as usize;
                if end > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
                data[start..end].to_vec()
            } else {
                Vec::new()
            };

            // Read trailers
            let _trailers = if trailers_size > 0 {
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                };
                let data = memory.data(&caller);
                let start = trailers_ptr as usize;
                let end = start + trailers_size as usize;
                if end > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
                data[start..end].to_vec()
            } else {
                Vec::new()
            };

            // Allocate token
            let state = caller.data_mut();
            let token = state.http_ctx.allocate_http_call_token();

            // Store pending call
            state.http_ctx.pending_http_calls.insert(
                token,
                PendingHttpCall {
                    token,
                    upstream: upstream.clone(),
                    timeout_ms: timeout_ms as u32,
                },
            );

            log::debug!(
                "[wasm:{}] HTTP call dispatched to '{}' with token {}",
                state.http_ctx.plugin_name,
                upstream,
                token
            );

            // Write token to return pointer
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data_mut(&mut caller);
            let ptr = return_token as usize;

            if ptr + 4 > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            data[ptr..ptr + 4].copy_from_slice(&token.to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    Ok(())
}
