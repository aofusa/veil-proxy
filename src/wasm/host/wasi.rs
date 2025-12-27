//! WASI Stub Functions
//!
//! Provides minimal WASI function stubs to allow Proxy-Wasm modules that
//! reference WASI to load. These implementations are minimal stubs that
//! allow the module to function without full WASI support.

use wasmtime::{Caller, Linker};

use crate::wasm::context::HostState;

/// Add WASI stub functions to linker
pub fn add_functions(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
    // fd_write - Write to a file descriptor
    // For WASM modules, this is typically used for stdout/stderr logging
    // We redirect this to our logging system
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_write",
        |mut caller: Caller<'_, HostState>,
         fd: i32,
         iovs_ptr: i32,
         iovs_len: i32,
         nwritten_ptr: i32|
         -> i32 {
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return 8, // EBADF - Bad file descriptor
            };

            // Only handle stdout (1) and stderr (2)
            if fd != 1 && fd != 2 {
                return 8; // EBADF
            }

            let data = memory.data(&caller);
            let mut total_written = 0u32;

            // Read iov structures
            for i in 0..iovs_len {
                let iov_ptr = (iovs_ptr + i * 8) as usize;
                if iov_ptr + 8 > data.len() {
                    return 21; // EINVAL
                }

                let buf_ptr = u32::from_le_bytes([
                    data[iov_ptr],
                    data[iov_ptr + 1],
                    data[iov_ptr + 2],
                    data[iov_ptr + 3],
                ]) as usize;
                let buf_len = u32::from_le_bytes([
                    data[iov_ptr + 4],
                    data[iov_ptr + 5],
                    data[iov_ptr + 6],
                    data[iov_ptr + 7],
                ]) as usize;

                if buf_ptr + buf_len > data.len() {
                    return 21; // EINVAL
                }

                // Get the text content
                if let Ok(text) = std::str::from_utf8(&data[buf_ptr..buf_ptr + buf_len]) {
                    // Log the output (strip trailing newline for cleaner logs)
                    let text = text.trim_end_matches('\n');
                    if !text.is_empty() {
                        let plugin_name = &caller.data().http_ctx.plugin_name;
                        ftlog::debug!("[wasm:{}] {}", plugin_name, text);
                    }
                }

                total_written += buf_len as u32;
            }

            // Write number of bytes written
            let data = memory.data_mut(&mut caller);
            let ptr = nwritten_ptr as usize;
            if ptr + 4 > data.len() {
                return 21; // EINVAL
            }
            data[ptr..ptr + 4].copy_from_slice(&total_written.to_le_bytes());

            0 // Success (ESUCCESS)
        },
    )?;

    // fd_read - Read from a file descriptor
    // Return EOF for stdin
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_read",
        |_caller: Caller<'_, HostState>,
         _fd: i32,
         _iovs_ptr: i32,
         _iovs_len: i32,
         _nread_ptr: i32|
         -> i32 {
            0 // Return 0 bytes read (EOF)
        },
    )?;

    // fd_close - Close a file descriptor
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_close",
        |_caller: Caller<'_, HostState>, _fd: i32| -> i32 {
            0 // Success
        },
    )?;

    // fd_seek - Seek in a file descriptor
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_seek",
        |_caller: Caller<'_, HostState>,
         _fd: i32,
         _offset: i64,
         _whence: i32,
         _newoffset_ptr: i32|
         -> i32 {
            8 // EBADF - Not seekable
        },
    )?;

    // fd_fdstat_get - Get file descriptor status
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_fdstat_get",
        |mut caller: Caller<'_, HostState>, fd: i32, stat_ptr: i32| -> i32 {
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return 8,
            };

            let data = memory.data_mut(&mut caller);
            let ptr = stat_ptr as usize;
            if ptr + 24 > data.len() {
                return 21; // EINVAL
            }

            // Return a basic file stat for stdout/stderr
            let filetype = if fd <= 2 { 2u8 } else { 0u8 }; // CHARACTER_DEVICE for stdio
            data[ptr] = filetype;
            // Zero out the rest
            for i in 1..24 {
                data[ptr + i] = 0;
            }

            0 // Success
        },
    )?;

    // fd_prestat_get - Get preopened directory info
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_prestat_get",
        |_caller: Caller<'_, HostState>, _fd: i32, _prestat_ptr: i32| -> i32 {
            8 // EBADF - No preopened directories
        },
    )?;

    // fd_prestat_dir_name - Get preopened directory name
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "fd_prestat_dir_name",
        |_caller: Caller<'_, HostState>,
         _fd: i32,
         _path_ptr: i32,
         _path_len: i32|
         -> i32 {
            8 // EBADF
        },
    )?;

    // environ_sizes_get - Get environment variable sizes
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "environ_sizes_get",
        |mut caller: Caller<'_, HostState>, count_ptr: i32, size_ptr: i32| -> i32 {
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return 8,
            };

            let data = memory.data_mut(&mut caller);
            
            // Write 0 environment variables
            let count_p = count_ptr as usize;
            let size_p = size_ptr as usize;
            if count_p + 4 > data.len() || size_p + 4 > data.len() {
                return 21;
            }
            data[count_p..count_p + 4].copy_from_slice(&0u32.to_le_bytes());
            data[size_p..size_p + 4].copy_from_slice(&0u32.to_le_bytes());

            0 // Success
        },
    )?;

    // environ_get - Get environment variables
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "environ_get",
        |_caller: Caller<'_, HostState>, _environ_ptr: i32, _environ_buf_ptr: i32| -> i32 {
            0 // Success (no env vars)
        },
    )?;

    // args_sizes_get - Get command line argument sizes
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "args_sizes_get",
        |mut caller: Caller<'_, HostState>, count_ptr: i32, size_ptr: i32| -> i32 {
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return 8,
            };

            let data = memory.data_mut(&mut caller);
            
            // Write 0 arguments
            let count_p = count_ptr as usize;
            let size_p = size_ptr as usize;
            if count_p + 4 > data.len() || size_p + 4 > data.len() {
                return 21;
            }
            data[count_p..count_p + 4].copy_from_slice(&0u32.to_le_bytes());
            data[size_p..size_p + 4].copy_from_slice(&0u32.to_le_bytes());

            0 // Success
        },
    )?;

    // args_get - Get command line arguments
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "args_get",
        |_caller: Caller<'_, HostState>, _argv_ptr: i32, _argv_buf_ptr: i32| -> i32 {
            0 // Success (no args)
        },
    )?;

    // proc_exit - Exit the process
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "proc_exit",
        |_caller: Caller<'_, HostState>, _exit_code: i32| {
            // WASM modules shouldn't exit - just log and continue
            ftlog::warn!("WASM module called proc_exit");
        },
    )?;

    // clock_time_get - Get current time
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "clock_time_get",
        |mut caller: Caller<'_, HostState>,
         _clock_id: i32,
         _precision: i64,
         time_ptr: i32|
         -> i32 {
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return 8,
            };

            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;

            let data = memory.data_mut(&mut caller);
            let ptr = time_ptr as usize;
            if ptr + 8 > data.len() {
                return 21;
            }
            data[ptr..ptr + 8].copy_from_slice(&now.to_le_bytes());

            0 // Success
        },
    )?;

    // random_get - Get random bytes
    linker.func_wrap(
        "wasi_snapshot_preview1",
        "random_get",
        |mut caller: Caller<'_, HostState>, buf_ptr: i32, buf_len: i32| -> i32 {
            let memory = match caller.get_export("memory") {
                Some(wasmtime::Extern::Memory(mem)) => mem,
                _ => return 8,
            };

            let data = memory.data_mut(&mut caller);
            let ptr = buf_ptr as usize;
            let len = buf_len as usize;
            if ptr + len > data.len() {
                return 21;
            }

            // Use simple PRNG for random bytes
            use std::time::{SystemTime, UNIX_EPOCH};
            let mut seed = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos() as u64;

            for i in 0..len {
                seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
                data[ptr + i] = (seed >> 32) as u8;
            }

            0 // Success
        },
    )?;

    Ok(())
}
