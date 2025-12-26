//! Logging Host Functions

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Add logging functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_log
    linker.func_wrap(
        "env",
        "proxy_log",
        |caller: Caller<'_, HostState>, level: i32, msg_ptr: i32, msg_size: i32| -> i32 {
            let state = caller.data();

            // Check capability
            if !state.http_ctx.capabilities.allow_logging {
                return PROXY_RESULT_OK; // Silently ignore
            }

            // Read message from WASM memory
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data(&caller);
            let start = msg_ptr as usize;
            let end = start + msg_size as usize;

            if end > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            let msg = match std::str::from_utf8(&data[start..end]) {
                Ok(s) => s,
                Err(_) => return PROXY_RESULT_PARSE_FAILURE,
            };

            let plugin_name = &state.http_ctx.plugin_name;

            match level {
                LOG_TRACE => log::trace!("[wasm:{}] {}", plugin_name, msg),
                LOG_DEBUG => log::debug!("[wasm:{}] {}", plugin_name, msg),
                LOG_INFO => log::info!("[wasm:{}] {}", plugin_name, msg),
                LOG_WARN => log::warn!("[wasm:{}] {}", plugin_name, msg),
                LOG_ERROR => log::error!("[wasm:{}] {}", plugin_name, msg),
                LOG_CRITICAL => log::error!("[wasm:{}] CRITICAL: {}", plugin_name, msg),
                _ => log::info!("[wasm:{}] {}", plugin_name, msg),
            }

            PROXY_RESULT_OK
        },
    )?;

    // proxy_get_current_time_nanoseconds
    linker.func_wrap(
        "env",
        "proxy_get_current_time_nanoseconds",
        |mut caller: Caller<'_, HostState>, return_time_ptr: i32| -> i32 {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as i64)
                .unwrap_or(0);

            // Write to WASM memory
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            let data = memory.data_mut(&mut caller);
            let ptr = return_time_ptr as usize;

            if ptr + 8 > data.len() {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            data[ptr..ptr + 8].copy_from_slice(&now.to_le_bytes());

            PROXY_RESULT_OK
        },
    )?;

    Ok(())
}
