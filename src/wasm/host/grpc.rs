//! gRPC Host Functions
//!
//! Implements Proxy-Wasm gRPC functions.
//! When `grpc` feature is enabled, provides actual gRPC call support.
//! Otherwise returns UNIMPLEMENTED.

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Helper to read memory from WASM
#[cfg(feature = "grpc")]
fn read_wasm_memory(caller: &mut Caller<'_, HostState>, ptr: i32, size: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory")?;
    let memory = memory.into_memory()?;
    let data = memory.data(caller);
    
    let start = ptr as usize;
    let end = start + size as usize;
    
    if end > data.len() {
        return None;
    }
    
    Some(data[start..end].to_vec())
}

/// Add gRPC functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_grpc_call
    // Initiates a gRPC call
    linker.func_wrap(
        "env",
        "proxy_grpc_call",
        |mut caller: Caller<'_, HostState>,
         upstream_ptr: i32,
         upstream_size: i32,
         service_ptr: i32,
         service_size: i32,
         method_ptr: i32,
         method_size: i32,
         _initial_metadata_ptr: i32,
         _initial_metadata_size: i32,
         message_ptr: i32,
         message_size: i32,
         timeout_ms: i32,
         return_call_id_ptr: i32|
         -> i32 {
            #[cfg(feature = "grpc")]
            {
                proxy_grpc_call_impl(
                    &mut caller,
                    upstream_ptr, upstream_size,
                    service_ptr, service_size,
                    method_ptr, method_size,
                    message_ptr, message_size,
                    timeout_ms,
                    return_call_id_ptr,
                )
            }
            #[cfg(not(feature = "grpc"))]
            {
                let _ = (upstream_ptr, upstream_size, service_ptr, service_size,
                         method_ptr, method_size, message_ptr, message_size,
                         timeout_ms, return_call_id_ptr);
                ftlog::debug!("WASM: proxy_grpc_call called (grpc feature not enabled)");
                PROXY_RESULT_UNIMPLEMENTED
            }
        },
    )?;

    // proxy_grpc_stream
    // Opens a gRPC stream
    linker.func_wrap(
        "env",
        "proxy_grpc_stream",
        |_caller: Caller<'_, HostState>,
         _upstream_ptr: i32,
         _upstream_size: i32,
         _service_ptr: i32,
         _service_size: i32,
         _method_ptr: i32,
         _method_size: i32,
         _initial_metadata_ptr: i32,
         _initial_metadata_size: i32,
         _return_stream_id_ptr: i32|
         -> i32 {
            ftlog::debug!("WASM: proxy_grpc_stream called (streaming not yet supported)");
            PROXY_RESULT_UNIMPLEMENTED
        },
    )?;

    // proxy_grpc_cancel
    // Cancels a gRPC call or stream
    linker.func_wrap(
        "env",
        "proxy_grpc_cancel",
        |mut caller: Caller<'_, HostState>, call_id: i32| -> i32 {
            #[cfg(feature = "grpc")]
            {
                proxy_grpc_cancel_impl(&mut caller, call_id)
            }
            #[cfg(not(feature = "grpc"))]
            {
                let _ = call_id;
                ftlog::debug!("WASM: proxy_grpc_cancel called (grpc feature not enabled)");
                PROXY_RESULT_UNIMPLEMENTED
            }
        },
    )?;

    // proxy_grpc_close
    // Closes a gRPC stream
    linker.func_wrap(
        "env",
        "proxy_grpc_close",
        |_caller: Caller<'_, HostState>, _stream_id: i32| -> i32 {
            ftlog::debug!("WASM: proxy_grpc_close called (streaming not yet supported)");
            PROXY_RESULT_UNIMPLEMENTED
        },
    )?;

    // proxy_grpc_send
    // Sends a message on a gRPC stream
    linker.func_wrap(
        "env",
        "proxy_grpc_send",
        |_caller: Caller<'_, HostState>,
         _stream_id: i32,
         _message_ptr: i32,
         _message_size: i32,
         _end_of_stream: i32|
         -> i32 {
            ftlog::debug!("WASM: proxy_grpc_send called (streaming not yet supported)");
            PROXY_RESULT_UNIMPLEMENTED
        },
    )?;

    Ok(())
}

// ============================================================================
// gRPC Implementation (feature = "grpc")
// ============================================================================

#[cfg(feature = "grpc")]
fn proxy_grpc_call_impl(
    caller: &mut Caller<'_, HostState>,
    upstream_ptr: i32,
    upstream_size: i32,
    service_ptr: i32,
    service_size: i32,
    method_ptr: i32,
    method_size: i32,
    message_ptr: i32,
    message_size: i32,
    timeout_ms: i32,
    return_call_id_ptr: i32,
) -> i32 {
    // Read upstream name
    let upstream = match read_wasm_memory(caller, upstream_ptr, upstream_size) {
        Some(data) => data,
        None => {
            ftlog::warn!("WASM: proxy_grpc_call - failed to read upstream name");
            return PROXY_RESULT_INVALID_MEMORY_ACCESS;
        }
    };

    // Read service name
    let service = match read_wasm_memory(caller, service_ptr, service_size) {
        Some(data) => data,
        None => {
            ftlog::warn!("WASM: proxy_grpc_call - failed to read service name");
            return PROXY_RESULT_INVALID_MEMORY_ACCESS;
        }
    };

    // Read method name
    let method = match read_wasm_memory(caller, method_ptr, method_size) {
        Some(data) => data,
        None => {
            ftlog::warn!("WASM: proxy_grpc_call - failed to read method name");
            return PROXY_RESULT_INVALID_MEMORY_ACCESS;
        }
    };

    // Read message
    let message = match read_wasm_memory(caller, message_ptr, message_size) {
        Some(data) => data,
        None => {
            ftlog::warn!("WASM: proxy_grpc_call - failed to read message");
            return PROXY_RESULT_INVALID_MEMORY_ACCESS;
        }
    };

    ftlog::info!(
        "WASM: proxy_grpc_call - upstream={}, service={}, method={}, message_size={}, timeout_ms={}",
        String::from_utf8_lossy(&upstream),
        String::from_utf8_lossy(&service),
        String::from_utf8_lossy(&method),
        message.len(),
        timeout_ms
    );

    // Build gRPC path: /<service>/<method>
    let grpc_path = format!(
        "/{}/{}",
        String::from_utf8_lossy(&service),
        String::from_utf8_lossy(&method)
    );

    // Store pending gRPC call info in context for later execution
    let state = caller.data_mut();
    let call_id = state.http_ctx.next_grpc_call_id();
    state.http_ctx.register_grpc_call(call_id, grpc_path, message, timeout_ms as u32);

    // Write call_id to return pointer
    if return_call_id_ptr > 0 {
        if let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) {
            let call_id_bytes = (call_id as u32).to_le_bytes();
            let data = memory.data_mut(&mut *caller);
            let ptr = return_call_id_ptr as usize;
            if ptr + 4 <= data.len() {
                data[ptr..ptr + 4].copy_from_slice(&call_id_bytes);
            } else {
                ftlog::warn!("WASM: proxy_grpc_call - failed to write call_id");
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }
        }
    }

    ftlog::debug!("WASM: proxy_grpc_call registered with call_id={}", call_id);
    PROXY_RESULT_OK
}

#[cfg(feature = "grpc")]
fn proxy_grpc_cancel_impl(
    caller: &mut Caller<'_, HostState>,
    call_id: i32,
) -> i32 {
    let state = caller.data_mut();
    
    if state.http_ctx.cancel_grpc_call(call_id as u32) {
        ftlog::debug!("WASM: proxy_grpc_cancel - cancelled call_id={}", call_id);
        PROXY_RESULT_OK
    } else {
        ftlog::debug!("WASM: proxy_grpc_cancel - call_id={} not found", call_id);
        PROXY_RESULT_NOT_FOUND
    }
}
