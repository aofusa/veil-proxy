//! Foreign Function Host Functions
//!
//! Implements Proxy-Wasm foreign function calls.
//! Provides an extensible registry for host-provided functions.

use std::collections::HashMap;
use std::sync::RwLock;

use once_cell::sync::Lazy;
use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Foreign function type: takes arguments and returns results
pub type ForeignFn = fn(&[u8]) -> Result<Vec<u8>, i32>;

/// Global foreign function registry
static FOREIGN_FUNCTIONS: Lazy<RwLock<HashMap<String, ForeignFn>>> =
    Lazy::new(|| RwLock::new(HashMap::new()));

/// Register a foreign function
pub fn register_foreign_function(name: &str, func: ForeignFn) {
    if let Ok(mut registry) = FOREIGN_FUNCTIONS.write() {
        registry.insert(name.to_string(), func);
        ftlog::info!("Registered foreign function: {}", name);
    }
}

/// Unregister a foreign function
#[allow(dead_code)]
pub fn unregister_foreign_function(name: &str) {
    if let Ok(mut registry) = FOREIGN_FUNCTIONS.write() {
        registry.remove(name);
    }
}

/// Add foreign function calls to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_call_foreign_function
    // Calls a function provided by the host
    linker.func_wrap(
        "env",
        "proxy_call_foreign_function",
        |mut caller: Caller<'_, HostState>,
         function_name_ptr: i32,
         function_name_size: i32,
         arguments_ptr: i32,
         arguments_size: i32,
         return_results_ptr: i32,
         return_results_size_ptr: i32|
         -> i32 {
            // Read function name
            let function_name = match read_string(&mut caller, function_name_ptr, function_name_size) {
                Some(n) => n,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Read arguments
            let arguments = if arguments_size > 0 {
                match read_bytes(&mut caller, arguments_ptr, arguments_size) {
                    Some(a) => a,
                    None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                }
            } else {
                Vec::new()
            };

            // Look up the function
            let func = match FOREIGN_FUNCTIONS.read() {
                Ok(registry) => registry.get(&function_name).copied(),
                Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            match func {
                Some(f) => {
                    // Call the function
                    match f(&arguments) {
                        Ok(results) => {
                            // Write results size
                            if return_results_size_ptr > 0 {
                                if !write_u32(&mut caller, return_results_size_ptr, results.len() as u32) {
                                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                                }
                            }

                            // Write results data
                            if return_results_ptr > 0 && !results.is_empty() {
                                if !write_bytes(&mut caller, return_results_ptr, &results) {
                                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                                }
                            }

                            ftlog::debug!(
                                "WASM: proxy_call_foreign_function '{}' called, returned {} bytes",
                                function_name, results.len()
                            );
                            PROXY_RESULT_OK
                        }
                        Err(code) => {
                            ftlog::debug!(
                                "WASM: proxy_call_foreign_function '{}' returned error: {}",
                                function_name, code
                            );
                            code
                        }
                    }
                }
                None => {
                    ftlog::debug!(
                        "WASM: proxy_call_foreign_function '{}' not found",
                        function_name
                    );
                    PROXY_RESULT_NOT_FOUND
                }
            }
        },
    )?;

    Ok(())
}

// Helper functions

fn read_string(caller: &mut Caller<'_, HostState>, ptr: i32, size: i32) -> Option<String> {
    let memory = caller.get_export("memory")?.into_memory()?;
    let data = memory.data(caller);
    let start = ptr as usize;
    let end = start + size as usize;
    if end > data.len() {
        return None;
    }
    String::from_utf8(data[start..end].to_vec()).ok()
}

fn read_bytes(caller: &mut Caller<'_, HostState>, ptr: i32, size: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory")?.into_memory()?;
    let data = memory.data(caller);
    let start = ptr as usize;
    let end = start + size as usize;
    if end > data.len() {
        return None;
    }
    Some(data[start..end].to_vec())
}

fn write_u32(caller: &mut Caller<'_, HostState>, ptr: i32, value: u32) -> bool {
    if let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) {
        let data = memory.data_mut(caller);
        let start = ptr as usize;
        if start + 4 <= data.len() {
            data[start..start + 4].copy_from_slice(&value.to_le_bytes());
            return true;
        }
    }
    false
}

fn write_bytes(caller: &mut Caller<'_, HostState>, ptr: i32, bytes: &[u8]) -> bool {
    if let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) {
        let data = memory.data_mut(caller);
        let start = ptr as usize;
        if start + bytes.len() <= data.len() {
            data[start..start + bytes.len()].copy_from_slice(bytes);
            return true;
        }
    }
    false
}
