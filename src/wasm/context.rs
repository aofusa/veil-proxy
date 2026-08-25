//! HTTP Context for Proxy-Wasm
//!
//! Holds the state for a single HTTP request/response processing.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use bytes::Bytes;

use super::capabilities::ModuleCapabilities;
use super::types::{HttpCallResponse, LocalResponse, Metric, PendingHttpCall};

/// ボディの Copy-on-Write バッファ（F-61）
///
/// ホスト側からは参照カウント共有の `Bytes` をゼロコピーで受け取り（`Shared`）、
/// WASM モジュールがボディを書き換えた時のみ所有 `Vec<u8>` へ昇格する（`Owned`）。
/// 読み取りのみのモジュール（大多数）ではボディの deep copy が一切発生しない。
pub enum BodyBuffer {
    /// ホストと共有する不変ボディ（読み取り専用、O(1) クローン）
    Shared(Bytes),
    /// モジュールが書き換えた後の所有ボディ
    Owned(Vec<u8>),
}

impl BodyBuffer {
    /// 空のバッファ
    pub fn empty() -> Self {
        BodyBuffer::Shared(Bytes::new())
    }

    /// バイトスライスとして参照
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        match self {
            BodyBuffer::Shared(b) => b,
            BodyBuffer::Owned(v) => v,
        }
    }

    #[inline]
    pub fn len(&self) -> usize {
        self.as_slice().len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }

    /// 可変参照を取得する（CoW: `Shared` は初回書き込み時のみコピーして `Owned` へ昇格）
    pub fn to_mut(&mut self) -> &mut Vec<u8> {
        if let BodyBuffer::Shared(b) = self {
            *self = BodyBuffer::Owned(b.to_vec());
        }
        match self {
            BodyBuffer::Owned(v) => v,
            BodyBuffer::Shared(_) => unreachable!("converted to Owned above"),
        }
    }

    /// `Bytes` として共有取得（`Shared` は O(1)、`Owned` はコピー。
    /// `Owned` になるのはモジュールが書き換えた後のみで稀）
    pub fn share(&self) -> Bytes {
        match self {
            BodyBuffer::Shared(b) => b.clone(),
            BodyBuffer::Owned(v) => Bytes::copy_from_slice(v),
        }
    }

    /// `Bytes` へ変換（`Shared` はそのまま、`Owned` はムーブ。いずれもコピーなし）
    pub fn into_bytes(self) -> Bytes {
        match self {
            BodyBuffer::Shared(b) => b,
            BodyBuffer::Owned(v) => Bytes::from(v),
        }
    }
}

impl Default for BodyBuffer {
    fn default() -> Self {
        Self::empty()
    }
}

/// HTTP context for a single request
pub struct HttpContext {
    // === Context IDs ===
    /// Unique context ID
    pub context_id: i32,
    /// Root context ID
    pub root_context_id: i32,

    // === Request ===
    /// Request headers (binary)
    pub request_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Request body（F-61: CoW バッファ。共有 Bytes で受け取り書き換え時のみ所有化）
    pub request_body: BodyBuffer,
    /// Request trailers (binary)
    pub request_trailers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Request path
    pub request_path: std::sync::Arc<str>,
    /// Request method
    pub request_method: std::sync::Arc<str>,
    /// Request query string
    pub request_query: String,
    /// Is request body complete
    pub request_body_complete: bool,

    // === Response ===
    /// Response status code
    pub response_status: u16,
    /// Response headers (binary)
    pub response_headers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Response body（F-61: CoW バッファ）
    pub response_body: BodyBuffer,
    /// Response trailers (binary)
    pub response_trailers: Vec<(Vec<u8>, Vec<u8>)>,
    /// Is response body complete
    pub response_body_complete: bool,

    // === Metadata ===
    /// Client IP address
    pub client_ip: std::sync::Arc<str>,
    /// Plugin name
    pub plugin_name: String,
    /// Plugin configuration（ルート単位で実効値が解決されている場合はその値。F-148）
    pub plugin_configuration: Arc<[u8]>,
    /// VM configuration
    pub vm_configuration: Arc<[u8]>,

    // === Modification Flags ===
    /// Request headers modified
    pub request_headers_modified: bool,
    /// Request body modified
    pub request_body_modified: bool,
    /// Response headers modified
    pub response_headers_modified: bool,
    /// Response body modified
    pub response_body_modified: bool,

    // === Local Response ===
    /// Local response to send (if set)
    pub local_response: Option<LocalResponse>,

    // === Network Filter (L4, F-133) ===
    /// Downstream (client → proxy) connection data（`BufferType::DownstreamData=2`）。
    /// HTTP コンテキストでは未使用（空のまま = 追加コストなし）。
    pub downstream_data: BodyBuffer,
    /// Downstream data がモジュールにより書き換えられたか。
    pub downstream_data_modified: bool,
    /// Upstream (proxy → backend) connection data（`BufferType::UpstreamData=3`）。
    pub upstream_data: BodyBuffer,
    /// Upstream data がモジュールにより書き換えられたか。
    pub upstream_data_modified: bool,
    /// `proxy_close_stream` が呼ばれ、接続クローズが要求されたか（network filter 専用。
    /// Proxy-Wasm ABI の Action には Close が無いため、host 関数呼び出しで検知する）。
    pub close_requested: bool,

    // === HTTP Calls ===
    /// Pending HTTP calls
    pub pending_http_calls: HashMap<u32, PendingHttpCall>,
    /// Next HTTP call token
    pub next_http_call_token: u32,
    /// HTTP call responses (token -> response)
    pub http_call_responses: HashMap<u32, HttpCallResponse>,
    /// Current HTTP call token being processed
    pub current_http_call_token: Option<u32>,

    // === Metrics ===
    /// Defined metrics (id -> metric)
    pub metrics: HashMap<i32, Metric>,
    /// Next metric ID
    pub next_metric_id: i32,

    // === Shared Data ===
    /// Shared data (key -> (value, cas))
    pub shared_data: Arc<RwLock<HashMap<String, (Vec<u8>, u32)>>>,
    /// Next CAS value
    pub shared_data_cas: u32,

    // === Custom Properties ===
    /// User-defined properties (set via proxy_set_property)
    pub custom_properties: HashMap<String, Vec<u8>>,

    // === Capabilities ===
    /// Module capabilities
    pub capabilities: ModuleCapabilities,

    // === Timer ===
    /// Tick period in milliseconds (0 = disabled)
    pub tick_period_ms: u32,

    // === gRPC Calls (feature = "grpc") ===
    /// Pending gRPC calls (call_id -> (path, message, timeout_ms))
    /// F-160: message は `Bytes`。ホスト関数側で `Vec<u8>` から 1 度だけ変換した
    /// ものを保持し、ここへ入れる際の `clone()` は参照カウントのみで完結する。
    #[cfg(feature = "grpc")]
    pub pending_grpc_calls: HashMap<u32, (String, Bytes, u32)>,
    /// Next gRPC call ID
    #[cfg(feature = "grpc")]
    pub next_grpc_call_id: u32,
    /// Cancelled gRPC call IDs
    #[cfg(feature = "grpc")]
    pub cancelled_grpc_calls: std::collections::HashSet<u32>,

    // === gRPC Streams (feature = "grpc") ===
    /// Active gRPC streams (stream_id -> GrpcStream)
    #[cfg(feature = "grpc")]
    pub pending_grpc_streams: HashMap<u32, GrpcStream>,
    /// Next gRPC stream ID
    #[cfg(feature = "grpc")]
    pub next_grpc_stream_id: u32,

    // === gRPC Receive State (feature = "grpc", F-134) ===
    // `MapType::GrpcReceiveInitialMetadata`(4) / `GrpcReceiveTrailingMetadata`(5) /
    // `BufferType::GrpcReceiveBuffer`(5) のバックストア。`proxy_on_grpc_receive*`
    // コールバック実行前に engine.rs がここへ書き込み、ゲストが
    // `proxy_get_header_map_pairs`/`proxy_get_buffer_bytes` で読み出せるようにする
    // （従来は認識されない MapType/BufferType として BadArgument になっていた）。
    /// 直近の gRPC 受信初期メタデータ
    #[cfg(feature = "grpc")]
    pub grpc_receive_initial_metadata: Vec<(Vec<u8>, Vec<u8>)>,
    /// 直近の gRPC 受信メッセージ本体
    #[cfg(feature = "grpc")]
    pub grpc_receive_message: Bytes,
    /// 直近の gRPC 受信トレーリングメタデータ
    #[cfg(feature = "grpc")]
    pub grpc_receive_trailing_metadata: Vec<(Vec<u8>, Vec<u8>)>,
}

/// gRPC stream state
#[cfg(feature = "grpc")]
#[derive(Debug, Clone)]
pub struct GrpcStream {
    /// Stream ID
    pub stream_id: u32,
    /// Upstream service name
    pub upstream: String,
    /// gRPC service name
    pub service: String,
    /// gRPC method name
    pub method: String,
    /// Stream state
    pub state: GrpcStreamState,
    /// Pending messages to send
    /// F-160: `Bytes` にして half-close 時の一括送出（`clone()`）を
    /// 参照カウントのみで完結させる。
    pub pending_messages: Vec<Bytes>,
    /// Initial metadata（直列化バイト列のまま保持。F-160）
    /// `GrpcMetadataBlob` はクレート内部専用の型のため、フィールドも
    /// `pub(crate)` にする（`GrpcStream` は crate 内でのみ組み立てられる）。
    pub(crate) initial_metadata: super::host::grpc::GrpcMetadataBlob,
}

/// gRPC stream state
#[cfg(feature = "grpc")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GrpcStreamState {
    /// Stream is open for bidirectional communication
    Open,
    /// Client has closed their send side (half-closed)
    HalfClosed,
    /// Stream is fully closed
    Closed,
}

impl HttpContext {
    /// Create a new HTTP context
    pub fn new(context_id: i32, capabilities: ModuleCapabilities) -> Self {
        Self {
            context_id,
            root_context_id: 0,
            request_headers: Vec::new(),
            request_body: BodyBuffer::empty(),
            request_trailers: Vec::new(),
            request_path: std::sync::Arc::from(""),
            request_method: std::sync::Arc::from(""),
            request_query: String::new(),
            request_body_complete: false,
            response_status: 0,
            response_headers: Vec::new(),
            response_body: BodyBuffer::empty(),
            response_trailers: Vec::new(),
            response_body_complete: false,
            client_ip: std::sync::Arc::from(""),
            plugin_name: String::new(),
            plugin_configuration: crate::wasm_plugin_config::empty_configuration(),
            vm_configuration: crate::wasm_plugin_config::empty_configuration(),
            request_headers_modified: false,
            request_body_modified: false,
            response_headers_modified: false,
            response_body_modified: false,
            local_response: None,
            downstream_data: BodyBuffer::empty(),
            downstream_data_modified: false,
            upstream_data: BodyBuffer::empty(),
            upstream_data_modified: false,
            close_requested: false,
            pending_http_calls: HashMap::new(),
            next_http_call_token: 1,
            http_call_responses: HashMap::new(),
            current_http_call_token: None,
            metrics: HashMap::new(),
            next_metric_id: 1,
            shared_data: Arc::new(RwLock::new(HashMap::new())),
            shared_data_cas: 1,
            custom_properties: HashMap::new(),
            capabilities,
            tick_period_ms: 0,
            #[cfg(feature = "grpc")]
            pending_grpc_calls: HashMap::new(),
            #[cfg(feature = "grpc")]
            next_grpc_call_id: 1,
            #[cfg(feature = "grpc")]
            cancelled_grpc_calls: std::collections::HashSet::new(),
            #[cfg(feature = "grpc")]
            pending_grpc_streams: HashMap::new(),
            #[cfg(feature = "grpc")]
            next_grpc_stream_id: 1,
            #[cfg(feature = "grpc")]
            grpc_receive_initial_metadata: Vec::new(),
            #[cfg(feature = "grpc")]
            grpc_receive_message: Bytes::new(),
            #[cfg(feature = "grpc")]
            grpc_receive_trailing_metadata: Vec::new(),
        }
    }

    /// Set request data
    ///
    /// F-43: 文字列は `Arc<str>` 共有・ヘッダは所有権ムーブで受け取り、
    /// per-module の deep copy を排除する。
    pub fn set_request(
        &mut self,
        method: std::sync::Arc<str>,
        path: std::sync::Arc<str>,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        client_ip: std::sync::Arc<str>,
    ) {
        // Extract query string
        if let Some(pos) = path.find('?') {
            self.request_query = path[pos + 1..].to_string();
        }
        self.request_method = method;
        self.request_path = path;
        self.request_headers = headers;
        self.client_ip = client_ip;
    }

    /// Set request body（F-61: 共有 `Bytes` をゼロコピーで受け取る）
    pub fn set_request_body(&mut self, body: Bytes, complete: bool) {
        self.request_body = BodyBuffer::Shared(body);
        self.request_body_complete = complete;
    }

    /// Set response data
    pub fn set_response(&mut self, status: u16, headers: Vec<(Vec<u8>, Vec<u8>)>) {
        self.response_status = status;
        self.response_headers = headers;
    }

    /// Set response body（F-61: 共有 `Bytes` をゼロコピーで受け取る）
    pub fn set_response_body(&mut self, body: Bytes, complete: bool) {
        self.response_body = BodyBuffer::Shared(body);
        self.response_body_complete = complete;
    }

    /// Get next HTTP call token
    pub fn allocate_http_call_token(&mut self) -> u32 {
        let token = self.next_http_call_token;
        self.next_http_call_token += 1;
        token
    }

    /// Get next metric ID
    pub fn allocate_metric_id(&mut self) -> i32 {
        let id = self.next_metric_id;
        self.next_metric_id += 1;
        id
    }

    /// Check if request headers are modified
    pub fn has_request_modifications(&self) -> bool {
        self.request_headers_modified || self.request_body_modified
    }

    /// Check if response headers are modified
    pub fn has_response_modifications(&self) -> bool {
        self.response_headers_modified || self.response_body_modified
    }

    /// Check if local response should be sent
    pub fn should_send_local_response(&self) -> bool {
        self.local_response.is_some()
    }

    /// Get next gRPC call ID
    #[cfg(feature = "grpc")]
    pub fn next_grpc_call_id(&mut self) -> u32 {
        let id = self.next_grpc_call_id;
        self.next_grpc_call_id += 1;
        id
    }

    /// Register a pending gRPC call
    #[cfg(feature = "grpc")]
    pub fn register_grpc_call(
        &mut self,
        call_id: u32,
        path: String,
        message: Bytes,
        timeout_ms: u32,
    ) {
        self.pending_grpc_calls
            .insert(call_id, (path, message, timeout_ms));
    }

    /// Cancel a gRPC call
    #[cfg(feature = "grpc")]
    pub fn cancel_grpc_call(&mut self, call_id: u32) -> bool {
        if self.pending_grpc_calls.remove(&call_id).is_some() {
            self.cancelled_grpc_calls.insert(call_id);
            true
        } else {
            false
        }
    }

    /// Take pending HTTP calls for execution
    pub fn take_pending_http_calls(&mut self) -> HashMap<u32, crate::wasm::types::PendingHttpCall> {
        std::mem::take(&mut self.pending_http_calls)
    }

    /// Check if there are pending HTTP calls
    pub fn has_pending_http_calls(&self) -> bool {
        !self.pending_http_calls.is_empty()
    }

    /// Take pending gRPC calls for execution
    #[cfg(feature = "grpc")]
    pub fn take_pending_grpc_calls(&mut self) -> HashMap<u32, (String, Bytes, u32)> {
        std::mem::take(&mut self.pending_grpc_calls)
    }
}

/// Host state for Wasmtime
pub struct HostState {
    /// HTTP context
    pub http_ctx: HttpContext,
}

impl HostState {
    /// Create a new host state
    pub fn new(http_ctx: HttpContext) -> Self {
        Self { http_ctx }
    }
}
