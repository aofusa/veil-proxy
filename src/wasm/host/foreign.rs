//! Foreign Function Host Functions
//!
//! Implements Proxy-Wasm foreign function calls.
//! Currently returns UNIMPLEMENTED.

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Add foreign function calls to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_call_foreign_function
    // Calls a function provided by the host
    linker.func_wrap(
        "env",
        "proxy_call_foreign_function",
        |_caller: Caller<'_, HostState>,
         _function_name_ptr: i32,
         _function_name_size: i32,
         _arguments_ptr: i32,
         _arguments_size: i32,
         _return_results_ptr: i32,
         _return_results_size_ptr: i32|
         -> i32 {
            ftlog::debug!("WASM: proxy_call_foreign_function called (not supported)");
            PROXY_RESULT_UNIMPLEMENTED
        },
    )?;

    Ok(())
}
