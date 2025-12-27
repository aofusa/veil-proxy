//! gRPC Host Functions
//!
//! Implements Proxy-Wasm gRPC functions.
//! Currently returns UNIMPLEMENTED as gRPC is not yet supported.

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Add gRPC functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_grpc_call
    // Initiates a gRPC call
    linker.func_wrap(
        "env",
        "proxy_grpc_call",
        |_caller: Caller<'_, HostState>,
         _upstream_ptr: i32,
         _upstream_size: i32,
         _service_ptr: i32,
         _service_size: i32,
         _method_ptr: i32,
         _method_size: i32,
         _initial_metadata_ptr: i32,
         _initial_metadata_size: i32,
         _message_ptr: i32,
         _message_size: i32,
         _timeout_ms: i32,
         _return_call_id_ptr: i32|
         -> i32 {
            ftlog::debug!("WASM: proxy_grpc_call called (not supported)");
            PROXY_RESULT_UNIMPLEMENTED
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
            ftlog::debug!("WASM: proxy_grpc_stream called (not supported)");
            PROXY_RESULT_UNIMPLEMENTED
        },
    )?;

    // proxy_grpc_cancel
    // Cancels a gRPC call or stream
    linker.func_wrap(
        "env",
        "proxy_grpc_cancel",
        |_caller: Caller<'_, HostState>, _call_id: i32| -> i32 {
            ftlog::debug!("WASM: proxy_grpc_cancel called (not supported)");
            PROXY_RESULT_UNIMPLEMENTED
        },
    )?;

    // proxy_grpc_close
    // Closes a gRPC stream
    linker.func_wrap(
        "env",
        "proxy_grpc_close",
        |_caller: Caller<'_, HostState>, _stream_id: i32| -> i32 {
            ftlog::debug!("WASM: proxy_grpc_close called (not supported)");
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
            ftlog::debug!("WASM: proxy_grpc_send called (not supported)");
            PROXY_RESULT_UNIMPLEMENTED
        },
    )?;

    Ok(())
}
