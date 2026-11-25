//! Shared Data Host Functions

use crate::wasm::exports::GuestMemory;
use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Helper to read bytes from WASM memory
fn read_bytes(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Option<Vec<u8>> {
    let memory = caller.guest_memory()?;
    let data = memory.data(caller);

    let start = ptr as u32 as usize;
    let end = start + len as u32 as usize;

    if end > data.len() {
        return None;
    }

    Some(data[start..end].to_vec())
}

/// Add shared data functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_get_shared_data
    // ゲストのアロケータを今のファイバーの上で呼ぶ同期のホスト関数（F-318。`exports::allocate_guest_memory`）
    linker.func_wrap(
        "env",
        "proxy_get_shared_data",
        |mut caller: Caller<'_, HostState>,
         key_ptr: i32,
         key_size: i32,
         return_value_ptr: i32,
         return_value_size: i32,
         return_cas: i32|
         -> anyhow::Result<i32> {
            // Check capability
            {
                let state = caller.data();
                if !state.http_ctx.capabilities.allow_shared_data {
                    return Ok(PROXY_RESULT_NOT_ALLOWED);
                }
            }

            // Read key
            let key = match read_bytes(&mut caller, key_ptr, key_size) {
                Some(k) => String::from_utf8_lossy(&k).to_string(),
                None => return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS),
            };

            // Get value from shared data（B-179: モジュールで共有する表。値は `Arc` の参照を増やすだけ）
            let (value, cas) = match caller.data().http_ctx.shared_data.get(&key) {
                Some(found) => found,
                None => return Ok(PROXY_RESULT_NOT_FOUND),
            };

            // Allocate memory and write value
            let ptr = match crate::wasm::exports::allocate_guest_memory(&mut caller, value.len())? {
                Some(p) => p,
                None => return Ok(PROXY_RESULT_INTERNAL_FAILURE),
            };

            // Write value
            let memory = match caller.guest_memory() {
                Some(mem) => mem,
                _ => return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS),
            };

            let data = memory.data_mut(&mut caller);
            let value_start = ptr as u32 as usize;
            if value_start + value.len() > data.len() {
                return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS);
            }
            data[value_start..value_start + value.len()].copy_from_slice(&value);

            // Write return pointers
            let ptr_offset = return_value_ptr as u32 as usize;
            let size_offset = return_value_size as u32 as usize;
            let cas_offset = return_cas as u32 as usize;

            if ptr_offset + 4 > data.len()
                || size_offset + 4 > data.len()
                || cas_offset + 4 > data.len()
            {
                return Ok(PROXY_RESULT_INVALID_MEMORY_ACCESS);
            }

            data[ptr_offset..ptr_offset + 4].copy_from_slice(&ptr.to_le_bytes());
            data[size_offset..size_offset + 4].copy_from_slice(&(value.len() as i32).to_le_bytes());
            data[cas_offset..cas_offset + 4].copy_from_slice(&cas.to_le_bytes());

            Ok(PROXY_RESULT_OK)
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
                if value_size as u32 as usize > state.http_ctx.capabilities.max_shared_data_size {
                    return PROXY_RESULT_BAD_ARGUMENT;
                }
            }

            // Read key
            let key = match read_bytes(&mut caller, key_ptr, key_size) {
                Some(k) => String::from_utf8_lossy(&k).to_string(),
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Read value
            let value = match read_bytes(&mut caller, value_ptr, value_size) {
                Some(v) => v,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Set value with CAS check（B-179: モジュールで共有する表。CAS も表で払い出す）
            match caller
                .data()
                .http_ctx
                .shared_data
                .set(&key, &value, cas as u32)
            {
                crate::wasm::shared_data::SetResult::Ok => {}
                crate::wasm::shared_data::SetResult::CasMismatch => {
                    return PROXY_RESULT_CAS_MISMATCH
                }
                crate::wasm::shared_data::SetResult::TooManyKeys => {
                    crate::logging::log_throttled!(
                        warn,
                        "[wasm] shared data key limit (shared_data_max_keys) reached; set rejected"
                    );
                    return PROXY_RESULT_INTERNAL_FAILURE;
                }
            }

            PROXY_RESULT_OK
        },
    )?;

    Ok(())
}
