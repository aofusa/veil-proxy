//! Shared Data Host Functions

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Helper to read bytes from WASM memory
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

/// Helper to allocate memory in WASM
fn allocate_wasm_memory(caller: &mut Caller<'_, HostState>, size: usize) -> Option<i32> {
    let func = caller.get_export("proxy_on_memory_allocate")?;
    let func = func.into_func()?;
    let typed = func.typed::<i32, i32>(caller).ok()?;
    typed.call(caller, size as i32).ok()
}

/// Add shared data functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_get_shared_data
    linker.func_wrap(
        "env",
        "proxy_get_shared_data",
        |mut caller: Caller<'_, HostState>,
         key_ptr: i32,
         key_size: i32,
         return_value_ptr: i32,
         return_value_size: i32,
         return_cas: i32|
         -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_shared_data {
                    return PROXY_RESULT_NOT_ALLOWED;
                }
            }

            // Read key
            let key = match read_bytes(&caller, key_ptr, key_size) {
                Some(k) => String::from_utf8_lossy(&k).to_string(),
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Get value from shared data
            let state = caller.data();
            let shared_data = state.http_ctx.shared_data.read().unwrap();
            let (value, cas) = match shared_data.get(&key) {
                Some((v, c)) => (v.clone(), *c),
                None => return PROXY_RESULT_NOT_FOUND,
            };
            drop(shared_data);

            // Allocate memory and write value
            let ptr = match allocate_wasm_memory(&mut caller, value.len()) {
                Some(p) => p,
                None => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            // Write value
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data_mut(&mut caller);
            let value_start = ptr as usize;
            if value_start + value.len() > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }
            data[value_start..value_start + value.len()].copy_from_slice(&value);

            // Write return pointers
            let ptr_offset = return_value_ptr as usize;
            let size_offset = return_value_size as usize;
            let cas_offset = return_cas as usize;

            if ptr_offset + 4 > data.len()
                || size_offset + 4 > data.len()
                || cas_offset + 4 > data.len()
            {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            data[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr.to_le_bytes());
            data[size_offset..size_offset + 4].copy_from_slice(&(value.len() as i32).to_le_bytes());
            data[cas_offset..cas_offset + 4].copy_from_slice(&cas.to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    // proxy_set_shared_data
    linker.func_wrap(
        "env",
        "proxy_set_shared_data",
        |mut caller: Caller<'_, HostState>,
         key_ptr: i32,
         key_size: i32,
         value_ptr: i32,
         value_size: i32,
         cas: i32|
         -> i32 {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_shared_data {
                    return PROXY_RESULT_NOT_ALLOWED;
                }

                // Check size limit
                if value_size as usize > state.http_ctx.capabilities.max_shared_data_size {
                    return PROXY_RESULT_BAD_ARGUMENT;
                }
            }

            // Read key
            let key = match read_bytes(&caller, key_ptr, key_size) {
                Some(k) => String::from_utf8_lossy(&k).to_string(),
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Read value
            let value = match read_bytes(&caller, value_ptr, value_size) {
                Some(v) => v,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Set value with CAS check
            let state = caller.data_mut();
            let mut shared_data = state.http_ctx.shared_data.write().unwrap();

            if cas != 0 {
                // CAS check
                if let Some((_, current_cas)) = shared_data.get(&key) {
                    if *current_cas != cas as u32 {
                        return PROXY_RESULT_CAS_MISMATCH;
                    }
                }
            }

            // Increment CAS
            state.http_ctx.shared_data_cas = state.http_ctx.shared_data_cas.wrapping_add(1);
            let new_cas = state.http_ctx.shared_data_cas;

            shared_data.insert(key, (value, new_cas));

            PROXY_RESULT_OK
        },
    )?;

    Ok(())
}
