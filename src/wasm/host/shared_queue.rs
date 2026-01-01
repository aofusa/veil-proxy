//! Shared Queue Host Functions
//!
//! Implements Proxy-Wasm shared queue functions for inter-module communication.
//! Queues are named and accessible across all WASM module instances.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, RwLock};

use once_cell::sync::Lazy;
use wasmtime::{Caller, Linker};

use crate::wasm::constants::*;
use crate::wasm::context::HostState;

/// Maximum number of messages per queue
const MAX_QUEUE_SIZE: usize = 1000;

/// Maximum message size in bytes
const MAX_MESSAGE_SIZE: usize = 65536; // 64KB

/// Global shared queue registry
static SHARED_QUEUES: Lazy<Arc<RwLock<SharedQueueRegistry>>> =
    Lazy::new(|| Arc::new(RwLock::new(SharedQueueRegistry::new())));

/// Shared queue registry
struct SharedQueueRegistry {
    /// Queue name to ID mapping
    name_to_id: HashMap<String, u32>,
    /// Queue ID to queue data
    queues: HashMap<u32, VecDeque<Vec<u8>>>,
    /// Next queue ID to allocate
    next_queue_id: u32,
}

impl SharedQueueRegistry {
    fn new() -> Self {
        Self {
            name_to_id: HashMap::new(),
            queues: HashMap::new(),
            next_queue_id: 1,
        }
    }

    /// Register or get existing queue by name
    fn register(&mut self, name: &str) -> u32 {
        if let Some(&id) = self.name_to_id.get(name) {
            return id;
        }

        let id = self.next_queue_id;
        self.next_queue_id += 1;
        self.name_to_id.insert(name.to_string(), id);
        self.queues.insert(id, VecDeque::new());
        id
    }

    /// Resolve queue name to ID
    fn resolve(&self, name: &str) -> Option<u32> {
        self.name_to_id.get(name).copied()
    }

    /// Enqueue data to a queue
    fn enqueue(&mut self, queue_id: u32, data: Vec<u8>) -> bool {
        if let Some(queue) = self.queues.get_mut(&queue_id) {
            if queue.len() < MAX_QUEUE_SIZE && data.len() <= MAX_MESSAGE_SIZE {
                queue.push_back(data);
                return true;
            }
        }
        false
    }

    /// Dequeue data from a queue
    fn dequeue(&mut self, queue_id: u32) -> Option<Vec<u8>> {
        if let Some(queue) = self.queues.get_mut(&queue_id) {
            queue.pop_front()
        } else {
            None
        }
    }
}

/// Add shared queue functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // proxy_register_shared_queue
    // Registers or opens a shared queue with the given name
    linker.func_wrap(
        "env",
        "proxy_register_shared_queue",
        |mut caller: Caller<'_, HostState>,
         name_ptr: i32,
         name_size: i32,
         return_queue_id_ptr: i32|
         -> i32 {
            // Read queue name from memory
            let name = match read_string(&mut caller, name_ptr, name_size) {
                Some(n) => n,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Register the queue
            let queue_id = match SHARED_QUEUES.write() {
                Ok(mut registry) => registry.register(&name),
                Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            // Write queue_id to return pointer
            if return_queue_id_ptr > 0 {
                if !write_u32(&mut caller, return_queue_id_ptr, queue_id) {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
            }

            ftlog::debug!("WASM: proxy_register_shared_queue '{}' -> id={}", name, queue_id);
            PROXY_RESULT_OK
        },
    )?;

    // proxy_resolve_shared_queue
    // Resolves a shared queue name to its ID
    linker.func_wrap(
        "env",
        "proxy_resolve_shared_queue",
        |mut caller: Caller<'_, HostState>,
         _vm_id_ptr: i32,
         _vm_id_size: i32,
         name_ptr: i32,
         name_size: i32,
         return_queue_id_ptr: i32|
         -> i32 {
            // Read queue name
            let name = match read_string(&mut caller, name_ptr, name_size) {
                Some(n) => n,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Look up queue ID
            let queue_id = match SHARED_QUEUES.read() {
                Ok(registry) => registry.resolve(&name),
                Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            match queue_id {
                Some(id) => {
                    if return_queue_id_ptr > 0 {
                        if !write_u32(&mut caller, return_queue_id_ptr, id) {
                            return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                        }
                    }
                    ftlog::debug!("WASM: proxy_resolve_shared_queue '{}' -> id={}", name, id);
                    PROXY_RESULT_OK
                }
                None => {
                    ftlog::debug!("WASM: proxy_resolve_shared_queue '{}' not found", name);
                    PROXY_RESULT_NOT_FOUND
                }
            }
        },
    )?;

    // proxy_enqueue_shared_queue
    // Enqueues data to a shared queue
    linker.func_wrap(
        "env",
        "proxy_enqueue_shared_queue",
        |mut caller: Caller<'_, HostState>,
         queue_id: i32,
         data_ptr: i32,
         data_size: i32|
         -> i32 {
            // Read data from memory
            let data = if data_size > 0 {
                match read_bytes(&mut caller, data_ptr, data_size) {
                    Some(d) => d,
                    None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                }
            } else {
                Vec::new()
            };

            // Enqueue
            let success = match SHARED_QUEUES.write() {
                Ok(mut registry) => registry.enqueue(queue_id as u32, data),
                Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            if success {
                ftlog::debug!("WASM: proxy_enqueue_shared_queue id={} size={}", queue_id, data_size);
                PROXY_RESULT_OK
            } else {
                ftlog::debug!("WASM: proxy_enqueue_shared_queue id={} failed (queue full or not found)", queue_id);
                PROXY_RESULT_NOT_FOUND
            }
        },
    )?;

    // proxy_dequeue_shared_queue
    // Dequeues data from a shared queue
    linker.func_wrap(
        "env",
        "proxy_dequeue_shared_queue",
        |mut caller: Caller<'_, HostState>,
         queue_id: i32,
         return_data_ptr: i32,
         return_data_size_ptr: i32|
         -> i32 {
            // Dequeue
            let data = match SHARED_QUEUES.write() {
                Ok(mut registry) => registry.dequeue(queue_id as u32),
                Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            match data {
                Some(bytes) => {
                    // Allocate memory in WASM and write data
                    // For simplicity, we expect the caller to have preallocated buffer
                    // Write size first
                    if return_data_size_ptr > 0 {
                        if !write_u32(&mut caller, return_data_size_ptr, bytes.len() as u32) {
                            return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                        }
                    }

                    // Write data
                    if return_data_ptr > 0 && !bytes.is_empty() {
                        if !write_bytes(&mut caller, return_data_ptr, &bytes) {
                            return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                        }
                    }

                    ftlog::debug!("WASM: proxy_dequeue_shared_queue id={} size={}", queue_id, bytes.len());
                    PROXY_RESULT_OK
                }
                None => {
                    // Queue empty or not found
                    if return_data_size_ptr > 0 {
                        let _ = write_u32(&mut caller, return_data_size_ptr, 0);
                    }
                    PROXY_RESULT_EMPTY
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
