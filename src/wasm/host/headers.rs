//! Header Map Host Functions

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

// B-19: マップ直列化は SDK 互換のワイヤ形式を実装する共通モジュールへ集約
use super::abi::{deserialize_headers, serialize_headers};

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
        _ => None,
    }
}

/// Get mutable headers by map type
fn get_headers_mut(state: &mut HostState, map_type: i32) -> Option<&mut Vec<(Vec<u8>, Vec<u8>)>> {
    match map_type {
        HTTP_REQUEST_HEADERS => Some(&mut state.http_ctx.request_headers),
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

/// Helper to read string from WASM memory
fn read_string(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Option<String> {
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

/// Helper to allocate memory in WASM
///
/// B-20: エンジンは async store（`async_support(true)` + fuel yield）で動作するため、
/// wasm への再入呼び出しは `call_async` でなければならない（同期 `call` は
/// "must use call_async with async stores" で panic する）。
async fn allocate_wasm_memory(caller: &mut Caller<'_, HostState>, size: usize) -> Option<i32> {
    // Call proxy_on_memory_allocate if exported
    let func = caller.get_export("proxy_on_memory_allocate")?;
    let func = func.into_func()?;
    let typed = func.typed::<i32, i32>(&mut *caller).ok()?;
    typed.call_async(&mut *caller, size as i32).await.ok()
}

/// Helper to write to WASM memory
fn write_to_wasm(caller: &mut Caller<'_, HostState>, ptr: i32, data: &[u8]) -> bool {
    let memory = match caller.get_export("memory") {
        Some(wasmtime::Extern::Memory(mem)) => mem,
        _ => return false,
    };

    let mem_data = memory.data_mut(caller);
    let start = ptr as usize;
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
    // B-20: wasm 側アロケータ（proxy_on_memory_allocate）へ再入するため async ホスト関数
    linker.func_wrap_async(
        "env",
        "proxy_get_header_map_pairs",
        |mut caller: Caller<'_, HostState>,
         (map_type, return_map_ptr, return_map_size): (i32, i32, i32)| {
            Box::new(async move {
                let state = caller.data();

                // Check capability
                if !check_read_capability(state, map_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }

                let headers = match get_headers(state, map_type) {
                    Some(h) => h.clone(),
                    None => return PROXY_RESULT_BAD_ARGUMENT,
                };

                let serialized = serialize_headers(&headers);
                let size = serialized.len();

                // Allocate memory in WASM
                let ptr = match allocate_wasm_memory(&mut caller, size).await {
                    Some(p) => p,
                    None => return PROXY_RESULT_INTERNAL_FAILURE,
                };

                // Write serialized headers
                if !write_to_wasm(&mut caller, ptr, &serialized) {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }

                // Write return values
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                };

                let data = memory.data_mut(&mut caller);

                let ptr_offset = return_map_ptr as usize;
                let size_offset = return_map_size as usize;

                if ptr_offset + 4 > data.len() || size_offset + 4 > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }

                data[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr.to_le_bytes());
                data[size_offset..size_offset + 4].copy_from_slice(&(size as i32).to_le_bytes());

                PROXY_RESULT_OK
            })
        },
    )?;

    // proxy_set_header_map_pairs
    linker.func_wrap(
        "env",
        "proxy_set_header_map_pairs",
        |mut caller: Caller<'_, HostState>, map_type: i32, map_ptr: i32, map_size: i32| -> i32 {
            // Check write capability
            {
                let state = caller.data();
                if !check_write_capability(state, map_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            // Read serialized headers from WASM memory
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data(&caller);
            let start = map_ptr as usize;
            let end = start + map_size as usize;

            if end > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            let headers = match deserialize_headers(&data[start..end]) {
                Some(h) => h,
                None => return PROXY_RESULT_PARSE_FAILURE,
            };

            // Set headers
            let state = caller.data_mut();
            match map_type {
                HTTP_REQUEST_HEADERS => {
                    state.http_ctx.request_headers = headers;
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
    linker.func_wrap_async(
        "env",
        "proxy_get_header_map_value",
        // B-20: wasm 側アロケータへ再入するため async ホスト関数
        |mut caller: Caller<'_, HostState>,
         (map_type, key_ptr, key_size, return_value_ptr, return_value_size): (
            i32,
            i32,
            i32,
            i32,
            i32,
        )| {
            Box::new(async move {
                let state = caller.data();

                // Check capability
                if !check_read_capability(state, map_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }

                let key = match read_string(&mut caller, key_ptr, key_size) {
                    Some(k) => k,
                    None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                };

                let headers = match get_headers(caller.data(), map_type) {
                    Some(h) => h,
                    None => return PROXY_RESULT_BAD_ARGUMENT,
                };

                // Find header value (case-insensitive)
                let value = headers
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(key.as_bytes()))
                    .map(|(_, v)| v.clone());

                let value = match value {
                    Some(v) => v,
                    None => return PROXY_RESULT_NOT_FOUND,
                };

                // Allocate memory and write value
                let ptr = match allocate_wasm_memory(&mut caller, value.len()).await {
                    Some(p) => p,
                    None => return PROXY_RESULT_INTERNAL_FAILURE,
                };

                if !write_to_wasm(&mut caller, ptr, &value) {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }

                // Write return pointers
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                };

                let data = memory.data_mut(&mut caller);

                let ptr_offset = return_value_ptr as usize;
                let size_offset = return_value_size as usize;

                if ptr_offset + 4 > data.len() || size_offset + 4 > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }

                data[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr.to_le_bytes());
                data[size_offset..size_offset + 4]
                    .copy_from_slice(&(value.len() as i32).to_le_bytes());

                PROXY_RESULT_OK
            })
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
