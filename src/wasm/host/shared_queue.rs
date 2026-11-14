//! Shared Queue Host Functions
//!
//! Implements Proxy-Wasm shared queue functions for inter-module communication.
//! A queue belongs to the module that registered it: only that module dequeues from it, and other modules enqueue
//! only after resolving it by that module's name (the `vm_id`). All of it needs the `allow_shared_data` capability.

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

/// One shared queue: the module that registered it (the only one that may dequeue), the modules that opened it
/// (registered or resolved it; only they may enqueue) and the messages.
struct SharedQueue {
    owner: Arc<str>,
    openers: Vec<Arc<str>>,
    items: VecDeque<Vec<u8>>,
}

// B-395: キューは登録したモジュールごと（Proxy-Wasm の vm_id。veil ではモジュール名）。以前は全キューがプロセスで
// 共有され、ホスト関数は権限も見なかったので、何も許可されていないモジュールがキューの番号を順に試して別の
// モジュールのメッセージを読み取り・偽造できた
/// Shared queue registry: queues are scoped by the module that registered them (the Proxy-Wasm `vm_id`, which is
/// the module name in veil).
struct SharedQueueRegistry {
    /// (owner module, queue name) to ID mapping
    name_to_id: HashMap<(Arc<str>, String), u32>,
    /// Queue ID to queue data
    queues: HashMap<u32, SharedQueue>,
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

    /// Register (or open again) the caller's own queue by name
    fn register(&mut self, owner: &Arc<str>, name: &str) -> u32 {
        if let Some(&id) = self.name_to_id.get(&(owner.clone(), name.to_string())) {
            return id;
        }

        let id = self.next_queue_id;
        self.next_queue_id += 1;
        self.name_to_id
            .insert((owner.clone(), name.to_string()), id);
        self.queues.insert(
            id,
            SharedQueue {
                owner: owner.clone(),
                openers: vec![owner.clone()],
                items: VecDeque::new(),
            },
        );
        id
    }

    /// Resolve a queue registered by `vm_id` (the caller's own module when empty) and remember that the caller
    /// opened it
    fn resolve(&mut self, caller: &Arc<str>, vm_id: &str, name: &str) -> Option<u32> {
        let owner: Arc<str> = if vm_id.is_empty() {
            caller.clone()
        } else {
            Arc::from(vm_id)
        };
        let id = *self.name_to_id.get(&(owner, name.to_string()))?;
        if let Some(q) = self.queues.get_mut(&id) {
            if !q.openers.iter().any(|m| m == caller) {
                q.openers.push(caller.clone());
            }
        }
        Some(id)
    }

    /// Enqueue data to a queue the caller opened
    fn enqueue(&mut self, caller: &Arc<str>, queue_id: u32, data: Vec<u8>) -> bool {
        if let Some(queue) = self.queues.get_mut(&queue_id) {
            if queue.openers.iter().any(|m| m == caller)
                && queue.items.len() < MAX_QUEUE_SIZE
                && data.len() <= MAX_MESSAGE_SIZE
            {
                queue.items.push_back(data);
                return true;
            }
        }
        false
    }

    /// Dequeue data from the caller's own queue
    fn dequeue(&mut self, caller: &Arc<str>, queue_id: u32) -> Option<Vec<u8>> {
        let queue = self.queues.get_mut(&queue_id)?;
        if &queue.owner != caller {
            return None;
        }
        queue.items.pop_front()
    }
}

/// The calling module's name when it may use shared queues (they need `allow_shared_data`, like shared data).
fn queue_caller(caller: &Caller<'_, HostState>) -> Option<Arc<str>> {
    let ctx = &caller.data().http_ctx;
    ctx.capabilities
        .allow_shared_data
        .then(|| ctx.plugin_name.clone())
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
            let Some(owner) = queue_caller(&caller) else {
                return PROXY_RESULT_NOT_ALLOWED;
            };
            // Read queue name from memory
            let name = match read_string(&mut caller, name_ptr, name_size) {
                Some(n) => n,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Register the queue
            let queue_id = match SHARED_QUEUES.write() {
                Ok(mut registry) => registry.register(&owner, &name),
                Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            // Write queue_id to return pointer
            if return_queue_id_ptr > 0 && !write_u32(&mut caller, return_queue_id_ptr, queue_id) {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }

            ftlog::debug!(
                "WASM: proxy_register_shared_queue '{}' -> id={}",
                name,
                queue_id
            );
            PROXY_RESULT_OK
        },
    )?;

    // proxy_resolve_shared_queue
    // Resolves a shared queue name to its ID
    linker.func_wrap(
        "env",
        "proxy_resolve_shared_queue",
        |mut caller: Caller<'_, HostState>,
         vm_id_ptr: i32,
         vm_id_size: i32,
         name_ptr: i32,
         name_size: i32,
         return_queue_id_ptr: i32|
         -> i32 {
            let Some(me) = queue_caller(&caller) else {
                return PROXY_RESULT_NOT_ALLOWED;
            };
            // vm_id: the module that registered the queue (empty = the caller's own)
            let vm_id = if vm_id_size > 0 {
                match read_string(&mut caller, vm_id_ptr, vm_id_size) {
                    Some(v) => v,
                    None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
                }
            } else {
                String::new()
            };
            // Read queue name
            let name = match read_string(&mut caller, name_ptr, name_size) {
                Some(n) => n,
                None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
            };

            // Look up queue ID
            let queue_id = match SHARED_QUEUES.write() {
                Ok(mut registry) => registry.resolve(&me, &vm_id, &name),
                Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            match queue_id {
                Some(id) => {
                    if return_queue_id_ptr > 0 && !write_u32(&mut caller, return_queue_id_ptr, id) {
                        return PROXY_RESULT_INVALID_MEMORY_ACCESS;
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
        |mut caller: Caller<'_, HostState>, queue_id: i32, data_ptr: i32, data_size: i32| -> i32 {
            let Some(me) = queue_caller(&caller) else {
                return PROXY_RESULT_NOT_ALLOWED;
            };
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
                Ok(mut registry) => registry.enqueue(&me, queue_id as u32, data),
                Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
            };

            if success {
                ftlog::debug!(
                    "WASM: proxy_enqueue_shared_queue id={} size={}",
                    queue_id,
                    data_size
                );
                // Mark queue for pending notification (will be processed in tick loop)
                crate::wasm::queue_notify::queue_enqueued(queue_id as u32);
                PROXY_RESULT_OK
            } else {
                ftlog::debug!(
                    "WASM: proxy_enqueue_shared_queue id={} failed (queue full or not found)",
                    queue_id
                );
                PROXY_RESULT_NOT_FOUND
            }
        },
    )?;

    // proxy_dequeue_shared_queue
    // Dequeues data from a shared queue
    // B-247: Proxy-Wasm ABI どおり、ゲストのアロケータ（proxy_on_memory_allocate、無ければ malloc）で
    // 確保した領域にペイロードを書き、その番地と長さを return_data_ptr / return_data_size に書く
    // （以前は return_data_ptr の番地へペイロードそのものを書き、SDK が不正なポインタから Vec を作っていた）。
    // ゲストのアロケータへ再入するため async ホスト関数（B-20）
    linker.func_wrap_async(
        "env",
        "proxy_dequeue_shared_queue",
        |mut caller: Caller<'_, HostState>,
         (queue_id, return_data_ptr, return_data_size_ptr): (i32, i32, i32)| {
            Box::new(async move {
                let Some(me) = queue_caller(&caller) else {
                    return PROXY_RESULT_NOT_ALLOWED;
                };
                let data = match SHARED_QUEUES.write() {
                    Ok(mut registry) => registry.dequeue(&me, queue_id as u32),
                    Err(_) => return PROXY_RESULT_INTERNAL_FAILURE,
                };
                let Some(bytes) = data else {
                    // Queue empty or not found
                    return PROXY_RESULT_EMPTY;
                };

                let ptr = if bytes.is_empty() {
                    0
                } else {
                    match super::shared_data::allocate_wasm_memory(&mut caller, bytes.len()).await {
                        Some(p) => p,
                        None => return PROXY_RESULT_INTERNAL_FAILURE,
                    }
                };
                if !bytes.is_empty() && !write_bytes(&mut caller, ptr, &bytes) {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }
                if !write_u32(&mut caller, return_data_ptr, ptr as u32)
                    || !write_u32(&mut caller, return_data_size_ptr, bytes.len() as u32)
                {
                    return PROXY_RESULT_INVALID_MEMORY_ACCESS;
                }

                ftlog::debug!(
                    "WASM: proxy_dequeue_shared_queue id={} size={}",
                    queue_id,
                    bytes.len()
                );
                PROXY_RESULT_OK
            })
        },
    )?;

    Ok(())
}

// Helper functions

fn read_string(caller: &mut Caller<'_, HostState>, ptr: i32, size: i32) -> Option<String> {
    let memory = caller.get_export("memory")?.into_memory()?;
    let data = memory.data(caller);
    let start = ptr as u32 as usize;
    let end = start + size as u32 as usize;
    if end > data.len() {
        return None;
    }
    String::from_utf8(data[start..end].to_vec()).ok()
}

fn read_bytes(caller: &mut Caller<'_, HostState>, ptr: i32, size: i32) -> Option<Vec<u8>> {
    let memory = caller.get_export("memory")?.into_memory()?;
    let data = memory.data(caller);
    let start = ptr as u32 as usize;
    let end = start + size as u32 as usize;
    if end > data.len() {
        return None;
    }
    Some(data[start..end].to_vec())
}

fn write_u32(caller: &mut Caller<'_, HostState>, ptr: i32, value: u32) -> bool {
    if let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) {
        let data = memory.data_mut(caller);
        let start = ptr as u32 as usize;
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
        let start = ptr as u32 as usize;
        if start + bytes.len() <= data.len() {
            data[start..start + bytes.len()].copy_from_slice(bytes);
            return true;
        }
    }
    false
}
