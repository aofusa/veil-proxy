//! Buffer Host Functions (Body Access)

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Get buffer by type
fn get_buffer<'a>(state: &'a HostState, buffer_type: i32) -> Option<&'a Vec<u8>> {
    match buffer_type {
        HTTP_REQUEST_BODY => Some(&state.http_ctx.request_body),
        HTTP_RESPONSE_BODY => Some(&state.http_ctx.response_body),
        HTTP_CALL_RESPONSE_BODY => {
            if let Some(token) = state.http_ctx.current_http_call_token {
                state
                    .http_ctx
                    .http_call_responses
                    .get(&token)
                    .map(|r| &r.body)
            } else {
                None
            }
        }
        PLUGIN_CONFIGURATION => Some(&state.http_ctx.plugin_configuration),
        VM_CONFIGURATION => Some(&state.http_ctx.vm_configuration),
        _ => None,
    }
}

/// Check read capability for buffer type
fn check_read_capability(state: &HostState, buffer_type: i32) -> bool {
    match buffer_type {
        HTTP_REQUEST_BODY => state.http_ctx.capabilities.allow_request_body_read,
        HTTP_RESPONSE_BODY => state.http_ctx.capabilities.allow_response_body_read,
        HTTP_CALL_RESPONSE_BODY => state.http_ctx.capabilities.allow_http_calls,
        PLUGIN_CONFIGURATION | VM_CONFIGURATION => true, // Always allowed
        _ => false,
    }
}

/// Check write capability for buffer type
fn check_write_capability(state: &HostState, buffer_type: i32) -> bool {
    match buffer_type {
        HTTP_REQUEST_BODY => state.http_ctx.capabilities.allow_request_body_write,
        HTTP_RESPONSE_BODY => state.http_ctx.capabilities.allow_response_body_write,
        _ => false,
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

/// Add buffer functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_get_buffer_bytes
    linker.func_wrap(
        "env",
        "proxy_get_buffer_bytes",
        |mut caller: Caller<'_, HostState>,
         buffer_type: i32,
         start: i32,
         max_size: i32,
         return_buffer_ptr: i32,
         return_buffer_size: i32|
         -> i32 {
            let state = caller.data();

            // Check capability
            if !check_read_capability(state, buffer_type) {
                return PROXY_RESULT_NOT_ALLOWED;
            }

            let buffer = match get_buffer(state, buffer_type) {
                Some(b) => b.clone(),
                None => return PROXY_RESULT_BAD_ARGUMENT,
            };

            // Calculate range
            let start_idx = start as usize;
            let end_idx = std::cmp::min(start_idx + max_size as usize, buffer.len());

            if start_idx > buffer.len() {
                return PROXY_RESULT_BAD_ARGUMENT;
            }

            let slice = &buffer[start_idx..end_idx];
            let size = slice.len();

            if size == 0 {
                // Write null pointer and zero size
                let memory = match caller.get_export("memory") {
                    Some(wasmtime::Extern::Memory(mem)) => mem,
                    _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                };

                let data = memory.data_mut(&mut caller);
                let ptr_offset = return_buffer_ptr as usize;
                let size_offset = return_buffer_size as usize;

                if ptr_offset + 4 > data.len() || size_offset + 4 > data.len() {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }

                data[ptr_offset..ptr_offset + 4].copy_from_slice(&0i32.to_le_bytes());
                data[size_offset..size_offset + 4].copy_from_slice(&0i32.to_le_bytes());

                return PROXY_RESULT_OK;
            }

            // Allocate memory in WASM
            let ptr = match allocate_wasm_memory(&mut caller, size) {
                Some(p) => p,
                None => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            // Write buffer data
            if !write_to_wasm(&mut caller, ptr, slice) {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            // Write return values
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data_mut(&mut caller);
            let ptr_offset = return_buffer_ptr as usize;
            let size_offset = return_buffer_size as usize;

            if ptr_offset + 4 > data.len() || size_offset + 4 > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            data[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr.to_le_bytes());
            data[size_offset..size_offset + 4].copy_from_slice(&(size as i32).to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    // proxy_set_buffer_bytes
    linker.func_wrap(
        "env",
        "proxy_set_buffer_bytes",
        |mut caller: Caller<'_, HostState>,
         buffer_type: i32,
         start: i32,
         size: i32,
         value_ptr: i32,
         value_size: i32|
         -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !check_write_capability(state, buffer_type) {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            // Read value from WASM memory
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data(&caller);
            let start_idx = value_ptr as usize;
            let end_idx = start_idx + value_size as usize;

            if end_idx > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            let value = data[start_idx..end_idx].to_vec();

            // Get mutable buffer
            let state = caller.data_mut();
            let buffer = match buffer_type {
                HTTP_REQUEST_BODY => &mut state.http_ctx.request_body,
                HTTP_RESPONSE_BODY => &mut state.http_ctx.response_body,
                _ => return PROXY_RESULT_BAD_ARGUMENT,
            };

            let start_pos = start as usize;
            let end_pos = start_pos + size as usize;

            // Handle replacement
            if start_pos > buffer.len() {
                return PROXY_RESULT_BAD_ARGUMENT;
            }

            if end_pos > buffer.len() {
                // Extend buffer if needed
                buffer.resize(end_pos, 0);
            }

            // Replace range with new value
            let mut new_buffer = Vec::with_capacity(start_pos + value.len() + buffer.len() - end_pos);
            new_buffer.extend_from_slice(&buffer[..start_pos]);
            new_buffer.extend_from_slice(&value);
            if end_pos < buffer.len() {
                new_buffer.extend_from_slice(&buffer[end_pos..]);
            }

            *buffer = new_buffer;

            // Mark as modified
            match buffer_type {
                HTTP_REQUEST_BODY => {
                    state.http_ctx.request_body_modified = true;
                }
                HTTP_RESPONSE_BODY => {
                    state.http_ctx.response_body_modified = true;
                }
                _ => {}
            }

            PROXY_RESULT_OK
        },
    )?;

    Ok(())
}
