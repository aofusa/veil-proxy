//! Header Map Host Functions

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Serialize headers to Proxy-Wasm format
/// Format: [num_pairs:4][key1_len:4][key1][val1_len:4][val1]...
fn serialize_headers(headers: &[(String, String)]) -> Vec<u8> {
    let mut buf = Vec::new();

    // Number of pairs
    buf.extend_from_slice(&(headers.len() as u32).to_le_bytes());

    for (key, value) in headers {
        buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
        buf.extend_from_slice(key.as_bytes());
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
        buf.extend_from_slice(value.as_bytes());
    }

    buf
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

/// Get headers by map type
fn get_headers<'a>(state: &'a HostState, map_type: i32) -> Option<&'a Vec<(String, String)>> {
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
fn get_headers_mut<'a>(
    state: &'a mut HostState,
    map_type: i32,
) -> Option<&'a mut Vec<(String, String)>> {
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

/// Helper to allocate memory in WASM
fn allocate_wasm_memory(caller: &mut Caller<'_, HostState>, size: usize) -> Option<i32> {
    // Call proxy_on_memory_allocate if exported
    let func = caller.get_export("proxy_on_memory_allocate")?;
    let func = func.into_func()?;
    let typed = func.typed::<i32, i32>(caller).ok()?;
    typed.call(caller, size as i32).ok()
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
    linker.func_wrap(
        "env",
        "proxy_get_header_map_pairs",
        |mut caller: Caller<'_, HostState>,
         map_type: i32,
         return_map_ptr: i32,
         return_map_size: i32|
         -> i32 {
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
            let ptr = match allocate_wasm_memory(&mut caller, size) {
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
        },
    )?;

    // proxy_set_header_map_pairs
    linker.func_wrap(
        "env",
        "proxy_set_header_map_pairs",
        |mut caller: Caller<'_, HostState>,
         map_type: i32,
         map_ptr: i32,
         map_size: i32|
         -> i32 {
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
    linker.func_wrap(
        "env",
        "proxy_get_header_map_value",
        |mut caller: Caller<'_, HostState>,
         map_type: i32,
         key_ptr: i32,
         key_size: i32,
         return_value_ptr: i32,
         return_value_size: i32|
         -> i32 {
            let state = caller.data();

            // Check capability
            if !check_read_capability(state, map_type) {
                return PROXY_RESULT_NOT_ALLOWED;
            }

            let key = match read_string(&caller, key_ptr, key_size) {
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
                .find(|(k, _)| k.eq_ignore_ascii_case(&key))
                .map(|(_, v)| v.clone());

            let value = match value {
                Some(v) => v,
                None => return PROXY_RESULT_NOT_FOUND,
            };

            // Allocate memory and write value
            let ptr = match allocate_wasm_memory(&mut caller, value.len()) {
                Some(p) => p,
                None => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            if !write_to_wasm(&mut caller, ptr, value.as_bytes()) {
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

            let key = match read_string(&caller, key_ptr, key_size) {
                Some(k) => k,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let value = match read_string(&caller, value_ptr, value_size) {
                Some(v) => v,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let state = caller.data_mut();
            let headers = match get_headers_mut(state, map_type) {
                Some(h) => h,
                None => return PROXY_RESULT_BAD_ARGUMENT,
            };

            headers.push((key, value));

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

            let key = match read_string(&caller, key_ptr, key_size) {
                Some(k) => k,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let value = match read_string(&caller, value_ptr, value_size) {
                Some(v) => v,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let state = caller.data_mut();
            let headers = match get_headers_mut(state, map_type) {
                Some(h) => h,
                None => return PROXY_RESULT_BAD_ARGUMENT,
            };

            // Remove existing and add new
            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));
            headers.push((key, value));

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
        |mut caller: Caller<'_, HostState>,
         map_type: i32,
         key_ptr: i32,
         key_size: i32|
         -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !check_write_capability(state, map_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            let key = match read_string(&caller, key_ptr, key_size) {
                Some(k) => k,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let state = caller.data_mut();
            let headers = match get_headers_mut(state, map_type) {
                Some(h) => h,
                None => return PROXY_RESULT_BAD_ARGUMENT,
            };

            headers.retain(|(k, _)| !k.eq_ignore_ascii_case(&key));

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
