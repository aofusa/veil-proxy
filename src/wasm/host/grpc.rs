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
         initial_metadata_ptr: i32,
         initial_metadata_size: i32,
         message_ptr: i32,
         message_size: i32,
         timeout_ms: i32,
         return_call_id_ptr: i32|
         -> i32 {
            #[cfg(feature = "grpc")]
            {
                proxy_grpc_call_impl(
                    &mut caller,
                    upstream_ptr,
                    upstream_size,
                    service_ptr,
                    service_size,
                    method_ptr,
                    method_size,
                    initial_metadata_ptr,
                    initial_metadata_size,
                    message_ptr,
                    message_size,
                    timeout_ms,
                    return_call_id_ptr,
                )
            }
            #[cfg(not(feature = "grpc"))]
            {
                let _ = (
                    &mut caller,
                    upstream_ptr,
                    upstream_size,
                    service_ptr,
                    service_size,
                    method_ptr,
                    method_size,
                    initial_metadata_ptr,
                    initial_metadata_size,
                    message_ptr,
                    message_size,
                    timeout_ms,
                    return_call_id_ptr,
                );
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
        |mut caller: Caller<'_, HostState>,
         upstream_ptr: i32,
         upstream_size: i32,
         service_ptr: i32,
         service_size: i32,
         method_ptr: i32,
         method_size: i32,
         initial_metadata_ptr: i32,
         initial_metadata_size: i32,
         return_stream_id_ptr: i32|
         -> i32 {
            #[cfg(feature = "grpc")]
            {
                proxy_grpc_stream_impl(
                    &mut caller,
                    upstream_ptr,
                    upstream_size,
                    service_ptr,
                    service_size,
                    method_ptr,
                    method_size,
                    initial_metadata_ptr,
                    initial_metadata_size,
                    return_stream_id_ptr,
                )
            }
            #[cfg(not(feature = "grpc"))]
            {
                let _ = (
                    &mut caller,
                    upstream_ptr,
                    upstream_size,
                    service_ptr,
                    service_size,
                    method_ptr,
                    method_size,
                    initial_metadata_ptr,
                    initial_metadata_size,
                    return_stream_id_ptr,
                );
                ftlog::debug!("WASM: proxy_grpc_stream called (grpc feature not enabled)");
                PROXY_RESULT_UNIMPLEMENTED
            }
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
                let _ = (&mut caller, call_id);
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
        |mut caller: Caller<'_, HostState>, stream_id: i32| -> i32 {
            #[cfg(feature = "grpc")]
            {
                proxy_grpc_close_impl(&mut caller, stream_id)
            }
            #[cfg(not(feature = "grpc"))]
            {
                let _ = (&mut caller, stream_id);
                ftlog::debug!("WASM: proxy_grpc_close called (grpc feature not enabled)");
                PROXY_RESULT_UNIMPLEMENTED
            }
        },
    )?;

    // proxy_grpc_send
    // Sends a message on a gRPC stream
    linker.func_wrap(
        "env",
        "proxy_grpc_send",
        |mut caller: Caller<'_, HostState>,
         stream_id: i32,
         message_ptr: i32,
         message_size: i32,
         end_of_stream: i32|
         -> i32 {
            #[cfg(feature = "grpc")]
            {
                proxy_grpc_send_impl(
                    &mut caller,
                    stream_id,
                    message_ptr,
                    message_size,
                    end_of_stream,
                )
            }
            #[cfg(not(feature = "grpc"))]
            {
                let _ = (
                    &mut caller,
                    stream_id,
                    message_ptr,
                    message_size,
                    end_of_stream,
                );
                ftlog::debug!("WASM: proxy_grpc_send called (grpc feature not enabled)");
                PROXY_RESULT_UNIMPLEMENTED
            }
        },
    )?;

    Ok(())
}

// ============================================================================
// gRPC Implementation (feature = "grpc")
// ============================================================================

/// F-134: proxy_grpc_call の 12 番目までの引数のうち、実際に使うものを渡す薄い
/// ラッパー引数構造体。呼び出し側（`add_functions`）が 12 個の位置引数を展開して
/// 渡すため、clippy::too_many_arguments を避けるために 1 か所へまとめる。
#[cfg(feature = "grpc")]
#[allow(clippy::too_many_arguments)] // ABI 上固定の 12 引数（ラッパー内で 1 回だけ集約する）
fn proxy_grpc_call_impl(
    caller: &mut Caller<'_, HostState>,
    upstream_ptr: i32,
    upstream_size: i32,
    service_ptr: i32,
    service_size: i32,
    method_ptr: i32,
    method_size: i32,
    initial_metadata_ptr: i32,
    initial_metadata_size: i32,
    message_ptr: i32,
    message_size: i32,
    timeout_ms: i32,
    return_call_id_ptr: i32,
) -> i32 {
    // F-134: 従来 proxy_http_call と異なり capability チェックが一切なく、
    // どのモジュールでも無制限に外向き gRPC 呼び出しを発行できてしまっていた。
    // proxy_http_call と同じ `allow_http_calls`/`allowed_upstreams` を流用する
    // （gRPC 専用の capability は新設しない。理由は headers.rs のコメント参照）。
    {
        let state = caller.data();
        if !state.http_ctx.capabilities.allow_http_calls {
            ftlog::warn!(
                "[wasm:{}] gRPC call denied: allow_http_calls=false",
                state.http_ctx.plugin_name
            );
            return PROXY_RESULT_NOT_ALLOWED;
        }
    }

    // Read upstream name
    let upstream_bytes = match read_wasm_memory(caller, upstream_ptr, upstream_size) {
        Some(data) => data,
        None => {
            ftlog::warn!("WASM: proxy_grpc_call - failed to read upstream name");
            return PROXY_RESULT_INVALID_MEMORY_ACCESS;
        }
    };
    let upstream = String::from_utf8_lossy(&upstream_bytes).to_string();

    {
        let state = caller.data();
        if !state.http_ctx.capabilities.is_upstream_allowed(&upstream) {
            ftlog::warn!(
                "[wasm:{}] gRPC call to '{}' denied: not in allowed_upstreams",
                state.http_ctx.plugin_name,
                upstream
            );
            return PROXY_RESULT_BAD_ARGUMENT;
        }
    }

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

    // Read initial metadata (optional)
    let initial_metadata = if initial_metadata_size > 0 {
        match read_wasm_memory(caller, initial_metadata_ptr, initial_metadata_size) {
            Some(bytes) => deserialize_grpc_metadata(&bytes),
            None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
        }
    } else {
        Vec::new()
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
        upstream,
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

    // Store pending gRPC call info in context (互換性維持: 既存の take_pending_grpc_calls
    // ベースのテスト・API のため）と、tick スレッドが実際に実行できるよう
    // グローバルレジストリの両方へ登録する（proxy_http_call と同じ二重登録方式）。
    let state = caller.data_mut();
    let call_id = state.http_ctx.next_grpc_call_id();
    let module_name = state.http_ctx.plugin_name.clone();
    state.http_ctx.register_grpc_call(
        call_id,
        grpc_path.clone(),
        message.clone(),
        timeout_ms as u32,
    );

    super::grpc_executor::register_global_pending_grpc_call(
        super::grpc_executor::PendingGrpcUnaryCall {
            module_name: module_name.clone(),
            call_id,
            upstream,
            path: grpc_path,
            initial_metadata,
            messages: vec![message],
            timeout_ms: timeout_ms as u32,
        },
    );

    // Write call_id to return pointer
    if return_call_id_ptr > 0 {
        if let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) {
            let call_id_bytes = call_id.to_le_bytes();
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

    ftlog::debug!(
        "[wasm:{}] proxy_grpc_call registered with call_id={} (queued for tick thread execution)",
        module_name,
        call_id
    );
    PROXY_RESULT_OK
}

#[cfg(feature = "grpc")]
fn proxy_grpc_cancel_impl(caller: &mut Caller<'_, HostState>, call_id: i32) -> i32 {
    let module_name = caller.data().http_ctx.plugin_name.clone();
    let state = caller.data_mut();

    // F-134: 従来はローカル pending_grpc_calls から消すだけで、tick スレッドが
    // 拾うグローバルレジストリ側は消えず、まだ実行されていない呼び出しでも
    // キャンセル後に実行されてしまっていた（グローバル側から拾って初めて
    // 実行される設計になったため、こちらも忘れず消す）。
    let removed_local = state.http_ctx.cancel_grpc_call(call_id as u32);
    let removed_global =
        super::grpc_executor::cancel_global_pending_grpc_call(&module_name, call_id as u32);

    if removed_local || removed_global {
        ftlog::debug!("WASM: proxy_grpc_cancel - cancelled call_id={}", call_id);
        PROXY_RESULT_OK
    } else {
        ftlog::debug!("WASM: proxy_grpc_cancel - call_id={} not found", call_id);
        PROXY_RESULT_NOT_FOUND
    }
}

#[cfg(feature = "grpc")]
fn proxy_grpc_stream_impl(
    caller: &mut Caller<'_, HostState>,
    upstream_ptr: i32,
    upstream_size: i32,
    service_ptr: i32,
    service_size: i32,
    method_ptr: i32,
    method_size: i32,
    initial_metadata_ptr: i32,
    initial_metadata_size: i32,
    return_stream_id_ptr: i32,
) -> i32 {
    use crate::wasm::context::{GrpcStream, GrpcStreamState};

    // F-134: proxy_grpc_call と同じ capability ゲート（従来は無制限だった）。
    {
        let state = caller.data();
        if !state.http_ctx.capabilities.allow_http_calls {
            ftlog::warn!(
                "[wasm:{}] gRPC stream denied: allow_http_calls=false",
                state.http_ctx.plugin_name
            );
            return PROXY_RESULT_NOT_ALLOWED;
        }
    }

    // Read upstream name
    let upstream = match read_wasm_memory(caller, upstream_ptr, upstream_size) {
        Some(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
    };

    {
        let state = caller.data();
        if !state.http_ctx.capabilities.is_upstream_allowed(&upstream) {
            ftlog::warn!(
                "[wasm:{}] gRPC stream to '{}' denied: not in allowed_upstreams",
                state.http_ctx.plugin_name,
                upstream
            );
            return PROXY_RESULT_BAD_ARGUMENT;
        }
    }

    // Read service name
    let service = match read_wasm_memory(caller, service_ptr, service_size) {
        Some(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
    };

    // Read method name
    let method = match read_wasm_memory(caller, method_ptr, method_size) {
        Some(bytes) => String::from_utf8_lossy(&bytes).to_string(),
        None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
    };

    // Read initial metadata (optional)
    let initial_metadata = if initial_metadata_size > 0 {
        match read_wasm_memory(caller, initial_metadata_ptr, initial_metadata_size) {
            Some(bytes) => deserialize_grpc_metadata(&bytes),
            None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
        }
    } else {
        Vec::new()
    };

    // Allocate stream ID
    let state = caller.data_mut();
    let stream_id = state.http_ctx.next_grpc_stream_id;
    state.http_ctx.next_grpc_stream_id += 1;

    // Create the stream
    let stream = GrpcStream {
        stream_id,
        upstream: upstream.clone(),
        service: service.clone(),
        method: method.clone(),
        state: GrpcStreamState::Open,
        pending_messages: Vec::new(),
        initial_metadata,
    };

    state
        .http_ctx
        .pending_grpc_streams
        .insert(stream_id, stream);

    // Write stream_id to return pointer
    if return_stream_id_ptr > 0 {
        if let Some(memory) = caller.get_export("memory").and_then(|e| e.into_memory()) {
            let stream_id_bytes = stream_id.to_le_bytes();
            let data = memory.data_mut(&mut *caller);
            let ptr = return_stream_id_ptr as usize;
            if ptr + 4 <= data.len() {
                data[ptr..ptr + 4].copy_from_slice(&stream_id_bytes);
            } else {
                return PROXY_RESULT_INVALID_MEMORY_ACCESS;
            }
        }
    }

    ftlog::debug!(
        "WASM: proxy_grpc_stream opened stream_id={} to {}/{}/{}",
        stream_id,
        upstream,
        service,
        method
    );
    PROXY_RESULT_OK
}

#[cfg(feature = "grpc")]
fn proxy_grpc_close_impl(caller: &mut Caller<'_, HostState>, stream_id: i32) -> i32 {
    use crate::wasm::context::GrpcStreamState;

    let module_name = caller.data().http_ctx.plugin_name.clone();
    let state = caller.data_mut();
    let stream_id_u32 = stream_id as u32;

    // ストリームがまだ tick スレッドで実行されていなければグローバル
    // レジストリからも取り除く（F-134: 実行前クローズで通信を起こさない）。
    super::grpc_executor::cancel_global_pending_grpc_call(&module_name, stream_id_u32);

    if let Some(stream) = state.http_ctx.pending_grpc_streams.get_mut(&stream_id_u32) {
        stream.state = GrpcStreamState::Closed;
        ftlog::debug!("WASM: proxy_grpc_close - closed stream_id={}", stream_id);
        PROXY_RESULT_OK
    } else {
        ftlog::debug!("WASM: proxy_grpc_close - stream_id={} not found", stream_id);
        PROXY_RESULT_NOT_FOUND
    }
}

#[cfg(feature = "grpc")]
fn proxy_grpc_send_impl(
    caller: &mut Caller<'_, HostState>,
    stream_id: i32,
    message_ptr: i32,
    message_size: i32,
    end_of_stream: i32,
) -> i32 {
    use crate::wasm::context::GrpcStreamState;

    // Read message data
    let message = if message_size > 0 {
        match read_wasm_memory(caller, message_ptr, message_size) {
            Some(bytes) => bytes,
            None => return PROXY_RESULT_INVALID_MEMORY_ACCESS,
        }
    } else {
        Vec::new()
    };

    let module_name = caller.data().http_ctx.plugin_name.clone();
    let state = caller.data_mut();
    let stream_id_u32 = stream_id as u32;

    if let Some(stream) = state.http_ctx.pending_grpc_streams.get_mut(&stream_id_u32) {
        // Check if stream is open
        if stream.state != GrpcStreamState::Open {
            ftlog::debug!(
                "WASM: proxy_grpc_send - stream_id={} is not open",
                stream_id
            );
            return PROXY_RESULT_BAD_ARGUMENT;
        }

        // Queue the message
        if !message.is_empty() {
            stream.pending_messages.push(message);
        }

        // Handle end of stream
        //
        // F-134: 従来はローカル状態を HalfClosed にするだけで、実際に
        // ネットワークへ何も送出しないまま黙って終わっていた（呼び出し元からは
        // 成功したように見えるのに proxy_on_grpc_receive/proxy_on_grpc_close が
        // 永遠に呼ばれない不適合）。ここでゲストが送信した全メッセージを
        // まとめてグローバルレジストリへ登録し、tick スレッドが 1 回の
        // HTTP/2 ストリームとして送出する（真の逐次双方向ストリーミングでは
        // ないクライアントストリーミングの簡略実装。理由・制限は
        // docs/backlog/features/F-139-wasm-grpc-call-execution.md 参照）。
        if end_of_stream != 0 {
            let path = format!("/{}/{}", stream.service, stream.method);
            super::grpc_executor::register_global_pending_grpc_call(
                super::grpc_executor::PendingGrpcUnaryCall {
                    module_name,
                    call_id: stream_id_u32,
                    upstream: stream.upstream.clone(),
                    path,
                    initial_metadata: stream.initial_metadata.clone(),
                    messages: stream.pending_messages.clone(),
                    // gRPC ストリームには proxy_grpc_call の timeout_ms 相当の
                    // パラメータが ABI に無いため、既定値を用いる。
                    timeout_ms: 30_000,
                },
            );
            stream.state = GrpcStreamState::HalfClosed;
            ftlog::debug!(
                "WASM: proxy_grpc_send - stream_id={} half-closed, queued for execution",
                stream_id
            );
        }

        ftlog::debug!(
            "WASM: proxy_grpc_send - queued {} bytes on stream_id={}",
            message_size,
            stream_id
        );
        PROXY_RESULT_OK
    } else {
        ftlog::debug!("WASM: proxy_grpc_send - stream_id={} not found", stream_id);
        PROXY_RESULT_NOT_FOUND
    }
}

/// Deserialize gRPC metadata from Proxy-Wasm format
#[cfg(feature = "grpc")]
fn deserialize_grpc_metadata(data: &[u8]) -> Vec<(String, String)> {
    if data.len() < 4 {
        return Vec::new();
    }

    let num_pairs = u32::from_le_bytes([data[0], data[1], data[2], data[3]]) as usize;
    let mut metadata = Vec::with_capacity(num_pairs);
    let mut pos = 4;

    for _ in 0..num_pairs {
        if pos + 4 > data.len() {
            break;
        }
        let key_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + key_len > data.len() {
            break;
        }
        let key = String::from_utf8_lossy(&data[pos..pos + key_len]).to_string();
        pos += key_len;

        if pos + 4 > data.len() {
            break;
        }
        let val_len =
            u32::from_le_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]) as usize;
        pos += 4;

        if pos + val_len > data.len() {
            break;
        }
        let value = String::from_utf8_lossy(&data[pos..pos + val_len]).to_string();
        pos += val_len;

        metadata.push((key, value));
    }

    metadata
}
