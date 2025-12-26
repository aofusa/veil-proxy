//! Properties Host Functions

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Get property value by path
fn get_property_value(state: &HostState, path: &str) -> Option<Vec<u8>> {
    // Check capability
    if !state.http_ctx.capabilities.is_property_allowed(path) {
        return None;
    }

    match path {
        // Request properties
        "request.path" => Some(state.http_ctx.request_path.as_bytes().to_vec()),
        "request.url_path" => Some(state.http_ctx.request_path.as_bytes().to_vec()),
        "request.host" => state
            .http_ctx
            .request_headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.as_bytes().to_vec()),
        "request.method" => Some(state.http_ctx.request_method.as_bytes().to_vec()),
        "request.scheme" => Some(b"https".to_vec()),
        "request.protocol" => Some(b"HTTP/1.1".to_vec()),
        "request.query" => Some(state.http_ctx.request_query.as_bytes().to_vec()),
        "request.time" => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs().to_string())
                .unwrap_or_default();
            Some(now.into_bytes())
        }
        "request.total_size" => {
            let size = state.http_ctx.request_body.len();
            Some(size.to_string().into_bytes())
        }

        // Response properties
        "response.code" => Some(state.http_ctx.response_status.to_string().into_bytes()),
        "response.code_details" => Some(b"via veil-proxy".to_vec()),

        // Connection properties
        "source.address" => Some(state.http_ctx.client_ip.as_bytes().to_vec()),
        "destination.address" => Some(b"0.0.0.0:0".to_vec()),

        // Plugin properties
        "plugin_name" => Some(state.http_ctx.plugin_name.as_bytes().to_vec()),
        "plugin_root_id" => Some(state.http_ctx.root_context_id.to_string().into_bytes()),

        _ => None,
    }
}

/// Helper to allocate memory in WASM
fn allocate_wasm_memory(caller: &mut Caller<'_, HostState>, size: usize) -> Option<i32> {
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

/// Helper to read string from WASM memory
fn read_bytes(caller: &Caller<'_, HostState>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory")?;
    let memory = memory.into_memory()?;
    let data = memory.data(caller);

    let start = ptr as usize;
    let end = start + len as usize;

    if end > data.len() {
        return None;
    }

    Some(data[start..end].to_vec())
}

/// Parse property path from bytes
fn parse_path(data: &[u8]) -> Option<String> {
    // Path is serialized as: [num_parts:4][part1_len:4][part1]...
    if data.len() < 4 {
        return None;
    }

    let num_parts = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut parts = Vec::with_capacity(num_parts);
    let mut pos = 4;

    for _ in 0..num_parts {
        if pos + 4 > data.len() {
            return None;
        }
        let part_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + part_len > data.len() {
            return None;
        }
        let part = String::from_utf8_lossy(&data[pos..pos + part_len]).to_string();
        pos += part_len;

        parts.push(part);
    }

    Some(parts.join("."))
}

/// Add properties functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_get_property
    linker.func_wrap(
        "env",
        "proxy_get_property",
        |mut caller: Caller<'_, HostState>,
         path_ptr: i32,
         path_size: i32,
         return_value_ptr: i32,
         return_value_size: i32|
         -> i32 {
            // Read path from WASM memory
            let path_data = match read_bytes(&caller, path_ptr, path_size) {
                Some(d) => d,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let path = match parse_path(&path_data) {
                Some(p) => p,
                None => return PROXY_RESULT_PARSE_FAILURE,
            };

            let state = caller.data();
            let value = match get_property_value(state, &path) {
                Some(v) => v,
                None => return PROXY_RESULT_NOT_FOUND,
            };

            // Allocate memory and write value
            let ptr = match allocate_wasm_memory(&mut caller, value.len()) {
                Some(p) => p,
                None => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            if !write_to_wasm(&mut caller, ptr, &value) {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            // Write return values
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
            data[size_offset..size_offset + 4].copy_from_slice(&(value.len() as i32).to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    // proxy_set_property
    linker.func_wrap(
        "env",
        "proxy_set_property",
        |_caller: Caller<'_, HostState>,
         _path_ptr: i32,
         _path_size: i32,
         _value_ptr: i32,
         _value_size: i32|
         -> i32 {
            // Property setting is not supported in this implementation
            PROXY_RESULT_UNIMPLEMENTED
        },
    )?;

    Ok(())
}
