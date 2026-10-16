//! Envoy の xDS v3 のうち、Istio の istiod が生成する設定を読むのに要るメッセージの部分集合（F-222）。
//!
//! `vds_grpc` と同じく prost の derive で手書きする（veil のビルドに protoc と Envoy の proto 一式を要求しない）。
//! フィールド番号は Envoy の data-plane-api（envoy-types 0.7.7 の生成物で照合）と同じ。使わないフィールドは
//! 定義しない（prost は未知のフィールドを読み飛ばす）。

use std::collections::HashMap;

// ---------------------------------------------------------------------------
// google.protobuf / google.rpc
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct Any {
    #[prost(string, tag = "1")]
    pub type_url: String,
    #[prost(bytes = "vec", tag = "2")]
    pub value: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Duration {
    #[prost(int64, tag = "1")]
    pub seconds: i64,
    #[prost(int32, tag = "2")]
    pub nanos: i32,
}

impl Duration {
    pub fn to_std(&self) -> std::time::Duration {
        if self.seconds < 0 {
            return std::time::Duration::ZERO;
        }
        std::time::Duration::new(self.seconds as u64, self.nanos.max(0) as u32)
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UInt32Value {
    #[prost(uint32, tag = "1")]
    pub value: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct BoolValue {
    #[prost(bool, tag = "1")]
    pub value: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Struct {
    #[prost(map = "string, message", tag = "1")]
    pub fields: HashMap<String, Value>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Value {
    #[prost(oneof = "value::Kind", tags = "1, 2, 3, 4, 5, 6")]
    pub kind: Option<value::Kind>,
}

pub mod value {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(int32, tag = "1")]
        NullValue(i32),
        #[prost(double, tag = "2")]
        NumberValue(f64),
        #[prost(string, tag = "3")]
        StringValue(String),
        #[prost(bool, tag = "4")]
        BoolValue(bool),
        #[prost(message, tag = "5")]
        StructValue(super::Struct),
        #[prost(message, tag = "6")]
        ListValue(super::ListValue),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListValue {
    #[prost(message, repeated, tag = "1")]
    pub values: Vec<Value>,
}

impl Value {
    pub fn string(s: impl Into<String>) -> Self {
        Value {
            kind: Some(value::Kind::StringValue(s.into())),
        }
    }
    pub fn as_str(&self) -> Option<&str> {
        match &self.kind {
            Some(value::Kind::StringValue(s)) => Some(s),
            _ => None,
        }
    }
    pub fn as_struct(&self) -> Option<&Struct> {
        match &self.kind {
            Some(value::Kind::StructValue(s)) => Some(s),
            _ => None,
        }
    }
    pub fn as_list(&self) -> Option<&ListValue> {
        match &self.kind {
            Some(value::Kind::ListValue(l)) => Some(l),
            _ => None,
        }
    }
    pub fn as_bool(&self) -> Option<bool> {
        match &self.kind {
            Some(value::Kind::BoolValue(b)) => Some(*b),
            _ => None,
        }
    }
    pub fn as_f64(&self) -> Option<f64> {
        match &self.kind {
            Some(value::Kind::NumberValue(n)) => Some(*n),
            _ => None,
        }
    }
}

impl Struct {
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.fields.get(key).and_then(|v| v.as_str())
    }
    pub fn get_struct(&self, key: &str) -> Option<&Struct> {
        self.fields.get(key).and_then(|v| v.as_struct())
    }
}

/// `google.rpc.Status`（NACK の `error_detail`）。
#[derive(Clone, PartialEq, prost::Message)]
pub struct Status {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

/// `udpa.type.v1.TypedStruct` / `xds.type.v3.TypedStruct`（Istio の拡張フィルタの設定）。
#[derive(Clone, PartialEq, prost::Message)]
pub struct TypedStruct {
    #[prost(string, tag = "1")]
    pub type_url: String,
    #[prost(message, optional, tag = "2")]
    pub value: Option<Struct>,
}

// ---------------------------------------------------------------------------
// envoy.service.discovery.v3
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeltaDiscoveryRequest {
    #[prost(message, optional, tag = "1")]
    pub node: Option<Node>,
    #[prost(string, tag = "2")]
    pub type_url: String,
    #[prost(string, repeated, tag = "3")]
    pub resource_names_subscribe: Vec<String>,
    #[prost(string, repeated, tag = "4")]
    pub resource_names_unsubscribe: Vec<String>,
    #[prost(map = "string, string", tag = "5")]
    pub initial_resource_versions: HashMap<String, String>,
    #[prost(string, tag = "6")]
    pub response_nonce: String,
    #[prost(message, optional, tag = "7")]
    pub error_detail: Option<Status>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DeltaDiscoveryResponse {
    #[prost(string, tag = "1")]
    pub system_version_info: String,
    #[prost(message, repeated, tag = "2")]
    pub resources: Vec<Resource>,
    #[prost(string, tag = "4")]
    pub type_url: String,
    #[prost(string, repeated, tag = "6")]
    pub removed_resources: Vec<String>,
    #[prost(string, tag = "5")]
    pub nonce: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Resource {
    #[prost(string, tag = "3")]
    pub name: String,
    #[prost(string, repeated, tag = "4")]
    pub aliases: Vec<String>,
    #[prost(string, tag = "1")]
    pub version: String,
    #[prost(message, optional, tag = "2")]
    pub resource: Option<Any>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DiscoveryRequest {
    #[prost(string, tag = "1")]
    pub version_info: String,
    #[prost(message, optional, tag = "2")]
    pub node: Option<Node>,
    #[prost(string, repeated, tag = "3")]
    pub resource_names: Vec<String>,
    #[prost(string, tag = "4")]
    pub type_url: String,
    #[prost(string, tag = "5")]
    pub response_nonce: String,
    #[prost(message, optional, tag = "6")]
    pub error_detail: Option<Status>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DiscoveryResponse {
    #[prost(string, tag = "1")]
    pub version_info: String,
    #[prost(message, repeated, tag = "2")]
    pub resources: Vec<Any>,
    #[prost(string, tag = "4")]
    pub type_url: String,
    #[prost(string, tag = "5")]
    pub nonce: String,
}

// ---------------------------------------------------------------------------
// envoy.config.core.v3
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct Node {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub cluster: String,
    #[prost(message, optional, tag = "3")]
    pub metadata: Option<Struct>,
    #[prost(message, optional, tag = "4")]
    pub locality: Option<Locality>,
    #[prost(string, tag = "6")]
    pub user_agent_name: String,
    #[prost(string, tag = "7")]
    pub user_agent_version: String,
    #[prost(string, repeated, tag = "10")]
    pub client_features: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Locality {
    #[prost(string, tag = "1")]
    pub region: String,
    #[prost(string, tag = "2")]
    pub zone: String,
    #[prost(string, tag = "3")]
    pub sub_zone: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Metadata {
    #[prost(map = "string, message", tag = "1")]
    pub filter_metadata: HashMap<String, Struct>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Address {
    #[prost(oneof = "address::Address", tags = "1, 2, 3")]
    pub address: Option<address::Address>,
}

pub mod address {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Address {
        #[prost(message, tag = "1")]
        SocketAddress(super::SocketAddress),
        #[prost(message, tag = "2")]
        Pipe(super::Pipe),
        /// Envoy の内部リスナー（ambient の waypoint）
        #[prost(message, tag = "3")]
        EnvoyInternalAddress(super::EnvoyInternalAddress),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EnvoyInternalAddress {
    #[prost(string, tag = "1")]
    pub server_listener_name: String,
    #[prost(string, tag = "2")]
    pub endpoint_id: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SocketAddress {
    #[prost(int32, tag = "1")]
    pub protocol: i32,
    #[prost(string, tag = "2")]
    pub address: String,
    #[prost(uint32, tag = "3")]
    pub port_value: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Pipe {
    #[prost(string, tag = "1")]
    pub path: String,
}

impl Address {
    /// `SocketAddress` を `std::net::SocketAddr` にする（IP リテラルのみ。名前は扱わない）。
    pub fn socket_addr(&self) -> Option<std::net::SocketAddr> {
        match &self.address {
            Some(address::Address::SocketAddress(sa)) => {
                let ip: std::net::IpAddr = sa.address.parse().ok()?;
                Some(std::net::SocketAddr::new(ip, sa.port_value as u16))
            }
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TransportSocket {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "3")]
    pub typed_config: Option<Any>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DataSource {
    #[prost(oneof = "data_source::Specifier", tags = "1, 2, 3, 4")]
    pub specifier: Option<data_source::Specifier>,
}

pub mod data_source {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Specifier {
        #[prost(string, tag = "1")]
        Filename(String),
        #[prost(bytes = "vec", tag = "2")]
        InlineBytes(Vec<u8>),
        #[prost(string, tag = "3")]
        InlineString(String),
        #[prost(string, tag = "4")]
        EnvironmentVariable(String),
    }
}

impl DataSource {
    /// インラインのデータ（ファイル参照は `None`）。
    pub fn inline(&self) -> Option<&[u8]> {
        match &self.specifier {
            Some(data_source::Specifier::InlineBytes(b)) => Some(b),
            Some(data_source::Specifier::InlineString(s)) => Some(s.as_bytes()),
            _ => None,
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HeaderValue {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
    #[prost(bytes = "vec", tag = "3")]
    pub raw_value: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HeaderValueOption {
    #[prost(message, optional, tag = "1")]
    pub header: Option<HeaderValue>,
    #[prost(message, optional, tag = "2")]
    pub append: Option<BoolValue>,
    /// 0 = APPEND_IF_EXISTS_OR_ADD, 1 = ADD_IF_ABSENT, 2 = OVERWRITE_IF_EXISTS_OR_ADD, 3 = OVERWRITE_IF_EXISTS
    #[prost(int32, tag = "3")]
    pub append_action: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CidrRange {
    #[prost(string, tag = "1")]
    pub address_prefix: String,
    #[prost(message, optional, tag = "2")]
    pub prefix_len: Option<UInt32Value>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct BindConfig {
    #[prost(message, optional, tag = "1")]
    pub source_address: Option<SocketAddress>,
}

/// `envoy.type.v3.FractionalPercent`
#[derive(Clone, PartialEq, prost::Message)]
pub struct FractionalPercent {
    #[prost(uint32, tag = "1")]
    pub numerator: u32,
    /// 0 = HUNDRED, 1 = TEN_THOUSAND, 2 = MILLION
    #[prost(int32, tag = "2")]
    pub denominator: i32,
}

impl FractionalPercent {
    /// 百万分率にする。
    pub fn per_million(&self) -> u32 {
        let n = self.numerator as u64;
        let v = match self.denominator {
            1 => n * 100,
            2 => n,
            _ => n * 10_000,
        };
        v.min(1_000_000) as u32
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RuntimeFractionalPercent {
    #[prost(message, optional, tag = "1")]
    pub default_value: Option<FractionalPercent>,
}

// ---------------------------------------------------------------------------
// envoy.config.listener.v3
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct Listener {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub address: Option<Address>,
    #[prost(message, repeated, tag = "3")]
    pub filter_chains: Vec<FilterChain>,
    #[prost(message, optional, tag = "4")]
    pub use_original_dst: Option<BoolValue>,
    #[prost(message, optional, tag = "25")]
    pub default_filter_chain: Option<FilterChain>,
    #[prost(message, repeated, tag = "9")]
    pub listener_filters: Vec<ListenerFilter>,
    #[prost(message, optional, tag = "15")]
    pub listener_filters_timeout: Option<Duration>,
    #[prost(bool, tag = "17")]
    pub continue_on_listener_filters_timeout: bool,
    /// 0 = UNSPECIFIED, 1 = INBOUND, 2 = OUTBOUND
    #[prost(int32, tag = "16")]
    pub traffic_direction: i32,
    #[prost(message, optional, tag = "26")]
    pub bind_to_port: Option<BoolValue>,
    /// 内部リスナー（`listener_specifier` の `internal_listener`）
    #[prost(message, optional, tag = "27")]
    pub internal_listener: Option<Empty>,
    /// フィルタチェーンの選択の matcher（`filter_chain_matcher`。ambient の waypoint）
    #[prost(message, optional, tag = "32")]
    pub filter_chain_matcher: Option<XdsMatcher>,
}

// ---------------------------------------------------------------------------
// xds.type.matcher.v3（フィルタチェーンの選択に使う形だけ）
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct XdsMatcher {
    #[prost(oneof = "xds_matcher::MatcherType", tags = "1, 2")]
    pub matcher_type: Option<xds_matcher::MatcherType>,
    #[prost(message, optional, boxed, tag = "3")]
    pub on_no_match: Option<Box<XdsOnMatch>>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct XdsOnMatch {
    #[prost(oneof = "xds_matcher::OnMatch", tags = "1, 2")]
    pub on_match: Option<xds_matcher::OnMatch>,
    #[prost(bool, tag = "3")]
    pub keep_matching: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct XdsMatcherTree {
    #[prost(message, optional, tag = "1")]
    pub input: Option<TypedExtensionConfig>,
    #[prost(oneof = "xds_matcher::TreeType", tags = "2, 3, 4")]
    pub tree_type: Option<xds_matcher::TreeType>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct XdsMatchMap {
    #[prost(map = "string, message", tag = "1")]
    pub map: HashMap<String, XdsOnMatch>,
}

/// `xds.type.matcher.v3.IPMatcher`
#[derive(Clone, PartialEq, prost::Message)]
pub struct XdsIpMatcher {
    #[prost(message, repeated, tag = "1")]
    pub range_matchers: Vec<XdsIpRangeMatcher>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct XdsIpRangeMatcher {
    #[prost(message, repeated, tag = "1")]
    pub ranges: Vec<CidrRange>,
    #[prost(message, optional, tag = "2")]
    pub on_match: Option<XdsOnMatch>,
    #[prost(bool, tag = "3")]
    pub exclusive: bool,
}

pub mod xds_matcher {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum MatcherType {
        /// `matcher_list`（使わない。来たら変換でエラーにする）
        #[prost(message, tag = "1")]
        MatcherList(super::Empty),
        #[prost(message, tag = "2")]
        MatcherTree(super::XdsMatcherTree),
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum OnMatch {
        #[prost(message, tag = "1")]
        Matcher(Box<super::XdsMatcher>),
        #[prost(message, tag = "2")]
        Action(super::TypedExtensionConfig),
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum TreeType {
        #[prost(message, tag = "2")]
        ExactMatchMap(super::XdsMatchMap),
        #[prost(message, tag = "3")]
        PrefixMatchMap(super::XdsMatchMap),
        #[prost(message, tag = "4")]
        CustomMatch(super::TypedExtensionConfig),
    }
}

/// `envoy.extensions.transport_sockets.internal_upstream.v3.InternalUpstreamTransport`
#[derive(Clone, PartialEq, prost::Message)]
pub struct InternalUpstreamTransport {
    #[prost(message, optional, tag = "3")]
    pub transport_socket: Option<TransportSocket>,
}

/// `Cluster.OriginalDstLbConfig`
#[derive(Clone, PartialEq, prost::Message)]
pub struct OriginalDstLbConfig {
    #[prost(bool, tag = "1")]
    pub use_http_header: bool,
    #[prost(string, tag = "2")]
    pub http_header_name: String,
    #[prost(message, optional, tag = "3")]
    pub upstream_port_override: Option<UInt32Value>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FilterChain {
    #[prost(message, optional, tag = "1")]
    pub filter_chain_match: Option<FilterChainMatch>,
    #[prost(message, repeated, tag = "3")]
    pub filters: Vec<Filter>,
    #[prost(message, optional, tag = "5")]
    pub metadata: Option<Metadata>,
    #[prost(message, optional, tag = "6")]
    pub transport_socket: Option<TransportSocket>,
    #[prost(string, tag = "7")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FilterChainMatch {
    #[prost(message, optional, tag = "8")]
    pub destination_port: Option<UInt32Value>,
    #[prost(message, repeated, tag = "3")]
    pub prefix_ranges: Vec<CidrRange>,
    #[prost(message, repeated, tag = "6")]
    pub source_prefix_ranges: Vec<CidrRange>,
    #[prost(uint32, repeated, packed = "false", tag = "7")]
    pub source_ports: Vec<u32>,
    #[prost(string, repeated, tag = "11")]
    pub server_names: Vec<String>,
    #[prost(string, tag = "9")]
    pub transport_protocol: String,
    #[prost(string, repeated, tag = "10")]
    pub application_protocols: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Filter {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "4")]
    pub typed_config: Option<Any>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ListenerFilter {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "3")]
    pub typed_config: Option<Any>,
}

// ---------------------------------------------------------------------------
// envoy.config.cluster.v3 / endpoint.v3
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct Cluster {
    #[prost(string, tag = "1")]
    pub name: String,
    /// 0 = STATIC, 1 = STRICT_DNS, 2 = LOGICAL_DNS, 3 = EDS, 4 = ORIGINAL_DST
    #[prost(int32, tag = "2")]
    pub r#type: i32,
    #[prost(message, optional, tag = "3")]
    pub eds_cluster_config: Option<EdsClusterConfig>,
    #[prost(message, optional, tag = "4")]
    pub connect_timeout: Option<Duration>,
    /// 0 = ROUND_ROBIN, 1 = LEAST_REQUEST, 2 = RING_HASH, 3 = RANDOM, 5 = MAGLEV, 6 = CLUSTER_PROVIDED
    #[prost(int32, tag = "6")]
    pub lb_policy: i32,
    #[prost(message, optional, tag = "33")]
    pub load_assignment: Option<ClusterLoadAssignment>,
    #[prost(message, optional, tag = "10")]
    pub circuit_breakers: Option<CircuitBreakers>,
    #[prost(map = "string, message", tag = "36")]
    pub typed_extension_protocol_options: HashMap<String, Any>,
    #[prost(message, optional, tag = "19")]
    pub outlier_detection: Option<OutlierDetection>,
    #[prost(message, optional, tag = "21")]
    pub upstream_bind_config: Option<BindConfig>,
    #[prost(message, optional, tag = "24")]
    pub transport_socket: Option<TransportSocket>,
    #[prost(message, optional, tag = "25")]
    pub metadata: Option<Metadata>,
    #[prost(message, repeated, tag = "43")]
    pub transport_socket_matches: Vec<TransportSocketMatch>,
    #[prost(string, tag = "28")]
    pub alt_stat_name: String,
    #[prost(message, optional, tag = "34")]
    pub original_dst_lb_config: Option<OriginalDstLbConfig>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EdsClusterConfig {
    #[prost(string, tag = "2")]
    pub service_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TransportSocketMatch {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub r#match: Option<Struct>,
    #[prost(message, optional, tag = "3")]
    pub transport_socket: Option<TransportSocket>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CircuitBreakers {
    #[prost(message, repeated, tag = "1")]
    pub thresholds: Vec<Thresholds>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Thresholds {
    #[prost(int32, tag = "1")]
    pub priority: i32,
    #[prost(message, optional, tag = "2")]
    pub max_connections: Option<UInt32Value>,
    #[prost(message, optional, tag = "3")]
    pub max_pending_requests: Option<UInt32Value>,
    #[prost(message, optional, tag = "4")]
    pub max_requests: Option<UInt32Value>,
    #[prost(message, optional, tag = "5")]
    pub max_retries: Option<UInt32Value>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutlierDetection {
    #[prost(message, optional, tag = "1")]
    pub consecutive_5xx: Option<UInt32Value>,
    #[prost(message, optional, tag = "2")]
    pub interval: Option<Duration>,
    #[prost(message, optional, tag = "3")]
    pub base_ejection_time: Option<Duration>,
    #[prost(message, optional, tag = "4")]
    pub max_ejection_percent: Option<UInt32Value>,
    #[prost(message, optional, tag = "5")]
    pub enforcing_consecutive_5xx: Option<UInt32Value>,
    #[prost(message, optional, tag = "10")]
    pub consecutive_gateway_failure: Option<UInt32Value>,
    #[prost(message, optional, tag = "11")]
    pub enforcing_consecutive_gateway_failure: Option<UInt32Value>,
    #[prost(bool, tag = "12")]
    pub split_external_local_origin_errors: bool,
    #[prost(message, optional, tag = "13")]
    pub consecutive_local_origin_failure: Option<UInt32Value>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ClusterLoadAssignment {
    #[prost(string, tag = "1")]
    pub cluster_name: String,
    #[prost(message, repeated, tag = "2")]
    pub endpoints: Vec<LocalityLbEndpoints>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LocalityLbEndpoints {
    #[prost(message, optional, tag = "1")]
    pub locality: Option<Locality>,
    #[prost(message, repeated, tag = "2")]
    pub lb_endpoints: Vec<LbEndpoint>,
    #[prost(message, optional, tag = "3")]
    pub load_balancing_weight: Option<UInt32Value>,
    #[prost(uint32, tag = "5")]
    pub priority: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct LbEndpoint {
    #[prost(message, optional, tag = "1")]
    pub endpoint: Option<Endpoint>,
    /// 0 = UNKNOWN, 1 = HEALTHY, 2 = UNHEALTHY, 3 = DRAINING, 4 = TIMEOUT, 5 = DEGRADED
    #[prost(int32, tag = "2")]
    pub health_status: i32,
    #[prost(message, optional, tag = "3")]
    pub metadata: Option<Metadata>,
    #[prost(message, optional, tag = "4")]
    pub load_balancing_weight: Option<UInt32Value>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Endpoint {
    #[prost(message, optional, tag = "1")]
    pub address: Option<Address>,
    #[prost(string, tag = "3")]
    pub hostname: String,
}

/// `envoy.extensions.upstreams.http.v3.HttpProtocolOptions`
#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpProtocolOptions {
    #[prost(message, optional, tag = "1")]
    pub common_http_protocol_options: Option<CommonHttpProtocolOptions>,
    #[prost(
        oneof = "http_protocol_options::UpstreamProtocolOptions",
        tags = "3, 4, 5"
    )]
    pub upstream_protocol_options: Option<http_protocol_options::UpstreamProtocolOptions>,
}

pub mod http_protocol_options {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum UpstreamProtocolOptions {
        #[prost(message, tag = "3")]
        ExplicitHttpConfig(super::ExplicitHttpConfig),
        #[prost(message, tag = "4")]
        UseDownstreamProtocolConfig(super::UseDownstreamHttpConfig),
        #[prost(message, tag = "5")]
        AutoConfig(super::Empty),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExplicitHttpConfig {
    /// 1 = HTTP/1.1、2 = HTTP/2、3 = HTTP/3（中身は使わない）
    #[prost(message, optional, tag = "1")]
    pub http_protocol_options: Option<Empty>,
    #[prost(message, optional, tag = "2")]
    pub http2_protocol_options: Option<Empty>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UseDownstreamHttpConfig {
    #[prost(message, optional, tag = "1")]
    pub http_protocol_options: Option<Empty>,
    #[prost(message, optional, tag = "2")]
    pub http2_protocol_options: Option<Empty>,
}

/// `envoy.config.core.v3.HttpProtocolOptions`
#[derive(Clone, PartialEq, prost::Message)]
pub struct CommonHttpProtocolOptions {
    #[prost(message, optional, tag = "1")]
    pub idle_timeout: Option<Duration>,
    #[prost(message, optional, tag = "6")]
    pub max_requests_per_connection: Option<UInt32Value>,
}

/// 中身を読まないメッセージ（存在だけを見る）。
#[derive(Clone, PartialEq, prost::Message)]
pub struct Empty {}

// ---------------------------------------------------------------------------
// envoy.config.route.v3
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct RouteConfiguration {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, repeated, tag = "2")]
    pub virtual_hosts: Vec<VirtualHost>,
    #[prost(message, repeated, tag = "4")]
    pub response_headers_to_add: Vec<HeaderValueOption>,
    #[prost(string, repeated, tag = "5")]
    pub response_headers_to_remove: Vec<String>,
    #[prost(message, repeated, tag = "6")]
    pub request_headers_to_add: Vec<HeaderValueOption>,
    #[prost(string, repeated, tag = "8")]
    pub request_headers_to_remove: Vec<String>,
    #[prost(bool, tag = "14")]
    pub ignore_port_in_host_matching: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct VirtualHost {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, repeated, tag = "2")]
    pub domains: Vec<String>,
    #[prost(message, repeated, tag = "3")]
    pub routes: Vec<Route>,
    #[prost(message, repeated, tag = "7")]
    pub request_headers_to_add: Vec<HeaderValueOption>,
    #[prost(string, repeated, tag = "13")]
    pub request_headers_to_remove: Vec<String>,
    #[prost(message, repeated, tag = "10")]
    pub response_headers_to_add: Vec<HeaderValueOption>,
    #[prost(string, repeated, tag = "11")]
    pub response_headers_to_remove: Vec<String>,
    #[prost(message, optional, tag = "8")]
    pub cors: Option<CorsPolicy>,
    #[prost(map = "string, message", tag = "15")]
    pub typed_per_filter_config: HashMap<String, Any>,
    #[prost(bool, tag = "14")]
    pub include_request_attempt_count: bool,
    #[prost(message, optional, tag = "16")]
    pub retry_policy: Option<RetryPolicy>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Route {
    #[prost(string, tag = "14")]
    pub name: String,
    #[prost(message, optional, tag = "1")]
    pub r#match: Option<RouteMatch>,
    #[prost(message, optional, tag = "4")]
    pub metadata: Option<Metadata>,
    #[prost(message, optional, tag = "5")]
    pub decorator: Option<Decorator>,
    #[prost(map = "string, message", tag = "13")]
    pub typed_per_filter_config: HashMap<String, Any>,
    #[prost(message, repeated, tag = "9")]
    pub request_headers_to_add: Vec<HeaderValueOption>,
    #[prost(string, repeated, tag = "12")]
    pub request_headers_to_remove: Vec<String>,
    #[prost(message, repeated, tag = "10")]
    pub response_headers_to_add: Vec<HeaderValueOption>,
    #[prost(string, repeated, tag = "11")]
    pub response_headers_to_remove: Vec<String>,
    #[prost(oneof = "route::Action", tags = "2, 3, 7, 18")]
    pub action: Option<route::Action>,
}

pub mod route {
    // 理由付き allow: prost の oneof（Envoy の Route.action の写し）。設定の受信時に 1 回デコードするだけで、
    // 箱に入れると生成コードの形（prost の oneof の型）と食い違う。
    #[allow(clippy::large_enum_variant)]
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Action {
        #[prost(message, tag = "2")]
        Route(super::RouteAction),
        #[prost(message, tag = "3")]
        Redirect(super::RedirectAction),
        #[prost(message, tag = "7")]
        DirectResponse(super::DirectResponseAction),
        #[prost(message, tag = "18")]
        NonForwardingAction(super::Empty),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Decorator {
    #[prost(string, tag = "1")]
    pub operation: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RouteMatch {
    #[prost(message, optional, tag = "4")]
    pub case_sensitive: Option<BoolValue>,
    #[prost(message, optional, tag = "9")]
    pub runtime_fraction: Option<RuntimeFractionalPercent>,
    #[prost(message, repeated, tag = "6")]
    pub headers: Vec<HeaderMatcher>,
    #[prost(message, repeated, tag = "7")]
    pub query_parameters: Vec<QueryParameterMatcher>,
    #[prost(message, optional, tag = "8")]
    pub grpc: Option<Empty>,
    #[prost(oneof = "route_match::PathSpecifier", tags = "1, 2, 10, 12, 14")]
    pub path_specifier: Option<route_match::PathSpecifier>,
}

pub mod route_match {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum PathSpecifier {
        #[prost(string, tag = "1")]
        Prefix(String),
        #[prost(string, tag = "2")]
        Path(String),
        #[prost(message, tag = "10")]
        SafeRegex(super::RegexMatcher),
        #[prost(message, tag = "12")]
        ConnectMatcher(super::Empty),
        #[prost(string, tag = "14")]
        PathSeparatedPrefix(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HeaderMatcher {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(bool, tag = "8")]
    pub invert_match: bool,
    #[prost(bool, tag = "14")]
    pub treat_missing_header_as_empty: bool,
    #[prost(oneof = "header_matcher::Specifier", tags = "4, 11, 7, 9, 10, 12, 13")]
    pub specifier: Option<header_matcher::Specifier>,
}

pub mod header_matcher {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Specifier {
        #[prost(string, tag = "4")]
        ExactMatch(String),
        #[prost(message, tag = "11")]
        SafeRegexMatch(super::RegexMatcher),
        #[prost(bool, tag = "7")]
        PresentMatch(bool),
        #[prost(string, tag = "9")]
        PrefixMatch(String),
        #[prost(string, tag = "10")]
        SuffixMatch(String),
        #[prost(string, tag = "12")]
        ContainsMatch(String),
        #[prost(message, tag = "13")]
        StringMatch(super::StringMatcher),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct QueryParameterMatcher {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(oneof = "query_parameter_matcher::Specifier", tags = "5, 6")]
    pub specifier: Option<query_parameter_matcher::Specifier>,
}

pub mod query_parameter_matcher {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Specifier {
        #[prost(message, tag = "5")]
        StringMatch(super::StringMatcher),
        #[prost(bool, tag = "6")]
        PresentMatch(bool),
    }
}

/// `envoy.type.matcher.v3.StringMatcher`
#[derive(Clone, PartialEq, prost::Message)]
pub struct StringMatcher {
    #[prost(bool, tag = "6")]
    pub ignore_case: bool,
    #[prost(oneof = "string_matcher::Pattern", tags = "1, 2, 3, 5, 7")]
    pub pattern: Option<string_matcher::Pattern>,
}

pub mod string_matcher {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Pattern {
        #[prost(string, tag = "1")]
        Exact(String),
        #[prost(string, tag = "2")]
        Prefix(String),
        #[prost(string, tag = "3")]
        Suffix(String),
        #[prost(message, tag = "5")]
        SafeRegex(super::RegexMatcher),
        #[prost(string, tag = "7")]
        Contains(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RegexMatcher {
    #[prost(string, tag = "2")]
    pub regex: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RouteAction {
    #[prost(oneof = "route_action::ClusterSpecifier", tags = "1, 2, 3")]
    pub cluster_specifier: Option<route_action::ClusterSpecifier>,
    #[prost(string, tag = "5")]
    pub prefix_rewrite: String,
    #[prost(message, optional, tag = "32")]
    pub regex_rewrite: Option<RegexMatchAndSubstitute>,
    #[prost(message, optional, tag = "8")]
    pub timeout: Option<Duration>,
    #[prost(message, optional, tag = "24")]
    pub idle_timeout: Option<Duration>,
    #[prost(message, optional, tag = "9")]
    pub retry_policy: Option<RetryPolicy>,
    #[prost(message, repeated, tag = "30")]
    pub request_mirror_policies: Vec<RequestMirrorPolicy>,
    #[prost(message, repeated, tag = "15")]
    pub hash_policy: Vec<HashPolicy>,
    #[prost(message, optional, tag = "17")]
    pub cors: Option<CorsPolicy>,
    #[prost(message, repeated, tag = "25")]
    pub upgrade_configs: Vec<UpgradeConfig>,
    #[prost(message, optional, tag = "36")]
    pub max_stream_duration: Option<MaxStreamDuration>,
    /// クラスタが無いときの応答（0 = 503、1 = 404、2 = 500。Istio は Gateway API の無効な backendRef に 500）
    #[prost(int32, tag = "20")]
    pub cluster_not_found_response_code: i32,
    #[prost(oneof = "route_action::HostRewrite", tags = "6, 7")]
    pub host_rewrite: Option<route_action::HostRewrite>,
}

pub mod route_action {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum ClusterSpecifier {
        #[prost(string, tag = "1")]
        Cluster(String),
        #[prost(string, tag = "2")]
        ClusterHeader(String),
        #[prost(message, tag = "3")]
        WeightedClusters(super::WeightedCluster),
    }

    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum HostRewrite {
        #[prost(string, tag = "6")]
        HostRewriteLiteral(String),
        #[prost(message, tag = "7")]
        AutoHostRewrite(super::BoolValue),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RegexMatchAndSubstitute {
    #[prost(message, optional, tag = "1")]
    pub pattern: Option<RegexMatcher>,
    #[prost(string, tag = "2")]
    pub substitution: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct MaxStreamDuration {
    #[prost(message, optional, tag = "1")]
    pub max_stream_duration: Option<Duration>,
    #[prost(message, optional, tag = "2")]
    pub grpc_timeout_header_max: Option<Duration>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WeightedCluster {
    #[prost(message, repeated, tag = "1")]
    pub clusters: Vec<ClusterWeight>,
    #[prost(message, optional, tag = "3")]
    pub total_weight: Option<UInt32Value>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ClusterWeight {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub weight: Option<UInt32Value>,
    #[prost(message, repeated, tag = "4")]
    pub request_headers_to_add: Vec<HeaderValueOption>,
    #[prost(string, repeated, tag = "9")]
    pub request_headers_to_remove: Vec<String>,
    #[prost(message, repeated, tag = "5")]
    pub response_headers_to_add: Vec<HeaderValueOption>,
    #[prost(string, repeated, tag = "6")]
    pub response_headers_to_remove: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RetryPolicy {
    #[prost(string, tag = "1")]
    pub retry_on: String,
    #[prost(message, optional, tag = "2")]
    pub num_retries: Option<UInt32Value>,
    #[prost(message, optional, tag = "3")]
    pub per_try_timeout: Option<Duration>,
    #[prost(uint32, repeated, tag = "7")]
    pub retriable_status_codes: Vec<u32>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestMirrorPolicy {
    #[prost(string, tag = "1")]
    pub cluster: String,
    #[prost(message, optional, tag = "3")]
    pub runtime_fraction: Option<RuntimeFractionalPercent>,
    /// Host に `-shadow` を付けない（Istio は true）
    #[prost(bool, tag = "6")]
    pub disable_shadow_host_suffix_append: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HashPolicy {
    #[prost(oneof = "hash_policy::Specifier", tags = "1, 2, 3, 5")]
    pub specifier: Option<hash_policy::Specifier>,
    #[prost(bool, tag = "4")]
    pub terminal: bool,
}

pub mod hash_policy {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Header {
        #[prost(string, tag = "1")]
        pub header_name: String,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct Cookie {
        #[prost(string, tag = "1")]
        pub name: String,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ConnectionProperties {
        #[prost(bool, tag = "1")]
        pub source_ip: bool,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct QueryParameter {
        #[prost(string, tag = "1")]
        pub name: String,
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Specifier {
        #[prost(message, tag = "1")]
        Header(Header),
        #[prost(message, tag = "2")]
        Cookie(Cookie),
        #[prost(message, tag = "3")]
        ConnectionProperties(ConnectionProperties),
        #[prost(message, tag = "5")]
        QueryParameter(QueryParameter),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UpgradeConfig {
    #[prost(string, tag = "1")]
    pub upgrade_type: String,
    #[prost(message, optional, tag = "2")]
    pub enabled: Option<BoolValue>,
    /// CONNECT を終端する（中身は使わない）
    #[prost(message, optional, tag = "3")]
    pub connect_config: Option<Empty>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CorsPolicy {
    #[prost(message, repeated, tag = "11")]
    pub allow_origin_string_match: Vec<StringMatcher>,
    #[prost(string, tag = "2")]
    pub allow_methods: String,
    #[prost(string, tag = "3")]
    pub allow_headers: String,
    #[prost(string, tag = "4")]
    pub expose_headers: String,
    #[prost(string, tag = "5")]
    pub max_age: String,
    #[prost(message, optional, tag = "6")]
    pub allow_credentials: Option<BoolValue>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RedirectAction {
    #[prost(string, tag = "1")]
    pub host_redirect: String,
    #[prost(uint32, tag = "8")]
    pub port_redirect: u32,
    /// 0 = 301, 1 = 302, 2 = 303, 3 = 307, 4 = 308
    #[prost(int32, tag = "3")]
    pub response_code: i32,
    #[prost(bool, tag = "6")]
    pub strip_query: bool,
    #[prost(bool, tag = "4")]
    pub https_redirect: bool,
    #[prost(string, tag = "7")]
    pub scheme_redirect: String,
    #[prost(string, tag = "2")]
    pub path_redirect: String,
    #[prost(string, tag = "5")]
    pub prefix_rewrite: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DirectResponseAction {
    #[prost(uint32, tag = "1")]
    pub status: u32,
    #[prost(message, optional, tag = "2")]
    pub body: Option<DataSource>,
}

// ---------------------------------------------------------------------------
// envoy.extensions.filters.network.*
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpConnectionManager {
    /// 0 = AUTO, 1 = HTTP1, 2 = HTTP2
    #[prost(int32, tag = "1")]
    pub codec_type: i32,
    #[prost(message, repeated, tag = "13")]
    pub access_log: Vec<crate::mesh::accesslog::AccessLogProto>,
    #[prost(string, tag = "2")]
    pub stat_prefix: String,
    #[prost(message, repeated, tag = "5")]
    pub http_filters: Vec<HttpFilter>,
    #[prost(message, optional, tag = "7")]
    pub tracing: Option<crate::mesh::tracing::HcmTracingFull>,
    #[prost(string, tag = "10")]
    pub server_name: String,
    #[prost(message, optional, tag = "24")]
    pub stream_idle_timeout: Option<Duration>,
    #[prost(message, optional, tag = "14")]
    pub use_remote_address: Option<BoolValue>,
    #[prost(message, optional, tag = "15")]
    pub generate_request_id: Option<BoolValue>,
    /// 0 = SANITIZE, 1 = FORWARD_ONLY, 2 = APPEND_FORWARD, 3 = SANITIZE_SET, 4 = ALWAYS_FORWARD_ONLY
    #[prost(int32, tag = "16")]
    pub forward_client_cert_details: i32,
    #[prost(message, optional, tag = "17")]
    pub set_current_client_cert_details: Option<SetCurrentClientCertDetails>,
    #[prost(message, repeated, tag = "23")]
    pub upgrade_configs: Vec<UpgradeConfig>,
    #[prost(oneof = "hcm::RouteSpecifier", tags = "3, 4")]
    pub route_specifier: Option<hcm::RouteSpecifier>,
}

pub mod hcm {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum RouteSpecifier {
        #[prost(message, tag = "3")]
        Rds(super::Rds),
        #[prost(message, tag = "4")]
        RouteConfig(super::RouteConfiguration),
    }
}

/// `envoy.type.v3.Percent`（0〜100 の double）
#[derive(Clone, PartialEq, prost::Message)]
pub struct Percent {
    #[prost(double, tag = "1")]
    pub value: f64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SetCurrentClientCertDetails {
    #[prost(message, optional, tag = "1")]
    pub subject: Option<BoolValue>,
    #[prost(bool, tag = "3")]
    pub cert: bool,
    #[prost(bool, tag = "6")]
    pub chain: bool,
    #[prost(bool, tag = "4")]
    pub dns: bool,
    #[prost(bool, tag = "5")]
    pub uri: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Rds {
    #[prost(string, tag = "2")]
    pub route_config_name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpFilter {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "4")]
    pub typed_config: Option<Any>,
    /// ECDS（Istio の WasmPlugin）。`config_source` は ADS 固定なので読まない
    #[prost(message, optional, tag = "5")]
    pub config_discovery: Option<ExtensionConfigSource>,
    #[prost(bool, tag = "6")]
    pub is_optional: bool,
    #[prost(bool, tag = "7")]
    pub disabled: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExtensionConfigSource {
    #[prost(message, optional, tag = "2")]
    pub default_config: Option<Any>,
    #[prost(bool, tag = "3")]
    pub apply_default_config_without_warming: bool,
    #[prost(string, repeated, tag = "4")]
    pub type_urls: Vec<String>,
}

/// ECDS のリソース（`envoy.config.core.v3.TypedExtensionConfig`）。
#[derive(Clone, PartialEq, prost::Message)]
pub struct TypedExtensionConfig {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub typed_config: Option<Any>,
}

/// `envoy.extensions.filters.http.wasm.v3.Wasm`
#[derive(Clone, PartialEq, prost::Message)]
pub struct WasmFilter {
    #[prost(message, optional, tag = "1")]
    pub config: Option<WasmPluginConfig>,
}

/// `envoy.extensions.wasm.v3.PluginConfig`
#[derive(Clone, PartialEq, prost::Message)]
pub struct WasmPluginConfig {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub root_id: String,
    #[prost(message, optional, tag = "3")]
    pub vm_config: Option<WasmVmConfig>,
    #[prost(message, optional, tag = "4")]
    pub configuration: Option<Any>,
    #[prost(bool, tag = "5")]
    pub fail_open: bool,
    /// 0 = FAIL_CLOSED, 1 = FAIL_OPEN, 2 = FAIL_RELOAD
    #[prost(int32, tag = "7")]
    pub failure_policy: i32,
}

/// `envoy.extensions.wasm.v3.VmConfig`
#[derive(Clone, PartialEq, prost::Message)]
pub struct WasmVmConfig {
    #[prost(string, tag = "1")]
    pub vm_id: String,
    #[prost(string, tag = "2")]
    pub runtime: String,
    #[prost(message, optional, tag = "3")]
    pub code: Option<AsyncDataSource>,
    #[prost(message, optional, tag = "4")]
    pub configuration: Option<Any>,
}

/// `envoy.config.core.v3.AsyncDataSource`（`remote` は pilot-agent がローカルのファイルに書き換えるので読まない）
#[derive(Clone, PartialEq, prost::Message)]
pub struct AsyncDataSource {
    #[prost(message, optional, tag = "1")]
    pub local: Option<DataSource>,
}

/// `google.protobuf.StringValue` / `BytesValue`（Wasm の `configuration`）
#[derive(Clone, PartialEq, prost::Message)]
pub struct StringValue {
    #[prost(string, tag = "1")]
    pub value: String,
}
#[derive(Clone, PartialEq, prost::Message)]
pub struct BytesValue {
    #[prost(bytes = "vec", tag = "1")]
    pub value: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TcpProxy {
    #[prost(string, tag = "1")]
    pub stat_prefix: String,
    #[prost(message, repeated, tag = "5")]
    pub access_log: Vec<crate::mesh::accesslog::AccessLogProto>,
    #[prost(message, optional, tag = "8")]
    pub idle_timeout: Option<Duration>,
    #[prost(oneof = "tcp_proxy::ClusterSpecifier", tags = "2, 10")]
    pub cluster_specifier: Option<tcp_proxy::ClusterSpecifier>,
    /// HTTP/2 CONNECT のトンネル（ambient の HBONE）
    #[prost(message, optional, tag = "12")]
    pub tunneling_config: Option<tcp_proxy::TunnelingConfig>,
}

pub mod tcp_proxy {
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct TunnelingConfig {
        #[prost(string, tag = "1")]
        pub hostname: String,
        #[prost(bool, tag = "2")]
        pub use_post: bool,
        #[prost(message, repeated, tag = "3")]
        pub headers_to_add: Vec<super::HeaderValueOption>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct WeightedCluster {
        #[prost(message, repeated, tag = "1")]
        pub clusters: Vec<ClusterWeight>,
    }
    #[derive(Clone, PartialEq, prost::Message)]
    pub struct ClusterWeight {
        #[prost(string, tag = "1")]
        pub name: String,
        #[prost(uint32, tag = "2")]
        pub weight: u32,
    }
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum ClusterSpecifier {
        #[prost(string, tag = "2")]
        Cluster(String),
        #[prost(message, tag = "10")]
        WeightedClusters(WeightedCluster),
    }
}

// ---------------------------------------------------------------------------
// envoy.extensions.filters.http.fault.v3
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpFault {
    #[prost(message, optional, tag = "1")]
    pub delay: Option<FaultDelay>,
    #[prost(message, optional, tag = "2")]
    pub abort: Option<FaultAbort>,
    #[prost(message, repeated, tag = "4")]
    pub headers: Vec<HeaderMatcher>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FaultDelay {
    #[prost(message, optional, tag = "4")]
    pub percentage: Option<FractionalPercent>,
    #[prost(message, optional, tag = "3")]
    pub fixed_delay: Option<Duration>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FaultAbort {
    #[prost(message, optional, tag = "3")]
    pub percentage: Option<FractionalPercent>,
    #[prost(uint32, tag = "2")]
    pub http_status: u32,
    #[prost(uint32, tag = "5")]
    pub grpc_status: u32,
}

// ---------------------------------------------------------------------------
// envoy.extensions.transport_sockets.tls.v3
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct DownstreamTlsContext {
    #[prost(message, optional, tag = "1")]
    pub common_tls_context: Option<CommonTlsContext>,
    #[prost(message, optional, tag = "2")]
    pub require_client_certificate: Option<BoolValue>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UpstreamTlsContext {
    #[prost(message, optional, tag = "1")]
    pub common_tls_context: Option<CommonTlsContext>,
    #[prost(string, tag = "2")]
    pub sni: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CommonTlsContext {
    #[prost(message, repeated, tag = "6")]
    pub tls_certificate_sds_secret_configs: Vec<SdsSecretConfig>,
    #[prost(string, repeated, tag = "4")]
    pub alpn_protocols: Vec<String>,
    #[prost(oneof = "common_tls_context::ValidationContextType", tags = "3, 7, 8")]
    pub validation_context_type: Option<common_tls_context::ValidationContextType>,
}

pub mod common_tls_context {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum ValidationContextType {
        #[prost(message, tag = "3")]
        ValidationContext(super::CertificateValidationContext),
        #[prost(message, tag = "7")]
        ValidationContextSdsSecretConfig(super::SdsSecretConfig),
        #[prost(message, tag = "8")]
        CombinedValidationContext(super::CombinedCertificateValidationContext),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CombinedCertificateValidationContext {
    #[prost(message, optional, tag = "1")]
    pub default_validation_context: Option<CertificateValidationContext>,
    #[prost(message, optional, tag = "2")]
    pub validation_context_sds_secret_config: Option<SdsSecretConfig>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SdsSecretConfig {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CertificateValidationContext {
    #[prost(message, optional, tag = "1")]
    pub trusted_ca: Option<DataSource>,
    #[prost(message, repeated, tag = "9")]
    pub match_subject_alt_names: Vec<StringMatcher>,
    #[prost(message, repeated, tag = "15")]
    pub match_typed_subject_alt_names: Vec<SubjectAltNameMatcher>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SubjectAltNameMatcher {
    /// 1 = EMAIL, 2 = DNS, 3 = URI, 4 = IP_ADDRESS
    #[prost(int32, tag = "1")]
    pub san_type: i32,
    #[prost(message, optional, tag = "2")]
    pub matcher: Option<StringMatcher>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Secret {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "2")]
    pub tls_certificate: Option<TlsCertificate>,
    #[prost(message, optional, tag = "4")]
    pub validation_context: Option<CertificateValidationContext>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TlsCertificate {
    #[prost(message, optional, tag = "1")]
    pub certificate_chain: Option<DataSource>,
    #[prost(message, optional, tag = "2")]
    pub private_key: Option<DataSource>,
}

// ---------------------------------------------------------------------------
// type URL
// ---------------------------------------------------------------------------

pub const TYPE_LISTENER: &str = "type.googleapis.com/envoy.config.listener.v3.Listener";
pub const TYPE_CLUSTER: &str = "type.googleapis.com/envoy.config.cluster.v3.Cluster";
pub const TYPE_ROUTE: &str = "type.googleapis.com/envoy.config.route.v3.RouteConfiguration";
pub const TYPE_ENDPOINT: &str =
    "type.googleapis.com/envoy.config.endpoint.v3.ClusterLoadAssignment";
pub const TYPE_SECRET: &str =
    "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.Secret";
pub const TYPE_ECDS: &str = "type.googleapis.com/envoy.config.core.v3.TypedExtensionConfig";

pub const T_HCM: &str = "type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager";
pub const T_TCP_PROXY: &str =
    "type.googleapis.com/envoy.extensions.filters.network.tcp_proxy.v3.TcpProxy";
pub const T_DOWNSTREAM_TLS: &str =
    "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.DownstreamTlsContext";
pub const T_UPSTREAM_TLS: &str =
    "type.googleapis.com/envoy.extensions.transport_sockets.tls.v3.UpstreamTlsContext";
pub const T_HTTP_PROTOCOL_OPTIONS: &str =
    "type.googleapis.com/envoy.extensions.upstreams.http.v3.HttpProtocolOptions";
pub const T_INTERNAL_UPSTREAM: &str =
    "type.googleapis.com/envoy.extensions.transport_sockets.internal_upstream.v3.InternalUpstreamTransport";
pub const T_IP_MATCHER: &str = "type.googleapis.com/xds.type.matcher.v3.IPMatcher";
pub const T_WASM: &str = "type.googleapis.com/envoy.extensions.filters.http.wasm.v3.Wasm";
pub const T_FAULT: &str = "type.googleapis.com/envoy.extensions.filters.http.fault.v3.HTTPFault";
pub const T_TYPED_STRUCT_UDPA: &str = "type.googleapis.com/udpa.type.v1.TypedStruct";
pub const T_TYPED_STRUCT_XDS: &str = "type.googleapis.com/xds.type.v3.TypedStruct";

/// `Any` を指定の型としてデコードする（型が違えば `None`）。
pub fn decode_any<M: prost::Message + Default>(any: &Any, type_url: &str) -> Option<M> {
    if any.type_url != type_url {
        return None;
    }
    M::decode(any.value.as_slice()).ok()
}

/// WDS（Istio の workload discovery。`istio.workload.Workload` のうち送信元の特定に使う項目）。
pub const TYPE_WORKLOAD: &str = "type.googleapis.com/istio.workload.Workload";

#[derive(Clone, PartialEq, prost::Message)]
pub struct WdsWorkload {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub namespace: String,
    #[prost(bytes = "vec", repeated, tag = "3")]
    pub addresses: Vec<Vec<u8>>,
    #[prost(string, tag = "6")]
    pub trust_domain: String,
    #[prost(string, tag = "7")]
    pub service_account: String,
    #[prost(string, tag = "10")]
    pub canonical_name: String,
    #[prost(string, tag = "11")]
    pub canonical_revision: String,
    #[prost(string, tag = "13")]
    pub workload_name: String,
    #[prost(string, tag = "18")]
    pub cluster_id: String,
    #[prost(string, tag = "20")]
    pub uid: String,
}
