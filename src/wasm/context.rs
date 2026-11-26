//! HTTP Context for Proxy-Wasm
//!
//! Holds the state for a single HTTP request/response processing.

use std::collections::HashMap;
use std::sync::Arc;

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

/// 起動時に読んだファイル（名前 → 中身。F-271）
pub type PluginData = Arc<HashMap<String, Arc<[u8]>>>;

/// 空の `Arc<str>`（コンテキストを作るたびに確保しない。B-175）
fn empty_str() -> Arc<str> {
    static EMPTY: std::sync::OnceLock<Arc<str>> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| Arc::from("")).clone()
}

/// このスレッドの空の `Arc<str>`（要求の終わりに context を空へ戻すときに使う。[`empty_str`] はプロセスで 1 つなので、
/// 要求ごとに全ワーカーが同じ参照カウントを上げ下げする。F-318）
fn local_empty_str() -> Arc<str> {
    thread_local! {
        static EMPTY: Arc<str> = Arc::from("");
    }
    // スレッドの終わり（スレッドローカルの破棄の後）は共有のもの
    EMPTY.try_with(Arc::clone).unwrap_or_else(|_| empty_str())
}

/// 空の [`PluginData`]（共有。要求ごとに確保しない）
pub fn empty_plugin_data() -> PluginData {
    static EMPTY: std::sync::OnceLock<PluginData> = std::sync::OnceLock::new();
    EMPTY.get_or_init(|| Arc::new(HashMap::new())).clone()
}

/// ゲストに見せる要求の属性のうち、パス・メソッド・ヘッダの他のもの（B-408）。
///
/// `:authority`・`:scheme` の疑似ヘッダと `request.host`・`request.scheme`・`request.protocol` のプロパティになる。
/// 要求ごとの [`super::RequestSessions`] に置き、要求のすべての段の context へ写す。
#[derive(Clone)]
pub struct RequestInfo {
    /// `:authority`（HTTP/1.1 は `Host`）
    pub authority: std::sync::Arc<str>,
    /// `http` か `https`
    pub scheme: &'static str,
    /// `HTTP/1.0`・`HTTP/1.1`・`HTTP/2`・`HTTP/3`
    pub protocol: &'static str,
}

/// [`HttpContext::reset_values`] で残すもの（モジュールのものはいつも残す）
#[derive(Clone, Copy, PartialEq, Eq)]
enum Keep {
    /// 何も残さない（要求の終わり。[`HttpContext::recycle`]）
    Nothing,
    /// context の ID・要求の属性・メソッドとパス（[`HttpContext::enter_log`]）
    RequestLine,
    /// それとクライアントの IP（[`HttpContext::enter_request_body`]）
    RequestLineAndClient,
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
    /// `:authority`・`:scheme`・プロトコル（B-408。無ければ `Host` ヘッダ・`https`・`HTTP/1.1`）。
    /// `Some` の context では、ホストが要求・応答ヘッダの表の疑似ヘッダを context から作り、ゲストには書き換えさせない。
    /// `None`（メッシュのデータプレーン）は呼び出し元が表に入れた疑似ヘッダをそのまま見せ、書き換えも受け取る
    pub request_info: Option<RequestInfo>,
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
    pub plugin_name: Arc<str>,
    /// Proxy-Wasm の root_id（設定されたモジュールのみ。F-222）
    pub plugin_root_id: Option<Arc<str>>,
    /// Plugin configuration（ルート単位で実効値が解決されている場合はその値。F-148）
    pub plugin_configuration: Arc<[u8]>,
    /// VM configuration
    pub vm_configuration: Arc<[u8]>,
    /// 起動時に読んだファイル（`[[wasm.modules]] data_files`。プロパティ `plugin_data.<名前>`。F-271）
    pub plugin_data: PluginData,

    // === Modification Flags ===
    /// Request headers modified
    pub request_headers_modified: bool,
    /// Request body modified
    pub request_body_modified: bool,
    /// ゲストが要求のパス・メソッドを書き換えた（`proxy_set_property` の `request.path`・`request.method`。F-329: 要求の段を
    /// 続けて呼ばない）
    pub request_line_overridden: bool,
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
    /// `proxy_continue_stream` が呼ばれたか（B-320: Pause したフィルタが `http_call` の応答で再開を
    /// 決めたかを見る。`settle` が呼ぶ前に倒す）
    pub stream_resumed: bool,

    // === HTTP Calls ===
    /// Pending HTTP calls
    pub pending_http_calls: HashMap<u32, PendingHttpCall>,
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
    /// Shared data（モジュールの表の共有。B-179: 以前は context ごとに作り、要求をまたいで共有されなかった）
    pub shared_data: Arc<super::shared_data::SharedData>,

    // === Custom Properties ===
    /// User-defined properties (set via proxy_set_property)
    pub custom_properties: HashMap<String, Vec<u8>>,

    // === Capabilities ===
    /// Module capabilities
    pub capabilities: Arc<ModuleCapabilities>,

    // === Timer ===
    /// Tick period in milliseconds (0 = disabled)
    pub tick_period_ms: u32,

    // === gRPC Calls (feature = "grpc") ===
    /// Pending gRPC calls (call_id -> (path, message, timeout_ms))
    /// F-160: message は `Bytes`。ホスト関数側で `Vec<u8>` から 1 度だけ変換した
    /// ものを保持し、ここへ入れる際の `clone()` は参照カウントのみで完結する。
    #[cfg(feature = "grpc")]
    pub pending_grpc_calls: HashMap<u32, (String, Bytes, u32)>,
    /// Cancelled gRPC call IDs
    #[cfg(feature = "grpc")]
    pub cancelled_grpc_calls: std::collections::HashSet<u32>,

    // === gRPC Streams (feature = "grpc") ===
    /// Active gRPC streams (stream_id -> GrpcStream)
    #[cfg(feature = "grpc")]
    pub pending_grpc_streams: HashMap<u32, GrpcStream>,

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

/// HTTP の呼び出しのトークン・gRPC の呼び出しとストリームの ID（1..=i32::MAX）。
///
/// B-320: プロセスで一意にする。以前はコンテキストごとに 1 から数えていて、コールバックごとに作るコンテキストの
/// 間で同じ ID になり、gRPC の実行器の表（モジュール名・ID で引く）で同じモジュールの別の要求の呼び出しへ
/// 送ったり、取り消したり、置き換えたりできた（F48）
fn next_unique_id() -> u32 {
    static NEXT: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % (i32::MAX as u32) + 1
}

impl HttpContext {
    /// Create a new HTTP context
    ///
    /// モジュールに属さない context（試験等）。shared data は自分だけの表を作る。要求ごとの context は
    /// `for_module`（B-175: 要求ごとに作るので確保しない。権限・shared data の表は `Arc` で共有、空の文字列も共有）。
    pub fn new(context_id: i32, capabilities: impl Into<Arc<ModuleCapabilities>>) -> Self {
        Self::with_shared_data(context_id, capabilities, Arc::default())
    }

    /// モジュールの要求の context（権限と shared data の表はモジュールのものを共有する。B-179）
    pub(crate) fn for_module(context_id: i32, module: &super::registry::LoadedModule) -> Self {
        Self::with_shared_data(
            context_id,
            module.capabilities.clone(),
            module.shared_data.clone(),
        )
    }

    fn with_shared_data(
        context_id: i32,
        capabilities: impl Into<Arc<ModuleCapabilities>>,
        shared_data: Arc<super::shared_data::SharedData>,
    ) -> Self {
        let capabilities = capabilities.into();
        Self {
            context_id,
            root_context_id: 0,
            request_headers: Vec::new(),
            request_body: BodyBuffer::empty(),
            request_trailers: Vec::new(),
            request_path: empty_str(),
            request_method: empty_str(),
            request_query: String::new(),
            request_info: None,
            request_body_complete: false,
            response_status: 0,
            response_headers: Vec::new(),
            response_body: BodyBuffer::empty(),
            response_trailers: Vec::new(),
            response_body_complete: false,
            client_ip: empty_str(),
            plugin_name: empty_str(),
            plugin_root_id: None,
            plugin_configuration: crate::wasm_plugin_config::empty_configuration(),
            vm_configuration: crate::wasm_plugin_config::empty_configuration(),
            plugin_data: empty_plugin_data(),
            request_headers_modified: false,
            request_body_modified: false,
            request_line_overridden: false,
            response_headers_modified: false,
            response_body_modified: false,
            local_response: None,
            downstream_data: BodyBuffer::empty(),
            downstream_data_modified: false,
            upstream_data: BodyBuffer::empty(),
            upstream_data_modified: false,
            close_requested: false,
            stream_resumed: false,
            pending_http_calls: HashMap::new(),
            http_call_responses: HashMap::new(),
            current_http_call_token: None,
            metrics: HashMap::new(),
            next_metric_id: 1,
            shared_data,
            custom_properties: HashMap::new(),
            capabilities,
            tick_period_ms: 0,
            #[cfg(feature = "grpc")]
            pending_grpc_calls: HashMap::new(),
            #[cfg(feature = "grpc")]
            cancelled_grpc_calls: std::collections::HashSet::new(),
            #[cfg(feature = "grpc")]
            pending_grpc_streams: HashMap::new(),
            #[cfg(feature = "grpc")]
            grpc_receive_initial_metadata: Vec::new(),
            #[cfg(feature = "grpc")]
            grpc_receive_message: Bytes::new(),
            #[cfg(feature = "grpc")]
            grpc_receive_trailing_metadata: Vec::new(),
        }
    }

    /// 要求の終わりに、モジュールのもの（権限・shared data・プラグインの名前・ルート ID・設定・VM の設定・ファイル）を
    /// 残して、[`Self::for_module`] で作ったときと同じ空の状態へ戻す（F-318）。
    ///
    /// プールのインスタンスの context を要求ごとに作り直すと、モジュールの `Arc` を全部複製し直して捨てる（全ワーカーが
    /// 同じ行へ書く）。
    pub(crate) fn recycle(&mut self) {
        self.reset_values(Keep::Nothing);
    }

    /// モジュールのものと `keep` のものを残して空の状態へ戻す（[`Self::recycle`]・[`Self::enter_log`]・
    /// [`Self::enter_request_body`]）。全フィールドを列挙する（足したフィールドを戻し忘れたらコンパイルで分かる）。
    ///
    /// F-331: 残すものは触らない（以前は要求の段の値を残すときも、メソッド・パスを空の文字列と入れ替えてから全部を戻し、
    /// 元へ戻していた。入れ替えのたびに参照カウントの加算・減算になる）。空の文字列はもう空なら置き直さない
    fn reset_values(&mut self, keep: Keep) {
        let HttpContext {
            context_id,
            root_context_id,
            request_headers,
            request_body,
            request_trailers,
            request_path,
            request_method,
            request_query,
            request_info,
            request_body_complete,
            response_status,
            response_headers,
            response_body,
            response_trailers,
            response_body_complete,
            client_ip,
            plugin_name: _,
            plugin_root_id: _,
            plugin_configuration: _,
            vm_configuration: _,
            plugin_data: _,
            request_headers_modified,
            request_body_modified,
            request_line_overridden,
            response_headers_modified,
            response_body_modified,
            local_response,
            downstream_data,
            downstream_data_modified,
            upstream_data,
            upstream_data_modified,
            close_requested,
            stream_resumed,
            pending_http_calls,
            http_call_responses,
            current_http_call_token,
            metrics,
            next_metric_id,
            shared_data: _,
            custom_properties,
            capabilities: _,
            tick_period_ms,
            #[cfg(feature = "grpc")]
            pending_grpc_calls,
            #[cfg(feature = "grpc")]
            cancelled_grpc_calls,
            #[cfg(feature = "grpc")]
            pending_grpc_streams,
            #[cfg(feature = "grpc")]
            grpc_receive_initial_metadata,
            #[cfg(feature = "grpc")]
            grpc_receive_message,
            #[cfg(feature = "grpc")]
            grpc_receive_trailing_metadata,
        } = self;
        let clear_str = |s: &mut Arc<str>| {
            if !s.is_empty() {
                *s = local_empty_str();
            }
        };
        if keep == Keep::Nothing {
            *context_id = 1;
            *root_context_id = 0;
            clear_str(request_path);
            clear_str(request_method);
            request_query.clear();
            *request_info = None;
        }
        if keep != Keep::RequestLineAndClient {
            clear_str(client_ip);
        }
        request_headers.clear();
        *request_body = BodyBuffer::empty();
        request_trailers.clear();
        *request_body_complete = false;
        *response_status = 0;
        response_headers.clear();
        *response_body = BodyBuffer::empty();
        response_trailers.clear();
        *response_body_complete = false;
        *request_headers_modified = false;
        *request_body_modified = false;
        *request_line_overridden = false;
        *response_headers_modified = false;
        *response_body_modified = false;
        *local_response = None;
        *downstream_data = BodyBuffer::empty();
        *downstream_data_modified = false;
        *upstream_data = BodyBuffer::empty();
        *upstream_data_modified = false;
        *close_requested = false;
        *stream_resumed = false;
        pending_http_calls.clear();
        http_call_responses.clear();
        *current_http_call_token = None;
        metrics.clear();
        *next_metric_id = 1;
        custom_properties.clear();
        *tick_period_ms = 0;
        #[cfg(feature = "grpc")]
        {
            pending_grpc_calls.clear();
            cancelled_grpc_calls.clear();
            pending_grpc_streams.clear();
            grpc_receive_initial_metadata.clear();
            *grpc_receive_message = Bytes::new();
            grpc_receive_trailing_metadata.clear();
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
        self.set_request_line(method, path);
        self.request_headers = headers;
        self.client_ip = client_ip;
    }

    /// メソッドとパス（クエリ文字列も）を置く
    pub fn set_request_line(&mut self, method: std::sync::Arc<str>, path: std::sync::Arc<str>) {
        // Extract query string（プールの context の確保を使い回す。F-319）
        self.request_query.clear();
        if let Some(pos) = path.find('?') {
            self.request_query.push_str(&path[pos + 1..]);
        }
        self.request_method = method;
        self.request_path = path;
    }

    /// `:authority`（B-408）。要求の属性が無ければ表の `:authority`（メッシュのデータプレーンが入れる）か `Host` ヘッダ
    pub fn request_authority(&self) -> Option<&[u8]> {
        match &self.request_info {
            Some(info) => Some(info.authority.as_bytes()),
            None => self
                .request_headers
                .iter()
                .find(|(k, _)| k == b":authority")
                .or_else(|| {
                    self.request_headers
                        .iter()
                        .find(|(k, _)| k.eq_ignore_ascii_case(b"host"))
                })
                .map(|(_, v)| v.as_slice()),
        }
    }

    /// `:scheme`（B-408）。要求の属性が無ければ表の `:scheme` が `http` のときだけ `http`
    pub fn request_scheme(&self) -> &'static str {
        match &self.request_info {
            Some(info) => info.scheme,
            None if self
                .request_headers
                .iter()
                .any(|(k, v)| k == b":scheme" && v == b"http") =>
            {
                "http"
            }
            None => "https",
        }
    }

    /// `request.protocol`（B-408）
    pub fn request_protocol(&self) -> &'static str {
        self.request_info
            .as_ref()
            .map_or("HTTP/1.1", |i| i.protocol)
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
        next_unique_id()
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

    /// まだ解決していない外部呼び出し（`proxy_http_call`・gRPC の呼び出しとストリーム）があるか（F-329。続けて呼ぶ段の条件）
    pub(crate) fn has_pending_calls(&self) -> bool {
        #[cfg(feature = "grpc")]
        if !self.pending_grpc_calls.is_empty() || !self.pending_grpc_streams.is_empty() {
            return true;
        }
        !self.pending_http_calls.is_empty()
    }

    /// 要求ヘッダの段に続けて呼ぶ要求本文の段の context にする（F-329）。本文の段を別に呼んでいたときと同じ見え方:
    /// ヘッダの段で置いた値は消し、要求の属性・メソッド・パス・クライアントの IP を残して、`headers`（ヘッダの段の後の
    /// ヘッダ）と全量の本文を置く
    pub(crate) fn enter_request_body(&mut self, headers: Vec<(Vec<u8>, Vec<u8>)>, body: Bytes) {
        self.reset_values(Keep::RequestLineAndClient);
        self.request_headers = headers;
        self.set_request_body(body, true);
    }

    /// ローカル応答の要求の `proxy_on_log` の前に、context を応答の後に別に呼んでいたログの段と同じ見え方にする（F-329）。
    /// 要求の段の値（ヘッダ・本文・トレーラ・クライアントの IP・ゲストが置いたプロパティ・メトリクス・ローカル応答等）を
    /// 消し、context の ID・要求の属性・メソッドとパス（[`HttpContext::set_request_line`]）だけを残す（応答のステータスも
    /// 別に呼んでいたときと同じく 0。段の間の `recycle` とセッションが入れ直す要求の属性〈`set_request_attrs`〉の後と同じ）
    pub(crate) fn enter_log(&mut self) {
        self.reset_values(Keep::RequestLine);
    }

    /// Get next gRPC call ID
    #[cfg(feature = "grpc")]
    pub fn next_grpc_call_id(&mut self) -> u32 {
        next_unique_id()
    }

    /// Get next gRPC stream ID
    #[cfg(feature = "grpc")]
    pub fn next_grpc_stream_id(&mut self) -> u32 {
        next_unique_id()
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
    /// インスタンスのエクスポートの関数（引いたものをインスタンスの寿命の間使い回す。F-318）
    pub(crate) exports: super::exports::GuestExports,
    /// CPU の上限・yield の計量（`[wasm] metering = "clock"`。F-319）
    pub(crate) meter: super::metering::Meter,
}

impl HostState {
    /// Create a new host state
    pub fn new(http_ctx: HttpContext) -> Self {
        Self {
            http_ctx,
            exports: Default::default(),
            meter: Default::default(),
        }
    }
}
