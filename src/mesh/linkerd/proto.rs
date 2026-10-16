//! Linkerd の制御プレーンの API（linkerd2-proxy-api 0.20 の proto のうち veil が使うもの）と、プロキシ同士の
//! transport header（`transport.l5d.io/v1`）。
//!
//! 公式のクレート（prost 0.14 + tonic）は veil の prost（0.13）と合わないので、型を手で書く（タグは
//! `proto/*.proto` と同じ。Envoy の xDS と同じ方針）。

use std::collections::HashMap;

pub use crate::mesh::xds::proto::Duration;

#[derive(Clone, PartialEq, prost::Message)]
pub struct Timestamp {
    #[prost(int64, tag = "1")]
    pub seconds: i64,
    #[prost(int32, tag = "2")]
    pub nanos: i32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Empty {}

// ---------------------------------------------------------------------------
// io.linkerd.proxy.identity
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct CertifyRequest {
    #[prost(string, tag = "1")]
    pub identity: String,
    #[prost(bytes = "vec", tag = "2")]
    pub token: Vec<u8>,
    #[prost(bytes = "vec", tag = "3")]
    pub certificate_signing_request: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CertifyResponse {
    #[prost(bytes = "vec", tag = "1")]
    pub leaf_certificate: Vec<u8>,
    #[prost(bytes = "vec", repeated, tag = "2")]
    pub intermediate_certificates: Vec<Vec<u8>>,
    #[prost(message, optional, tag = "3")]
    pub valid_until: Option<Timestamp>,
}

// ---------------------------------------------------------------------------
// io.linkerd.proxy.net / meta
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct IpAddress {
    #[prost(oneof = "ip_address::Ip", tags = "1, 2")]
    pub ip: Option<ip_address::Ip>,
}

pub mod ip_address {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Ip {
        #[prost(fixed32, tag = "1")]
        Ipv4(u32),
        #[prost(message, tag = "2")]
        Ipv6(super::IPv6),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct IPv6 {
    #[prost(fixed64, tag = "1")]
    pub first: u64,
    #[prost(fixed64, tag = "2")]
    pub last: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct IpNetwork {
    #[prost(message, optional, tag = "1")]
    pub ip: Option<IpAddress>,
    #[prost(uint32, tag = "2")]
    pub prefix_len: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TcpAddress {
    #[prost(message, optional, tag = "1")]
    pub ip: Option<IpAddress>,
    #[prost(uint32, tag = "2")]
    pub port: u32,
}

impl IpAddress {
    pub fn to_ip(&self) -> Option<std::net::IpAddr> {
        match &self.ip {
            Some(ip_address::Ip::Ipv4(v)) => Some(std::net::Ipv4Addr::from(*v).into()),
            Some(ip_address::Ip::Ipv6(v)) => {
                let n = ((v.first as u128) << 64) | v.last as u128;
                Some(std::net::Ipv6Addr::from(n).into())
            }
            None => None,
        }
    }

    pub fn from_ip(ip: std::net::IpAddr) -> Self {
        let ip = match ip {
            std::net::IpAddr::V4(v) => ip_address::Ip::Ipv4(u32::from(v)),
            std::net::IpAddr::V6(v) => {
                let n = u128::from(v);
                ip_address::Ip::Ipv6(IPv6 {
                    first: (n >> 64) as u64,
                    last: n as u64,
                })
            }
        };
        IpAddress { ip: Some(ip) }
    }
}

impl TcpAddress {
    pub fn to_addr(&self) -> Option<std::net::SocketAddr> {
        Some(std::net::SocketAddr::new(
            self.ip.as_ref()?.to_ip()?,
            self.port as u16,
        ))
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Metadata {
    #[prost(oneof = "metadata::Kind", tags = "1, 2")]
    pub kind: Option<metadata::Kind>,
}

pub mod metadata {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(string, tag = "1")]
        Default(String),
        #[prost(message, tag = "2")]
        Resource(super::Resource),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Resource {
    #[prost(string, tag = "1")]
    pub group: String,
    #[prost(string, tag = "2")]
    pub kind: String,
    #[prost(string, tag = "3")]
    pub name: String,
    #[prost(string, tag = "4")]
    pub namespace: String,
    #[prost(string, tag = "5")]
    pub section: String,
    #[prost(uint32, tag = "6")]
    pub port: u32,
}

impl Metadata {
    /// 観測・ログ用の名前（`kind/namespace/name` か既定の名前）。
    pub fn name(&self) -> String {
        match &self.kind {
            Some(metadata::Kind::Default(d)) => d.clone(),
            Some(metadata::Kind::Resource(r)) => format!("{}/{}/{}", r.kind, r.namespace, r.name),
            None => String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// io.linkerd.proxy.destination
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct GetDestination {
    #[prost(string, tag = "1")]
    pub scheme: String,
    #[prost(string, tag = "2")]
    pub path: String,
    #[prost(string, tag = "3")]
    pub context_token: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Update {
    #[prost(oneof = "update::Update", tags = "1, 2, 3")]
    pub update: Option<update::Update>,
}

pub mod update {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Update {
        #[prost(message, tag = "1")]
        Add(super::WeightedAddrSet),
        #[prost(message, tag = "2")]
        Remove(super::AddrSet),
        #[prost(message, tag = "3")]
        NoEndpoints(super::NoEndpoints),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AddrSet {
    #[prost(message, repeated, tag = "1")]
    pub addrs: Vec<TcpAddress>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WeightedAddrSet {
    #[prost(message, repeated, tag = "1")]
    pub addrs: Vec<WeightedAddr>,
    #[prost(map = "string, string", tag = "2")]
    pub metric_labels: HashMap<String, String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WeightedAddr {
    #[prost(message, optional, tag = "1")]
    pub addr: Option<TcpAddress>,
    #[prost(uint32, tag = "3")]
    pub weight: u32,
    #[prost(map = "string, string", tag = "4")]
    pub metric_labels: HashMap<String, String>,
    #[prost(message, optional, tag = "5")]
    pub tls_identity: Option<TlsIdentity>,
    #[prost(message, optional, tag = "6")]
    pub protocol_hint: Option<ProtocolHint>,
    #[prost(message, optional, tag = "7")]
    pub authority_override: Option<AuthorityOverride>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TlsIdentity {
    #[prost(oneof = "tls_identity::Strategy", tags = "1, 3")]
    pub strategy: Option<tls_identity::Strategy>,
    #[prost(message, optional, tag = "4")]
    pub server_name: Option<DnsLikeIdentity>,
}

pub mod tls_identity {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Strategy {
        #[prost(message, tag = "1")]
        DnsLikeIdentity(super::DnsLikeIdentity),
        #[prost(message, tag = "3")]
        UriLikeIdentity(super::UriLikeIdentity),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DnsLikeIdentity {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UriLikeIdentity {
    #[prost(string, tag = "1")]
    pub uri: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct AuthorityOverride {
    #[prost(string, tag = "1")]
    pub authority_override: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct NoEndpoints {
    #[prost(bool, tag = "1")]
    pub exists: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ProtocolHint {
    #[prost(oneof = "protocol_hint::Protocol", tags = "1, 3")]
    pub protocol: Option<protocol_hint::Protocol>,
    #[prost(message, optional, tag = "2")]
    pub opaque_transport: Option<OpaqueTransport>,
}

pub mod protocol_hint {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Protocol {
        #[prost(message, tag = "1")]
        H2(super::Empty),
        #[prost(message, tag = "3")]
        Opaque(super::Empty),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OpaqueTransport {
    #[prost(uint32, tag = "1")]
    pub inbound_port: u32,
}

// ---------------------------------------------------------------------------
// io.linkerd.proxy.http_types / http_route / grpc_route
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpMethod {
    #[prost(oneof = "http_method::Type", tags = "1, 2")]
    pub r#type: Option<http_method::Type>,
}

pub mod http_method {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Type {
        /// GET=0 POST=1 PUT=2 DELETE=3 PATCH=4 OPTIONS=5 CONNECT=6 HEAD=7 TRACE=8
        #[prost(int32, tag = "1")]
        Registered(i32),
        #[prost(string, tag = "2")]
        Unregistered(String),
    }
}

impl HttpMethod {
    pub fn as_str(&self) -> Option<String> {
        const R: [&str; 9] = [
            "GET", "POST", "PUT", "DELETE", "PATCH", "OPTIONS", "CONNECT", "HEAD", "TRACE",
        ];
        match &self.r#type {
            Some(http_method::Type::Registered(i)) => R.get(*i as usize).map(|s| s.to_string()),
            Some(http_method::Type::Unregistered(s)) => Some(s.clone()),
            None => None,
        }
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Scheme {
    #[prost(oneof = "scheme::Type", tags = "1, 2")]
    pub r#type: Option<scheme::Type>,
}

pub mod scheme {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Type {
        /// HTTP=0 HTTPS=1
        #[prost(int32, tag = "1")]
        Registered(i32),
        #[prost(string, tag = "2")]
        Unregistered(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Headers {
    #[prost(message, repeated, tag = "1")]
    pub headers: Vec<Header>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Header {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(bytes = "vec", tag = "2")]
    pub value: Vec<u8>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HostMatch {
    #[prost(oneof = "host_match::Match", tags = "1, 2")]
    pub r#match: Option<host_match::Match>,
}

pub mod host_match {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Match {
        #[prost(string, tag = "1")]
        Exact(String),
        #[prost(message, tag = "2")]
        Suffix(super::Suffix),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Suffix {
    #[prost(string, repeated, tag = "1")]
    pub reverse_labels: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpRouteMatch {
    #[prost(message, optional, tag = "1")]
    pub path: Option<PathMatch>,
    #[prost(message, repeated, tag = "2")]
    pub headers: Vec<HeaderMatch>,
    #[prost(message, repeated, tag = "3")]
    pub query_params: Vec<QueryParamMatch>,
    #[prost(message, optional, tag = "4")]
    pub method: Option<HttpMethod>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PathMatch {
    #[prost(oneof = "path_match::Kind", tags = "1, 2, 3")]
    pub kind: Option<path_match::Kind>,
}

pub mod path_match {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(string, tag = "1")]
        Exact(String),
        #[prost(string, tag = "2")]
        Prefix(String),
        #[prost(string, tag = "3")]
        Regex(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HeaderMatch {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(oneof = "header_match::Value", tags = "2, 3")]
    pub value: Option<header_match::Value>,
}

pub mod header_match {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(bytes = "vec", tag = "2")]
        Exact(Vec<u8>),
        #[prost(string, tag = "3")]
        Regex(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct QueryParamMatch {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(oneof = "query_param_match::Value", tags = "2, 3")]
    pub value: Option<query_param_match::Value>,
}

pub mod query_param_match {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Value {
        #[prost(string, tag = "2")]
        Exact(String),
        #[prost(string, tag = "3")]
        Regex(String),
    }
}

/// `RequestHeaderModifier` と `ResponseHeaderModifier`（同じ形）。
#[derive(Clone, PartialEq, prost::Message)]
pub struct HeaderModifier {
    #[prost(message, optional, tag = "1")]
    pub add: Option<Headers>,
    #[prost(message, optional, tag = "2")]
    pub set: Option<Headers>,
    #[prost(string, repeated, tag = "3")]
    pub remove: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RequestRedirect {
    #[prost(message, optional, tag = "1")]
    pub scheme: Option<Scheme>,
    #[prost(string, tag = "2")]
    pub host: String,
    #[prost(message, optional, tag = "3")]
    pub path: Option<PathModifier>,
    #[prost(uint32, tag = "4")]
    pub port: u32,
    #[prost(uint32, tag = "5")]
    pub status: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PathModifier {
    #[prost(oneof = "path_modifier::Replace", tags = "1, 2")]
    pub replace: Option<path_modifier::Replace>,
}

pub mod path_modifier {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Replace {
        #[prost(string, tag = "1")]
        Full(String),
        #[prost(string, tag = "2")]
        Prefix(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpFailureInjector {
    #[prost(uint32, tag = "1")]
    pub status: u32,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(message, optional, tag = "3")]
    pub ratio: Option<Ratio>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Ratio {
    #[prost(uint32, tag = "1")]
    pub numerator: u32,
    #[prost(uint32, tag = "2")]
    pub denominator: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Timeouts {
    #[prost(message, optional, tag = "1")]
    pub response: Option<Duration>,
    #[prost(message, optional, tag = "2")]
    pub request: Option<Duration>,
    #[prost(message, optional, tag = "3")]
    pub idle: Option<Duration>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrpcRouteMatch {
    #[prost(message, optional, tag = "1")]
    pub rpc: Option<GrpcRpcMatch>,
    #[prost(message, repeated, tag = "2")]
    pub headers: Vec<HeaderMatch>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrpcRpcMatch {
    #[prost(string, tag = "1")]
    pub service: String,
    #[prost(string, tag = "2")]
    pub method: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrpcFailureInjector {
    #[prost(uint32, tag = "1")]
    pub code: u32,
    #[prost(string, tag = "2")]
    pub message: String,
    #[prost(message, optional, tag = "3")]
    pub ratio: Option<Ratio>,
}

// ---------------------------------------------------------------------------
// io.linkerd.proxy.inbound
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct PortSpec {
    #[prost(string, tag = "1")]
    pub workload: String,
    #[prost(uint32, tag = "2")]
    pub port: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Server {
    #[prost(message, optional, tag = "1")]
    pub protocol: Option<InboundProtocol>,
    #[prost(message, repeated, tag = "2")]
    pub server_ips: Vec<IpAddress>,
    #[prost(message, repeated, tag = "3")]
    pub authorizations: Vec<Authz>,
    #[prost(map = "string, string", tag = "4")]
    pub labels: HashMap<String, String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InboundProtocol {
    #[prost(oneof = "inbound_protocol::Kind", tags = "1, 2, 3, 4, 5, 6")]
    pub kind: Option<inbound_protocol::Kind>,
}

pub mod inbound_protocol {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        Detect(super::InboundDetect),
        #[prost(message, tag = "2")]
        Opaque(super::Empty),
        #[prost(message, tag = "3")]
        Tls(super::Empty),
        #[prost(message, tag = "4")]
        Http1(super::InboundHttp),
        #[prost(message, tag = "5")]
        Http2(super::InboundHttp),
        #[prost(message, tag = "6")]
        Grpc(super::InboundGrpc),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InboundDetect {
    #[prost(message, optional, tag = "1")]
    pub timeout: Option<Duration>,
    #[prost(message, repeated, tag = "3")]
    pub http_routes: Vec<InboundHttpRoute>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InboundHttp {
    #[prost(message, repeated, tag = "2")]
    pub routes: Vec<InboundHttpRoute>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InboundGrpc {
    #[prost(message, repeated, tag = "2")]
    pub routes: Vec<InboundGrpcRoute>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Authz {
    #[prost(message, repeated, tag = "1")]
    pub networks: Vec<Network>,
    #[prost(message, optional, tag = "2")]
    pub authentication: Option<Authn>,
    #[prost(map = "string, string", tag = "3")]
    pub labels: HashMap<String, String>,
    #[prost(message, optional, tag = "4")]
    pub metadata: Option<Metadata>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Network {
    #[prost(message, optional, tag = "1")]
    pub net: Option<IpNetwork>,
    #[prost(message, repeated, tag = "2")]
    pub except: Vec<IpNetwork>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Authn {
    #[prost(oneof = "authn::Permit", tags = "1, 2")]
    pub permit: Option<authn::Permit>,
}

pub mod authn {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Permit {
        #[prost(message, tag = "1")]
        Unauthenticated(super::Empty),
        #[prost(message, tag = "2")]
        MeshTls(super::PermitMeshTls),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PermitMeshTls {
    #[prost(oneof = "permit_mesh_tls::Clients", tags = "1, 2")]
    pub clients: Option<permit_mesh_tls::Clients>,
}

pub mod permit_mesh_tls {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Clients {
        #[prost(message, tag = "1")]
        Unauthenticated(super::Empty),
        #[prost(message, tag = "2")]
        Identities(super::PermitClientIdentities),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct PermitClientIdentities {
    #[prost(message, repeated, tag = "1")]
    pub identities: Vec<Identity>,
    #[prost(message, repeated, tag = "2")]
    pub suffixes: Vec<IdentitySuffix>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Identity {
    #[prost(string, tag = "1")]
    pub name: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct IdentitySuffix {
    #[prost(string, repeated, tag = "1")]
    pub parts: Vec<String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InboundHttpRoute {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(message, repeated, tag = "2")]
    pub hosts: Vec<HostMatch>,
    #[prost(message, repeated, tag = "3")]
    pub authorizations: Vec<Authz>,
    #[prost(message, repeated, tag = "4")]
    pub rules: Vec<InboundHttpRule>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InboundHttpRule {
    #[prost(message, repeated, tag = "1")]
    pub matches: Vec<HttpRouteMatch>,
    #[prost(message, repeated, tag = "2")]
    pub filters: Vec<HttpFilter>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InboundGrpcRoute {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(message, repeated, tag = "2")]
    pub hosts: Vec<HostMatch>,
    #[prost(message, repeated, tag = "3")]
    pub authorizations: Vec<Authz>,
    #[prost(message, repeated, tag = "4")]
    pub rules: Vec<InboundGrpcRule>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct InboundGrpcRule {
    #[prost(message, repeated, tag = "1")]
    pub matches: Vec<GrpcRouteMatch>,
    #[prost(message, repeated, tag = "2")]
    pub filters: Vec<GrpcFilter>,
}

/// HTTP のルートのフィルタ（inbound・outbound 共通の形。response_header_modifier は outbound だけ）。
#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpFilter {
    #[prost(oneof = "http_filter::Kind", tags = "1, 2, 3, 4")]
    pub kind: Option<http_filter::Kind>,
}

pub mod http_filter {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        FailureInjector(super::HttpFailureInjector),
        #[prost(message, tag = "2")]
        RequestHeaderModifier(super::HeaderModifier),
        #[prost(message, tag = "3")]
        Redirect(super::RequestRedirect),
        #[prost(message, tag = "4")]
        ResponseHeaderModifier(super::HeaderModifier),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrpcFilter {
    #[prost(oneof = "grpc_filter::Kind", tags = "1, 2")]
    pub kind: Option<grpc_filter::Kind>,
}

pub mod grpc_filter {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        FailureInjector(super::GrpcFailureInjector),
        #[prost(message, tag = "2")]
        RequestHeaderModifier(super::HeaderModifier),
    }
}

// ---------------------------------------------------------------------------
// io.linkerd.proxy.outbound
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct TrafficSpec {
    #[prost(string, tag = "1")]
    pub source_workload: String,
    #[prost(oneof = "traffic_spec::Target", tags = "2, 3")]
    pub target: Option<traffic_spec::Target>,
}

pub mod traffic_spec {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Target {
        #[prost(message, tag = "2")]
        Addr(super::TcpAddress),
        #[prost(string, tag = "3")]
        Authority(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundPolicy {
    #[prost(message, optional, tag = "1")]
    pub protocol: Option<OutboundProtocol>,
    #[prost(message, optional, tag = "2")]
    pub metadata: Option<Metadata>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundProtocol {
    #[prost(oneof = "outbound_protocol::Kind", tags = "1, 2, 3, 4, 5, 6")]
    pub kind: Option<outbound_protocol::Kind>,
}

pub mod outbound_protocol {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        Detect(super::OutboundDetect),
        #[prost(message, tag = "2")]
        Opaque(super::OutboundOpaque),
        #[prost(message, tag = "3")]
        Http1(super::OutboundHttp),
        #[prost(message, tag = "4")]
        Http2(super::OutboundHttp),
        #[prost(message, tag = "5")]
        Grpc(super::OutboundGrpc),
        #[prost(message, tag = "6")]
        Tls(super::OutboundTls),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundDetect {
    #[prost(message, optional, tag = "1")]
    pub timeout: Option<Duration>,
    #[prost(message, optional, tag = "2")]
    pub opaque: Option<OutboundOpaque>,
    #[prost(message, optional, tag = "3")]
    pub http1: Option<OutboundHttp>,
    #[prost(message, optional, tag = "4")]
    pub http2: Option<OutboundHttp>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundOpaque {
    #[prost(message, repeated, tag = "1")]
    pub routes: Vec<OpaqueRoute>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundTls {
    #[prost(message, repeated, tag = "1")]
    pub routes: Vec<TlsRoute>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundHttp {
    #[prost(message, repeated, tag = "1")]
    pub routes: Vec<OutboundHttpRoute>,
    #[prost(message, optional, tag = "2")]
    pub failure_accrual: Option<FailureAccrual>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundGrpc {
    #[prost(message, repeated, tag = "1")]
    pub routes: Vec<OutboundGrpcRoute>,
    #[prost(message, optional, tag = "2")]
    pub failure_accrual: Option<FailureAccrual>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundHttpRoute {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(message, repeated, tag = "2")]
    pub hosts: Vec<HostMatch>,
    #[prost(message, repeated, tag = "3")]
    pub rules: Vec<OutboundHttpRule>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundHttpRule {
    #[prost(message, repeated, tag = "1")]
    pub matches: Vec<HttpRouteMatch>,
    #[prost(message, repeated, tag = "2")]
    pub filters: Vec<HttpFilter>,
    #[prost(message, optional, tag = "3")]
    pub backends: Option<Distribution>,
    #[prost(message, optional, tag = "4")]
    pub request_timeout: Option<Duration>,
    #[prost(message, optional, tag = "5")]
    pub timeouts: Option<Timeouts>,
    #[prost(message, optional, tag = "6")]
    pub retry: Option<HttpRetry>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundGrpcRoute {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(message, repeated, tag = "2")]
    pub hosts: Vec<HostMatch>,
    #[prost(message, repeated, tag = "3")]
    pub rules: Vec<OutboundGrpcRule>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OutboundGrpcRule {
    #[prost(message, repeated, tag = "1")]
    pub matches: Vec<GrpcRouteMatch>,
    #[prost(message, repeated, tag = "2")]
    pub filters: Vec<GrpcFilter>,
    #[prost(message, optional, tag = "3")]
    pub backends: Option<Distribution>,
    #[prost(message, optional, tag = "4")]
    pub request_timeout: Option<Duration>,
    #[prost(message, optional, tag = "5")]
    pub timeouts: Option<Timeouts>,
    #[prost(message, optional, tag = "6")]
    pub retry: Option<GrpcRetry>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OpaqueRoute {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(message, repeated, tag = "3")]
    pub rules: Vec<OpaqueRule>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OpaqueRule {
    #[prost(message, optional, tag = "1")]
    pub backends: Option<Distribution>,
    #[prost(message, repeated, tag = "2")]
    pub filters: Vec<OpaqueFilter>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct OpaqueFilter {
    #[prost(oneof = "opaque_filter::Kind", tags = "1, 2")]
    pub kind: Option<opaque_filter::Kind>,
}

pub mod opaque_filter {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        Invalid(super::Invalid),
        #[prost(message, tag = "2")]
        Forbidden(super::Empty),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Invalid {
    #[prost(string, tag = "1")]
    pub message: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TlsRoute {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(message, repeated, tag = "3")]
    pub rules: Vec<OpaqueRule>,
}

/// ルートのバックエンドの分配（HTTP・gRPC・opaque・TLS で同じ形。RouteBackend の filters は使わない）。
#[derive(Clone, PartialEq, prost::Message)]
pub struct Distribution {
    #[prost(oneof = "distribution::Kind", tags = "1, 2, 3")]
    pub kind: Option<distribution::Kind>,
}

pub mod distribution {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        Empty(super::Empty),
        #[prost(message, tag = "2")]
        FirstAvailable(super::FirstAvailable),
        #[prost(message, tag = "3")]
        RandomAvailable(super::RandomAvailable),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FirstAvailable {
    #[prost(message, repeated, tag = "1")]
    pub backends: Vec<RouteBackend>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RandomAvailable {
    #[prost(message, repeated, tag = "1")]
    pub backends: Vec<WeightedRouteBackend>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct RouteBackend {
    #[prost(message, optional, tag = "1")]
    pub backend: Option<Backend>,
    #[prost(message, optional, tag = "4")]
    pub request_timeout: Option<Duration>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct WeightedRouteBackend {
    #[prost(message, optional, tag = "1")]
    pub backend: Option<RouteBackend>,
    #[prost(uint32, tag = "2")]
    pub weight: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Backend {
    #[prost(message, optional, tag = "1")]
    pub metadata: Option<Metadata>,
    #[prost(oneof = "backend::Kind", tags = "2, 3")]
    pub kind: Option<backend::Kind>,
    #[prost(message, optional, tag = "4")]
    pub queue: Option<Queue>,
}

pub mod backend {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "2")]
        Forward(super::WeightedAddr),
        #[prost(message, tag = "3")]
        Balancer(super::BalanceP2c),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct BalanceP2c {
    #[prost(message, optional, tag = "1")]
    pub discovery: Option<EndpointDiscovery>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EndpointDiscovery {
    #[prost(message, optional, tag = "1")]
    pub dst: Option<DestinationGet>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct DestinationGet {
    #[prost(string, tag = "1")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Queue {
    #[prost(uint32, tag = "1")]
    pub capacity: u32,
    #[prost(message, optional, tag = "2")]
    pub failfast_timeout: Option<Duration>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpRetry {
    #[prost(uint32, tag = "1")]
    pub max_retries: u32,
    #[prost(uint32, tag = "2")]
    pub max_request_bytes: u32,
    #[prost(message, optional, tag = "3")]
    pub conditions: Option<HttpRetryConditions>,
    #[prost(message, optional, tag = "4")]
    pub timeout: Option<Duration>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpRetryConditions {
    #[prost(message, repeated, tag = "1")]
    pub status_ranges: Vec<StatusRange>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct StatusRange {
    #[prost(uint32, tag = "1")]
    pub start: u32,
    #[prost(uint32, tag = "2")]
    pub end: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrpcRetry {
    #[prost(uint32, tag = "1")]
    pub max_retries: u32,
    #[prost(uint32, tag = "2")]
    pub max_request_bytes: u32,
    #[prost(message, optional, tag = "3")]
    pub conditions: Option<GrpcRetryConditions>,
    #[prost(message, optional, tag = "4")]
    pub timeout: Option<Duration>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct GrpcRetryConditions {
    #[prost(bool, tag = "1")]
    pub cancelled: bool,
    #[prost(bool, tag = "4")]
    pub deadline_exceeded: bool,
    #[prost(bool, tag = "8")]
    pub resource_exhausted: bool,
    #[prost(bool, tag = "13")]
    pub internal: bool,
    #[prost(bool, tag = "14")]
    pub unavailable: bool,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct FailureAccrual {
    #[prost(oneof = "failure_accrual::Kind", tags = "1, 2")]
    pub kind: Option<failure_accrual::Kind>,
}

pub mod failure_accrual {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        ConsecutiveFailures(super::ConsecutiveFailures),
        #[prost(message, tag = "2")]
        Unified(super::UnifiedAccrual),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ConsecutiveFailures {
    #[prost(uint32, tag = "1")]
    pub max_failures: u32,
    #[prost(message, optional, tag = "2")]
    pub backoff: Option<ExponentialBackoff>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct UnifiedAccrual {
    #[prost(uint32, tag = "4")]
    pub max_consecutive_failures: u32,
    #[prost(message, optional, tag = "5")]
    pub backoff: Option<ExponentialBackoff>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ExponentialBackoff {
    #[prost(message, optional, tag = "1")]
    pub min_backoff: Option<Duration>,
    #[prost(message, optional, tag = "2")]
    pub max_backoff: Option<Duration>,
}

// ---------------------------------------------------------------------------
// io.linkerd.proxy.tap（viz の `linkerd viz tap / top`）
// ---------------------------------------------------------------------------

#[derive(Clone, PartialEq, prost::Message)]
pub struct ObserveRequest {
    #[prost(uint32, tag = "1")]
    pub limit: u32,
    #[prost(message, optional, tag = "2")]
    pub r#match: Option<TapMatch>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapMatch {
    #[prost(oneof = "tap_match::Match", tags = "1, 2, 3, 4, 5, 6, 7, 8")]
    pub r#match: Option<tap_match::Match>,
}

pub mod tap_match {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Match {
        #[prost(message, tag = "1")]
        All(super::TapMatchSeq),
        #[prost(message, tag = "2")]
        Any(super::TapMatchSeq),
        #[prost(message, tag = "3")]
        Not(Box<super::TapMatch>),
        #[prost(message, tag = "4")]
        Source(super::TapTcp),
        #[prost(message, tag = "5")]
        Destination(super::TapTcp),
        #[prost(message, tag = "6")]
        Http(super::TapHttp),
        #[prost(message, tag = "7")]
        DestinationLabel(super::TapLabel),
        #[prost(message, tag = "8")]
        RouteLabel(super::TapLabel),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapMatchSeq {
    #[prost(message, repeated, tag = "1")]
    pub matches: Vec<TapMatch>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapLabel {
    #[prost(string, tag = "1")]
    pub key: String,
    #[prost(string, tag = "2")]
    pub value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapTcp {
    #[prost(oneof = "tap_tcp::Match", tags = "1, 3")]
    pub r#match: Option<tap_tcp::Match>,
}

pub mod tap_tcp {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Match {
        #[prost(message, tag = "1")]
        Netmask(super::TapNetmask),
        #[prost(message, tag = "3")]
        Ports(super::TapPortRange),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapNetmask {
    #[prost(message, optional, tag = "1")]
    pub ip: Option<IpAddress>,
    #[prost(uint32, tag = "2")]
    pub mask: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapPortRange {
    #[prost(uint32, tag = "1")]
    pub min: u32,
    #[prost(uint32, tag = "2")]
    pub max: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapHttp {
    #[prost(oneof = "tap_http::Match", tags = "1, 2, 3, 4")]
    pub r#match: Option<tap_http::Match>,
}

pub mod tap_http {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Match {
        #[prost(message, tag = "1")]
        Scheme(super::Scheme),
        #[prost(message, tag = "2")]
        Authority(super::TapStringMatch),
        #[prost(message, tag = "3")]
        Method(super::HttpMethod),
        #[prost(message, tag = "4")]
        Path(super::TapStringMatch),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapStringMatch {
    #[prost(oneof = "tap_string_match::Match", tags = "1, 2")]
    pub r#match: Option<tap_string_match::Match>,
}

pub mod tap_string_match {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Match {
        #[prost(string, tag = "1")]
        Exact(String),
        #[prost(string, tag = "2")]
        Prefix(String),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapEvent {
    #[prost(message, optional, tag = "1")]
    pub source: Option<TcpAddress>,
    #[prost(message, optional, tag = "5")]
    pub source_meta: Option<EndpointMeta>,
    #[prost(message, optional, tag = "7")]
    pub route_meta: Option<EndpointMeta>,
    #[prost(message, optional, tag = "2")]
    pub destination: Option<TcpAddress>,
    #[prost(message, optional, tag = "4")]
    pub destination_meta: Option<EndpointMeta>,
    /// UNKNOWN=0 INBOUND=1 OUTBOUND=2
    #[prost(int32, tag = "6")]
    pub proxy_direction: i32,
    #[prost(message, optional, tag = "3")]
    pub http: Option<TapHttpEvent>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct EndpointMeta {
    #[prost(map = "string, string", tag = "1")]
    pub labels: HashMap<String, String>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapHttpEvent {
    #[prost(oneof = "tap_http_event::Event", tags = "1, 2, 3")]
    pub event: Option<tap_http_event::Event>,
}

pub mod tap_http_event {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Event {
        #[prost(message, tag = "1")]
        RequestInit(super::TapRequestInit),
        #[prost(message, tag = "2")]
        ResponseInit(super::TapResponseInit),
        #[prost(message, tag = "3")]
        ResponseEnd(super::TapResponseEnd),
    }
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapStreamId {
    #[prost(uint32, tag = "1")]
    pub base: u32,
    #[prost(uint64, tag = "2")]
    pub stream: u64,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapRequestInit {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TapStreamId>,
    #[prost(message, optional, tag = "2")]
    pub method: Option<HttpMethod>,
    #[prost(message, optional, tag = "3")]
    pub scheme: Option<Scheme>,
    #[prost(string, tag = "4")]
    pub authority: String,
    #[prost(string, tag = "5")]
    pub path: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapResponseInit {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TapStreamId>,
    #[prost(message, optional, tag = "2")]
    pub since_request_init: Option<Duration>,
    #[prost(uint32, tag = "3")]
    pub http_status: u32,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapResponseEnd {
    #[prost(message, optional, tag = "1")]
    pub id: Option<TapStreamId>,
    #[prost(message, optional, tag = "2")]
    pub since_request_init: Option<Duration>,
    #[prost(message, optional, tag = "3")]
    pub since_response_init: Option<Duration>,
    #[prost(uint64, tag = "4")]
    pub response_bytes: u64,
    #[prost(message, optional, tag = "5")]
    pub eos: Option<TapEos>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TapEos {
    #[prost(oneof = "tap_eos::End", tags = "1, 2")]
    pub end: Option<tap_eos::End>,
}

pub mod tap_eos {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum End {
        #[prost(uint32, tag = "1")]
        GrpcStatusCode(u32),
        #[prost(uint32, tag = "2")]
        ResetErrorCode(u32),
    }
}

// ---------------------------------------------------------------------------
// transport.l5d.io/v1（プロキシ同士の opaque transport の先頭に送るヘッダ）
// ---------------------------------------------------------------------------

pub const TRANSPORT_ALPN: &[u8] = b"transport.l5d.io/v1";
pub const TRANSPORT_PREFACE: &[u8] = b"transport.l5d.io/v1\r\n\r\n";

#[derive(Clone, PartialEq, prost::Message)]
pub struct TransportHeader {
    #[prost(int32, tag = "1")]
    pub port: i32,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(message, optional, tag = "3")]
    pub session_protocol: Option<SessionProtocol>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct SessionProtocol {
    #[prost(oneof = "session_protocol::Kind", tags = "1, 2")]
    pub kind: Option<session_protocol::Kind>,
}

pub mod session_protocol {
    #[derive(Clone, PartialEq, prost::Oneof)]
    pub enum Kind {
        #[prost(message, tag = "1")]
        Http1(super::Empty),
        #[prost(message, tag = "2")]
        Http2(super::Empty),
    }
}

impl TransportHeader {
    /// 先頭に送るバイト列（プレフェース + 長さ（u32 BE） + proto）。
    pub fn encode_prefaced(&self) -> Vec<u8> {
        use prost::Message;
        let len = self.encoded_len();
        let mut b = Vec::with_capacity(TRANSPORT_PREFACE.len() + 4 + len);
        b.extend_from_slice(TRANSPORT_PREFACE);
        b.extend_from_slice(&(len as u32).to_be_bytes());
        let _ = self.encode(&mut b);
        b
    }

    /// 先読みしたバイト列から読む。足りなければ `Ok(None)`（`need` が要るバイト数）。
    pub fn decode_prefaced(buf: &[u8]) -> Result<Option<(Self, usize)>, String> {
        use prost::Message;
        let hdr = TRANSPORT_PREFACE.len() + 4;
        if buf.len() < hdr {
            return Ok(None);
        }
        if &buf[..TRANSPORT_PREFACE.len()] != TRANSPORT_PREFACE {
            return Err("not a transport header".into());
        }
        let n =
            u32::from_be_bytes([buf[hdr - 4], buf[hdr - 3], buf[hdr - 2], buf[hdr - 1]]) as usize;
        if n > 64 * 1024 {
            return Err("transport header too large".into());
        }
        if buf.len() < hdr + n {
            return Ok(None);
        }
        let h = TransportHeader::decode(&buf[hdr..hdr + n]).map_err(|e| e.to_string())?;
        Ok(Some((h, hdr + n)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_header_roundtrip() {
        let h = TransportHeader {
            port: 6379,
            name: String::new(),
            session_protocol: None,
        };
        let mut b = h.encode_prefaced();
        assert!(b.starts_with(TRANSPORT_PREFACE));
        b.extend_from_slice(b"PING\r\n");
        let (d, used) = TransportHeader::decode_prefaced(&b).unwrap().unwrap();
        assert_eq!(d.port, 6379);
        assert_eq!(&b[used..], b"PING\r\n");
        assert!(TransportHeader::decode_prefaced(&b[..10])
            .unwrap()
            .is_none());
    }

    #[test]
    fn ip_address_roundtrip() {
        for ip in ["10.244.0.7", "fd00::1:2"] {
            let ip: std::net::IpAddr = ip.parse().unwrap();
            assert_eq!(IpAddress::from_ip(ip).to_ip(), Some(ip));
        }
    }
}
