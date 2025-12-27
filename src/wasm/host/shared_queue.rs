//! Shared Queue Host Functions
//!
//! Implements Proxy-Wasm shared queue functions for inter-module communication.
//! Currently returns NOT_FOUND as shared queues are not yet implemented.

use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Add shared queue functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_register_shared_queue
    // Registers or opens a shared queue with the given name
    // params: name_ptr, name_size, return_queue_id_ptr
    // Returns: status code
    linker.func_wrap(
        "env",
        "proxy_register_shared_queue",
        |mut caller: Caller<'_, HostState>,
         name_ptr: i32,
         name_size: i32,
         return_queue_id_ptr: i32|
         -> i32 {
            // Read queue name from memory for logging
            if let Some(wasmtime::Extern::Memory(memory)) = caller.get_export("memory") {
                let data = memory.data(&caller);
                let start = name_ptr as usize;
                let end = start + name_size as usize;
                
                if end <= data.len() {
                    if let Ok(name) = std::str::from_utf8(&data[start..end]) {
                        ftlog::debug!("WASM: proxy_register_shared_queue called for '{}'", name);
                    }
                }
                
                // Write a dummy queue_id (0) to the return pointer
                if return_queue_id_ptr > 0 {
                    let data = memory.data_mut(&mut caller);
                    let ptr = return_queue_id_ptr as usize;
                    if ptr + 4 <= data.len() {
                        data[ptr..ptr + 4].copy_from_slice(&0u32.to_le_bytes());
                    }
                }
            }
            
            // Shared queues not supported - return NOT_FOUND
            // This allows modules that import the function to load even if they don't use it
            PROXY_RESULT_NOT_FOUND
        },
    )?;

    // proxy_resolve_shared_queue
    // Resolves a shared queue name to its ID
    // params: vm_id_ptr, vm_id_size, name_ptr, name_size, return_queue_id_ptr
    // Returns: status code
    linker.func_wrap(
        "env",
        "proxy_resolve_shared_queue",
        |_caller: Caller<'_, HostState>,
         _vm_id_ptr: i32,
         _vm_id_size: i32,
         _name_ptr: i32,
         _name_size: i32,
         _return_queue_id_ptr: i32|
         -> i32 {
            ftlog::debug!("WASM: proxy_resolve_shared_queue called (not supported)");
            PROXY_RESULT_NOT_FOUND
        },
    )?;

    // proxy_enqueue_shared_queue
    // Enqueues data to a shared queue
    // params: queue_id, data_ptr, data_size
    // Returns: status code
    linker.func_wrap(
        "env",
        "proxy_enqueue_shared_queue",
        |_caller: Caller<'_, HostState>,
         queue_id: i32,
         _data_ptr: i32,
         _data_size: i32|
         -> i32 {
            ftlog::debug!("WASM: proxy_enqueue_shared_queue called for queue {} (not supported)", queue_id);
            PROXY_RESULT_NOT_FOUND
        },
    )?;

    // proxy_dequeue_shared_queue
    // Dequeues data from a shared queue
    // params: queue_id, return_data_ptr, return_data_size_ptr
    // Returns: status code
    linker.func_wrap(
        "env",
        "proxy_dequeue_shared_queue",
        |_caller: Caller<'_, HostState>,
         queue_id: i32,
         _return_data_ptr: i32,
         _return_data_size_ptr: i32|
         -> i32 {
            ftlog::debug!("WASM: proxy_dequeue_shared_queue called for queue {} (not supported)", queue_id);
            PROXY_RESULT_NOT_FOUND
        },
    )?;

    Ok(())
}
