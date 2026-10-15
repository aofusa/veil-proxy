//! メッシュモデル（F-218）: 制御プレーンに依存しない中間表現。
//!
//! Envoy の xDS（F-222）も Linkerd の proxy-api（F-223）もここへ変換する。各値は作ったら変えない
//! （更新は新しい値を作って [`super::state`] の `ArcSwap` を差し替える）。ルート・クラスタの参照は
//! 名前ではなく共有のスロット（[`RouteSlot`]・[`ClusterSlot`]）を持ち、要求ごとに名前で表を引かない。

use arc_swap::ArcSwap;
use bytes::Bytes;
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

// ---------------------------------------------------------------------------
// 文字列・ヘッダの照合
// ---------------------------------------------------------------------------

/// Envoy の `StringMatcher`。
#[derive(Debug, Clone)]
pub enum StrMatch {
    Exact(String),
    Prefix(String),
    Suffix(String),
    Contains(String),
    Regex(regex::Regex),
}

#[derive(Debug, Clone)]
pub struct StringMatcher {
    pub pattern: StrMatch,
    pub ignore_case: bool,
}

impl StringMatcher {
    pub fn exact(s: &str) -> Self {
        StringMatcher {
            pattern: StrMatch::Exact(s.to_string()),
            ignore_case: false,
        }
    }

    pub fn matches(&self, v: &[u8]) -> bool {
        let eq = |a: &[u8], b: &[u8]| {
            if self.ignore_case {
                a.eq_ignore_ascii_case(b)
            } else {
                a == b
            }
        };
        match &self.pattern {
            StrMatch::Exact(s) => eq(v, s.as_bytes()),
            StrMatch::Prefix(s) => v.len() >= s.len() && eq(&v[..s.len()], s.as_bytes()),
            StrMatch::Suffix(s) => v.len() >= s.len() && eq(&v[v.len() - s.len()..], s.as_bytes()),
            StrMatch::Contains(s) => {
                if s.is_empty() {
                    return true;
                }
                if self.ignore_case {
                    v.windows(s.len())
                        .any(|w| w.eq_ignore_ascii_case(s.as_bytes()))
                } else {
                    memchr::memmem::find(v, s.as_bytes()).is_some()
                }
            }
            StrMatch::Regex(re) => match std::str::from_utf8(v) {
                // Envoy の safe_regex は全体一致
                Ok(s) => re
                    .find(s)
                    .is_some_and(|m| m.start() == 0 && m.end() == s.len()),
                Err(_) => false,
            },
        }
    }
}

/// ヘッダの条件（`RouteMatch.headers`）。
#[derive(Debug, Clone)]
pub struct HeaderMatch {
    /// 小文字の名前
    pub name: String,
    pub kind: HeaderMatchKind,
    pub invert: bool,
    pub treat_missing_as_empty: bool,
}

#[derive(Debug, Clone)]
pub enum HeaderMatchKind {
    Present(bool),
    Str(StringMatcher),
}

impl HeaderMatch {
    /// `value` はヘッダの値（無ければ `None`。複数あれば `,` で連結した値）。
    pub fn matches(&self, value: Option<&[u8]>) -> bool {
        let r = match (&self.kind, value) {
            (HeaderMatchKind::Present(p), v) => v.is_some() == *p,
            (HeaderMatchKind::Str(m), Some(v)) => m.matches(v),
            (HeaderMatchKind::Str(m), None) => self.treat_missing_as_empty && m.matches(b""),
        };
        r != self.invert
    }
}

#[derive(Debug, Clone)]
pub struct QueryMatch {
    pub name: String,
    /// `None` は存在だけ
    pub value: Option<StringMatcher>,
}

/// IP の範囲。
#[derive(Debug, Clone, Copy)]
pub struct Cidr {
    pub addr: IpAddr,
    pub len: u8,
}

impl Cidr {
    pub fn contains(&self, ip: IpAddr) -> bool {
        match (self.addr, ip) {
            (IpAddr::V4(a), IpAddr::V4(b)) => {
                let len = self.len.min(32) as u32;
                if len == 0 {
                    return true;
                }
                let mask = u32::MAX << (32 - len);
                (u32::from(a) & mask) == (u32::from(b) & mask)
            }
            (IpAddr::V6(a), IpAddr::V6(b)) => {
                let len = self.len.min(128) as u32;
                if len == 0 {
                    return true;
                }
                let mask = u128::MAX << (128 - len);
                (u128::from(a) & mask) == (u128::from(b) & mask)
            }
            // IPv4 射影の IPv6
            (IpAddr::V4(_), IpAddr::V6(b)) => b
                .to_ipv4_mapped()
                .is_some_and(|v4| self.contains(IpAddr::V4(v4))),
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------
// ヘッダの書き換え
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendAction {
    /// 既にあれば追加、無ければ追加（Envoy の APPEND_IF_EXISTS_OR_ADD）
    Append,
    /// 無いときだけ追加
    AddIfAbsent,
    /// 上書き（無ければ追加）
    Overwrite,
    /// あるときだけ上書き
    OverwriteIfExists,
}

#[derive(Debug, Clone)]
pub struct HeaderAdd {
    /// 小文字の名前
    pub name: Bytes,
    pub value: Bytes,
    pub action: AppendAction,
}

/// 要求または応答のヘッダの書き換え（削除 → 追加の順）。
#[derive(Debug, Clone, Default)]
pub struct HeaderOps {
    pub remove: Vec<Bytes>,
    pub add: Vec<HeaderAdd>,
}

impl HeaderOps {
    pub fn is_empty(&self) -> bool {
        self.remove.is_empty() && self.add.is_empty()
    }

    /// `headers`（小文字の名前）に適用する。
    pub fn apply(&self, headers: &mut Vec<(Bytes, Bytes)>) {
        for r in &self.remove {
            headers.retain(|(n, _)| !n.eq_ignore_ascii_case(r));
        }
        for a in &self.add {
            let exists = headers.iter().any(|(n, _)| n.eq_ignore_ascii_case(&a.name));
            match a.action {
                AppendAction::Append => headers.push((a.name.clone(), a.value.clone())),
                AppendAction::AddIfAbsent => {
                    if !exists {
                        headers.push((a.name.clone(), a.value.clone()));
                    }
                }
                AppendAction::Overwrite | AppendAction::OverwriteIfExists => {
                    if exists {
                        headers.retain(|(n, _)| !n.eq_ignore_ascii_case(&a.name));
                        headers.push((a.name.clone(), a.value.clone()));
                    } else if a.action == AppendAction::Overwrite {
                        headers.push((a.name.clone(), a.value.clone()));
                    }
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// リスナー・フィルタチェーン
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Inbound,
    Outbound,
    Unspecified,
}

/// 受け口。
#[derive(Debug)]
pub struct MeshListener {
    pub name: String,
    pub addr: SocketAddr,
    /// 元の宛先で別のリスナーへ渡す（Envoy の `use_original_dst`。virtualOutbound）
    pub use_original_dst: bool,
    /// 自分でポートを開く（`bind_to_port`。false は元の宛先で渡されるだけのリスナー）
    pub bind_to_port: bool,
    pub direction: Direction,
    pub chains: Vec<Arc<FilterChain>>,
    pub default_chain: Option<Arc<FilterChain>>,
    /// TLS の ClientHello を調べる（`tls_inspector`）
    pub tls_inspector: bool,
    /// 平文の HTTP を調べる（`http_inspector`）
    pub http_inspector: bool,
    /// フィルタチェーンの選択に送信元・宛先 IP・SNI・ALPN を見るか（見ないなら先読みを省く）
    pub needs_sniff: bool,
    /// Envoy の内部リスナー（ポートを持たず、HBONE の CONNECT の終端・内部アドレスのエンドポイントから渡される）
    pub internal: bool,
    /// `filter_chain_matcher`（あれば `chains` の条件より優先する。ambient の waypoint）
    pub matcher: Option<ChainMatcher>,
}

/// `filter_chain_matcher`（xds.type.matcher.v3 の matcher_tree）を評価する形。
#[derive(Debug)]
pub struct ChainMatcher {
    pub input: MatchInput,
    pub map: MatchMap,
    pub on_no_match: Option<Box<ChainOnMatch>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchInput {
    DstIp,
    DstPort,
    SrcIp,
    SrcPort,
    ServerName,
    TransportProtocol,
    ApplicationProtocol,
}

#[derive(Debug)]
pub enum MatchMap {
    Exact(HashMap<String, ChainOnMatch>),
    Prefix(Vec<(String, ChainOnMatch)>),
    /// IP の範囲（最長一致）
    Ip(Vec<(Vec<Cidr>, ChainOnMatch)>),
}

#[derive(Debug)]
pub enum ChainOnMatch {
    /// `chains` の添字
    Chain(usize),
    Matcher(Box<ChainMatcher>),
}

impl ChainMatcher {
    /// 一致したフィルタチェーンの添字。
    pub fn eval(&self, f: &ConnFacts<'_>) -> Option<usize> {
        let value: Option<String> = match self.input {
            MatchInput::DstIp => f.dst.map(|a| a.ip().to_string()),
            MatchInput::DstPort => f.dst.map(|a| a.port().to_string()),
            MatchInput::SrcIp => f.src.map(|a| a.ip().to_string()),
            MatchInput::SrcPort => f.src.map(|a| a.port().to_string()),
            MatchInput::ServerName => f.sni.map(|s| s.to_string()),
            MatchInput::TransportProtocol => f.transport.map(|t| {
                match t {
                    TransportProto::Tls => "tls",
                    TransportProto::RawBuffer => "raw_buffer",
                }
                .to_string()
            }),
            MatchInput::ApplicationProtocol => f.alpn.first().map(|s| s.to_string()),
        };
        let hit = match (&self.map, &value) {
            (MatchMap::Exact(m), Some(v)) => m.get(v),
            (MatchMap::Prefix(v), Some(val)) => v
                .iter()
                .filter(|(p, _)| val.starts_with(p.as_str()))
                .max_by_key(|(p, _)| p.len())
                .map(|(_, m)| m),
            (MatchMap::Ip(v), Some(val)) => match val.parse::<IpAddr>() {
                Ok(ip) => v
                    .iter()
                    .filter_map(|(ranges, m)| {
                        ranges
                            .iter()
                            .filter(|r| r.contains(ip))
                            .map(|r| r.len)
                            .max()
                            .map(|len| (len, m))
                    })
                    .max_by_key(|(len, _)| *len)
                    .map(|(_, m)| m),
                Err(_) => None,
            },
            _ => None,
        };
        let resolved = hit.and_then(|m| m.eval(f));
        resolved.or_else(|| self.on_no_match.as_ref().and_then(|m| m.eval(f)))
    }
}

impl ChainOnMatch {
    fn eval(&self, f: &ConnFacts<'_>) -> Option<usize> {
        match self {
            ChainOnMatch::Chain(i) => Some(*i),
            ChainOnMatch::Matcher(m) => m.eval(f),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportProto {
    RawBuffer,
    Tls,
}

/// フィルタチェーンの選択条件（Envoy の `FilterChainMatch`）。空の条件は何にでも一致する。
#[derive(Debug, Default)]
pub struct ChainMatch {
    pub dst_port: Option<u16>,
    pub prefix_ranges: Vec<Cidr>,
    pub server_names: Vec<String>,
    pub transport: Option<TransportProto>,
    pub application_protocols: Vec<String>,
    pub source_prefix_ranges: Vec<Cidr>,
    pub source_ports: Vec<u16>,
}

/// 接続の属性（フィルタチェーンの選択に使う）。
#[derive(Debug, Clone, Default)]
pub struct ConnFacts<'a> {
    pub dst: Option<SocketAddr>,
    pub src: Option<SocketAddr>,
    pub sni: Option<&'a str>,
    pub transport: Option<TransportProto>,
    /// TLS なら ClientHello の ALPN、平文なら検出したプロトコル（`http/1.1`・`h2c`）
    pub alpn: &'a [&'a str],
}

#[derive(Debug)]
pub struct FilterChain {
    pub name: String,
    pub m: ChainMatch,
    /// 終端する TLS（mTLS のサーバ側）
    pub tls: Option<Arc<DownstreamTls>>,
    pub action: ChainAction,
    /// この経路の観測（`istio.stats` 等）を有効にするか
    pub stats: bool,
}

#[derive(Debug)]
pub enum ChainAction {
    Http(Arc<HttpCm>),
    Tcp(Arc<TcpProxyCfg>),
    /// 何もせず閉じる（BlackHole）
    Close,
}

/// TLS の終端（サーバ側）の設定。
#[derive(Debug)]
pub struct DownstreamTls {
    pub require_client_cert: bool,
    pub alpn: Vec<Vec<u8>>,
    /// クライアント証明書の SAN の条件（空 = 検証済みなら何でも）
    pub san_matchers: Vec<StringMatcher>,
    /// 自分の証明書の SDS 名（Istio は `default`）
    pub cert_name: String,
    /// 信頼アンカーの SDS 名（Istio は `ROOTCA`）
    pub validation_name: Option<String>,
    /// rustls のサーバ設定（証明書・信頼アンカーは差し替え可能なリゾルバを経由する）
    pub server_config: Arc<rustls::ServerConfig>,
    /// ALPN を交渉しない設定。クライアントの ALPN と重ならないときに使う（rustls は ALPN の不一致で
    /// ハンドシェイクを止めるが、Envoy（BoringSSL）は交渉しないで続ける。Istio の TCP の mTLS は
    /// `istio` だけを提示し、受け側の Envoy は `istio-peer-exchange, h2, http/1.1` を持つ）
    pub server_config_no_alpn: Arc<rustls::ServerConfig>,
}

impl DownstreamTls {
    /// ClientHello の ALPN に合わせて設定を選ぶ。
    pub fn config_for(&self, client_alpn: &[String]) -> Arc<rustls::ServerConfig> {
        if client_alpn.is_empty()
            || self
                .alpn
                .iter()
                .any(|a| client_alpn.iter().any(|c| c.as_bytes() == a.as_slice()))
        {
            self.server_config.clone()
        } else {
            self.server_config_no_alpn.clone()
        }
    }
}

/// クライアント証明書の転送（`forward_client_cert_details`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum XfccMode {
    Sanitize,
    ForwardOnly,
    AppendForward,
    SanitizeSet,
    AlwaysForwardOnly,
}

/// HTTP の接続マネージャ（Envoy の HCM）。
#[derive(Debug)]
pub struct HttpCm {
    pub stat_prefix: String,
    pub routes: RouteRef,
    pub codec: Codec,
    pub server_name: Bytes,
    pub xfcc: XfccMode,
    pub xfcc_subject: bool,
    pub xfcc_uri: bool,
    pub xfcc_dns: bool,
    pub generate_request_id: bool,
    /// 接続の相手のアドレスを X-Forwarded-For に足す（Envoy の `use_remote_address`。ゲートウェイ）
    pub use_remote_address: bool,
    pub websocket: bool,
    /// HTTP フィルタ（順序どおり。fault・cors・rbac 等。未知で任意のものは除いてある）
    pub filters: Vec<HttpFilterCfg>,
    /// Istio の peer metadata の交換（ヘッダ）をするか
    pub peer_metadata: bool,
    /// Istio の ALPN の上書き（`istio.alpn`）。上流の ALPN を `istio-http/1.1` 等にする
    pub istio_alpn: bool,
    pub direction: Direction,
    pub stream_idle_timeout: Option<Duration>,
    /// トレースのサンプリング（百万分率）
    pub trace_sampling: u32,
    /// アクセスログ（Istio の Telemetry API。空なら何もしない）
    pub access_logs: Vec<Arc<crate::mesh::accesslog::AccessLogCfg>>,
    /// 分散トレース（Zipkin。None なら B3 を素通しするだけ）
    pub tracer: Option<Arc<crate::mesh::tracing::TracerCfg>>,
    /// Linkerd の取り決め（`server`・`x-envoy-*`・`x-forwarded-proto` を付けない、inbound で
    /// `l5d-client-id`、ローカル応答に `l5d-proxy-error`。F-223）
    pub linkerd: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Codec {
    Auto,
    Http1,
    Http2,
}

#[derive(Debug)]
pub enum HttpFilterCfg {
    Fault(Arc<FaultCfg>),
    Cors,
    Rbac(Arc<crate::mesh::model::RbacCfg>),
    /// JWT の検証（Istio の RequestAuthentication。F-220）
    Jwt(Arc<crate::mesh::jwt::JwtCfg>),
    /// ECDS で届くフィルタ（Istio の WasmPlugin。F-222）
    Ecds(Arc<EcdsSlot>),
    /// Proxy-Wasm のプラグイン（ECDS の中身）
    #[cfg(feature = "wasm")]
    Wasm(Arc<crate::mesh::wasm::MeshWasm>),
    /// 何もしない（読み込めなかった FAIL_OPEN のプラグイン）
    Noop,
}

/// ECDS の 1 リソースのスロット（HCM のフィルタはこれを共有し、ECDS の更新で中身を差し替える）。
pub struct EcdsSlot {
    pub name: String,
    /// 届くまでは `None`（Envoy はリスナーのウォーミングで待つ。veil は要求を少し待たせてから 500）
    pub filter: arc_swap::ArcSwapOption<HttpFilterCfg>,
}

impl std::fmt::Debug for EcdsSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EcdsSlot").field("name", &self.name).finish()
    }
}

/// ルート表への参照（RDS の名前で共有するスロットか、インライン）。
#[derive(Debug, Clone)]
pub enum RouteRef {
    Rds(Arc<RouteSlot>),
    Inline(Arc<RouteTable>),
}

impl RouteRef {
    pub fn load(&self) -> Arc<RouteTable> {
        match self {
            RouteRef::Rds(s) => s.table.load_full(),
            RouteRef::Inline(t) => t.clone(),
        }
    }
}

/// RDS の名前ごとのスロット（LDS・RDS の届く順序に依らず同じ `Arc` を共有する）。
#[derive(Debug)]
pub struct RouteSlot {
    pub name: String,
    pub table: ArcSwap<RouteTable>,
    /// RDS が一度でも届いたか（届くまでは空の表＝404。Envoy のウォーミングに相当）
    pub ready: std::sync::atomic::AtomicBool,
}

impl RouteSlot {
    pub fn new(name: &str) -> Self {
        RouteSlot {
            name: name.to_string(),
            table: ArcSwap::from_pointee(RouteTable::empty(name)),
            ready: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

#[derive(Debug)]
pub struct TcpProxyCfg {
    pub stat_prefix: String,
    pub clusters: Vec<(Arc<ClusterSlot>, u32)>,
    pub total_weight: u32,
    pub idle_timeout: Option<Duration>,
    /// TCP の認可（Envoy の network RBAC。Istio の AuthorizationPolicy の TCP 規則）
    pub rbac: Option<Arc<RbacCfg>>,
    pub access_logs: Vec<Arc<crate::mesh::accesslog::AccessLogCfg>>,
    /// HTTP/2 CONNECT のトンネル（HBONE）の `:authority` の書式（`tunneling_config.hostname`）
    pub tunnel: Option<Arc<str>>,
}

impl TcpProxyCfg {
    pub fn pick(&self) -> Option<&Arc<ClusterSlot>> {
        if self.clusters.len() <= 1 || self.total_weight == 0 {
            return self.clusters.first().map(|(c, _)| c);
        }
        let mut r = crate::mesh::httpcore::rand_u32() % self.total_weight;
        for (c, w) in &self.clusters {
            if r < *w {
                return Some(c);
            }
            r -= *w;
        }
        self.clusters.last().map(|(c, _)| c)
    }
}

// ---------------------------------------------------------------------------
// ルート
// ---------------------------------------------------------------------------

#[derive(Debug)]
pub struct RouteTable {
    pub name: String,
    pub vhosts: Vec<VirtualHost>,
    /// 完全一致のドメイン（小文字）→ vhosts の添字
    pub exact: HashMap<String, usize>,
    /// `*.example.com` 型（接尾辞、長い順）
    pub suffix: Vec<(String, usize)>,
    /// `example.*` 型（接頭辞、長い順）
    pub prefix: Vec<(String, usize)>,
    pub catch_all: Option<usize>,
    pub ignore_port: bool,
    pub request_ops: HeaderOps,
    pub response_ops: HeaderOps,
}

/// Host の末尾のポートを外す（IPv6 の `[..]:port` に注意）。
fn strip_port(h: &[u8]) -> &[u8] {
    if let Some(pos) = h.iter().rposition(|&b| b == b':') {
        if h.first() == Some(&b'[') {
            if pos > 0 && h[pos - 1] == b']' {
                return &h[..pos];
            }
            return h;
        }
        if h[pos + 1..].iter().all(|b| b.is_ascii_digit()) {
            return &h[..pos];
        }
    }
    h
}

impl RouteTable {
    pub fn empty(name: &str) -> Self {
        RouteTable {
            name: name.to_string(),
            vhosts: Vec::new(),
            exact: HashMap::new(),
            suffix: Vec::new(),
            prefix: Vec::new(),
            catch_all: None,
            ignore_port: true,
            request_ops: HeaderOps::default(),
            response_ops: HeaderOps::default(),
        }
    }

    /// Host（`:authority`）から仮想ホストを選ぶ（Envoy と同じ優先順: 完全一致 → 接尾辞 → 接頭辞 → `*`）。
    pub fn select_vhost(&self, host: &[u8]) -> Option<&VirtualHost> {
        let mut h = host;
        let mut buf = [0u8; 256];
        let lower: &[u8] = if h.len() <= buf.len() {
            for (d, s) in buf.iter_mut().zip(h.iter()) {
                *d = s.to_ascii_lowercase();
            }
            &buf[..h.len()]
        } else {
            h
        };
        h = lower;
        let key = std::str::from_utf8(h).unwrap_or("");
        if let Some(&i) = self.exact.get(key) {
            return self.vhosts.get(i);
        }
        let no_port = std::str::from_utf8(strip_port(h)).unwrap_or("");
        if self.ignore_port || no_port.len() != key.len() {
            if let Some(&i) = self.exact.get(no_port) {
                return self.vhosts.get(i);
            }
        }
        let name = if self.ignore_port { no_port } else { key };
        for (suf, i) in &self.suffix {
            if name.len() > suf.len() && name.ends_with(suf.as_str()) {
                return self.vhosts.get(*i);
            }
        }
        for (pre, i) in &self.prefix {
            if name.len() > pre.len() && name.starts_with(pre.as_str()) {
                return self.vhosts.get(*i);
            }
        }
        self.catch_all.and_then(|i| self.vhosts.get(i))
    }
}

#[derive(Debug)]
pub struct VirtualHost {
    pub name: String,
    pub routes: Vec<MeshRoute>,
    pub request_ops: HeaderOps,
    pub response_ops: HeaderOps,
    pub include_attempt_count: bool,
    pub retry: Option<Arc<RetryPolicy>>,
    pub cors: Option<Arc<CorsCfg>>,
    pub fault: Option<Arc<FaultCfg>>,
}

#[derive(Debug)]
pub enum PathMatch {
    Prefix(String),
    Exact(String),
    /// `/a` は `/a` と `/a/...` に一致
    Separated(String),
    Regex(regex::Regex),
    Any,
    /// `connect_matcher`（CONNECT の要求だけ）
    Connect,
}

#[derive(Debug)]
pub struct MeshRoute {
    pub name: String,
    pub path: PathMatch,
    pub case_sensitive: bool,
    pub headers: Vec<HeaderMatch>,
    pub query: Vec<QueryMatch>,
    /// 一致させる割合（百万分率。None は常に）
    pub fraction: Option<u32>,
    /// gRPC の要求だけに一致
    pub grpc_only: bool,
    pub action: RouteAction,
    pub request_ops: HeaderOps,
    pub response_ops: HeaderOps,
    /// Istio の `decorator.operation`（トレースの操作名）
    pub operation: Bytes,
    pub fault: Option<Arc<FaultCfg>>,
    pub cors: Option<Arc<CorsCfg>>,
    /// ルート単位の認可（Linkerd の HTTPRoute を対象にした AuthorizationPolicy。F-223）
    pub rbac: Option<Arc<RbacCfg>>,
}

impl MeshRoute {
    /// パス（クエリ抜き）・ヘッダ・クエリで一致するか。
    pub fn matches(
        &self,
        path: &[u8],
        query: &[u8],
        header: &dyn Fn(&str) -> Option<Bytes>,
        is_grpc: bool,
    ) -> bool {
        if self.grpc_only && !is_grpc {
            return false;
        }
        let ok = match &self.path {
            PathMatch::Any => true,
            PathMatch::Connect => header(":method").is_some_and(|m| m.as_ref() == b"CONNECT"),
            PathMatch::Prefix(p) => {
                path.len() >= p.len()
                    && if self.case_sensitive {
                        &path[..p.len()] == p.as_bytes()
                    } else {
                        path[..p.len()].eq_ignore_ascii_case(p.as_bytes())
                    }
            }
            PathMatch::Exact(p) => {
                if self.case_sensitive {
                    path == p.as_bytes()
                } else {
                    path.eq_ignore_ascii_case(p.as_bytes())
                }
            }
            PathMatch::Separated(p) => {
                path.len() >= p.len()
                    && &path[..p.len()] == p.as_bytes()
                    && (path.len() == p.len() || path[p.len()] == b'/')
            }
            PathMatch::Regex(re) => std::str::from_utf8(path)
                .ok()
                .and_then(|s| re.find(s))
                .is_some_and(|m| m.start() == 0 && m.end() == path.len()),
        };
        if !ok {
            return false;
        }
        for h in &self.headers {
            let v = header(&h.name);
            if !h.matches(v.as_deref()) {
                return false;
            }
        }
        for q in &self.query {
            let v = query_value(query, q.name.as_bytes());
            match (&q.value, v) {
                (None, Some(_)) => {}
                (Some(m), Some(v)) if m.matches(v) => {}
                _ => return false,
            }
        }
        if let Some(f) = self.fraction {
            if crate::mesh::httpcore::rand_u32() % 1_000_000 >= f {
                return false;
            }
        }
        true
    }
}

/// クエリ文字列から名前の値を探す（デコードしない）。
pub fn query_value<'a>(query: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    for pair in query.split(|&b| b == b'&') {
        let (k, v) = match pair.iter().position(|&b| b == b'=') {
            Some(i) => (&pair[..i], &pair[i + 1..]),
            None => (pair, &b""[..]),
        };
        if k == name {
            return Some(v);
        }
    }
    None
}

#[derive(Debug)]
pub enum RouteAction {
    Forward(Arc<ForwardAction>),
    Redirect(Arc<RedirectCfg>),
    Direct {
        status: u16,
        body: Bytes,
    },
    /// 転送しない（Istio のゲートウェイの一部で使う）
    NonForwarding,
}

#[derive(Debug)]
pub struct ForwardAction {
    pub target: ClusterTarget,
    /// 要求全体のタイムアウト（None = 無制限。Istio の既定は 0s = 無制限）
    pub timeout: Option<Duration>,
    pub idle_timeout: Option<Duration>,
    pub retry: Option<Arc<RetryPolicy>>,
    pub prefix_rewrite: Option<Bytes>,
    pub regex_rewrite: Option<(regex::Regex, String)>,
    pub host_rewrite: Option<Bytes>,
    pub auto_host_rewrite: bool,
    /// ミラー先（クラスタ, 百万分率, Host に `-shadow` を付けるか）
    pub mirrors: Vec<(Arc<ClusterSlot>, u32, bool)>,
    pub hash: Vec<HashPolicy>,
    pub websocket: Option<bool>,
    /// CONNECT を終端して中身を内部リスナーへ渡す（HBONE。`upgrade_configs` の `connect_config`）
    pub connect_terminate: bool,
}

#[derive(Debug)]
pub enum ClusterTarget {
    Single(Arc<ClusterSlot>),
    Weighted {
        clusters: Vec<WeightedTarget>,
        total: u32,
    },
    /// 要求ヘッダの値をクラスタ名とする
    Header(String),
}

#[derive(Debug)]
pub struct WeightedTarget {
    pub cluster: Arc<ClusterSlot>,
    pub weight: u32,
    pub request_ops: HeaderOps,
    pub response_ops: HeaderOps,
}

#[derive(Debug, Clone)]
pub enum HashPolicy {
    Header(String),
    Cookie(String),
    SourceIp,
    Query(String),
}

#[derive(Debug)]
pub struct RetryPolicy {
    pub num_retries: u32,
    pub per_try_timeout: Option<Duration>,
    pub on_5xx: bool,
    pub on_gateway_error: bool,
    pub on_connect_failure: bool,
    pub on_reset: bool,
    pub on_refused_stream: bool,
    pub on_reset_before_request: bool,
    pub on_retriable_status: bool,
    pub retriable_status_codes: Vec<u16>,
    /// gRPC のステータス（cancelled=1, deadline-exceeded=4, resource-exhausted=8, unavailable=14, internal=13）
    pub grpc_codes: Vec<u32>,
}

#[derive(Debug)]
pub struct RedirectCfg {
    pub host: Option<String>,
    pub port: Option<u16>,
    pub scheme: Option<String>,
    pub path: Option<String>,
    pub prefix_rewrite: Option<String>,
    pub strip_query: bool,
    pub status: u16,
}

#[derive(Debug)]
pub struct FaultCfg {
    pub delay: Option<(Duration, u32)>,
    pub abort_http: Option<(u16, u32)>,
    pub abort_grpc: Option<(u32, u32)>,
    pub headers: Vec<HeaderMatch>,
}

#[derive(Debug)]
pub struct CorsCfg {
    pub allow_origins: Vec<StringMatcher>,
    pub allow_methods: Bytes,
    pub allow_headers: Bytes,
    pub expose_headers: Bytes,
    pub max_age: Bytes,
    pub allow_credentials: bool,
}

// ---------------------------------------------------------------------------
// 認可（F-220）
// ---------------------------------------------------------------------------

/// Envoy の RBAC フィルタ（Istio の AuthorizationPolicy）。
#[derive(Debug)]
pub struct RbacCfg {
    /// ALLOW / DENY / LOG
    pub action: RbacAction,
    pub policies: Vec<(String, RbacPolicy)>,
    /// shadow（dry-run）なら判定だけして通す
    pub shadow: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RbacAction {
    Allow,
    Deny,
    Log,
}

#[derive(Debug)]
pub struct RbacPolicy {
    /// いずれかに一致（OR）
    pub permissions: Vec<RbacPermission>,
    pub principals: Vec<RbacPrincipal>,
}

#[derive(Debug)]
pub enum RbacPermission {
    Any,
    And(Vec<RbacPermission>),
    Or(Vec<RbacPermission>),
    Not(Box<RbacPermission>),
    Header(HeaderMatch),
    Path(StringMatcher),
    DestIp(Cidr),
    DestPort(u16),
    Sni(StringMatcher),
}

#[derive(Debug)]
pub enum RbacPrincipal {
    Any,
    And(Vec<RbacPrincipal>),
    Or(Vec<RbacPrincipal>),
    Not(Box<RbacPrincipal>),
    /// 認証済みの主体（SAN の URI 等）。`None` は「認証済みなら誰でも」
    Authenticated(Option<StringMatcher>),
    SourceIp(Cidr),
    DirectRemoteIp(Cidr),
    RemoteIp(Cidr),
    Header(HeaderMatch),
    /// JWT 等のメタデータ（`envoy.filters.http.jwt_authn` の `payload.iss` 等）
    Metadata {
        filter: String,
        path: Vec<String>,
        value: MetaMatch,
    },
}

/// メタデータの値の照合（Envoy の `ValueMatcher`）。
#[derive(Debug)]
pub enum MetaMatch {
    Str(StringMatcher),
    /// 配列のいずれかの要素が一致
    List(Box<MetaMatch>),
    Or(Vec<MetaMatch>),
    Present(bool),
    Bool(bool),
    /// 評価できない（一致しない）
    Never,
}

impl MetaMatch {
    pub fn matches(&self, v: Option<&serde_json::Value>) -> bool {
        match self {
            MetaMatch::Present(p) => v.is_some() == *p,
            MetaMatch::Never => false,
            MetaMatch::Or(list) => list.iter().any(|m| m.matches(v)),
            MetaMatch::Str(m) => {
                matches!(v, Some(serde_json::Value::String(s)) if m.matches(s.as_bytes()))
            }
            MetaMatch::Bool(b) => matches!(v, Some(serde_json::Value::Bool(x)) if x == b),
            MetaMatch::List(inner) => match v {
                Some(serde_json::Value::Array(a)) => a.iter().any(|e| inner.matches(Some(e))),
                _ => false,
            },
        }
    }
}

// ---------------------------------------------------------------------------
// クラスタ・エンドポイント
// ---------------------------------------------------------------------------

/// クラスタの名前ごとのスロット。CDS の更新は `cfg` を差し替え、EDS の更新は `endpoints` を差し替える。
#[derive(Debug)]
pub struct ClusterSlot {
    pub name: String,
    pub cfg: ArcSwap<Option<Arc<ClusterCfg>>>,
    pub endpoints: ArcSwap<EndpointSet>,
    /// 最小要求数（LEAST_REQUEST）用の同時要求数を持つエンドポイントごとの表は EndpointSet 側
    pub stats: ClusterStats,
}

impl ClusterSlot {
    pub fn new(name: &str) -> Self {
        ClusterSlot {
            name: name.to_string(),
            cfg: ArcSwap::from_pointee(None),
            endpoints: ArcSwap::from_pointee(EndpointSet::default()),
            stats: ClusterStats::default(),
        }
    }
}

#[derive(Debug, Default)]
pub struct ClusterStats {
    /// 処理中の要求（サーキットブレーカの上限。`upstream::admit`）
    pub active_rq: AtomicU64,
    pub upstream_rq_total: AtomicU64,
    pub upstream_cx_total: AtomicU64,
    pub upstream_rq_5xx: AtomicU64,
    pub upstream_cx_connect_fail: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClusterKind {
    Eds,
    Static,
    StrictDns,
    OriginalDst,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LbPolicy {
    RoundRobin,
    LeastRequest,
    Random,
    RingHash,
    Maglev,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamProto {
    Http1,
    Http2,
    /// 下流と同じ
    UseDownstream,
}

#[derive(Debug)]
pub struct ClusterCfg {
    pub name: String,
    pub kind: ClusterKind,
    pub lb: LbPolicy,
    pub connect_timeout: Duration,
    pub bind: Option<SocketAddr>,
    /// エンドポイントのメタデータ（`envoy.transport_socket_match`）で選ぶ TLS。順に評価し、最初の一致を使う
    pub tls_matches: Vec<(Vec<(String, String)>, Option<Arc<UpstreamTls>>)>,
    /// `transport_socket`（matches が無いときの既定）
    pub tls_default: Option<Arc<UpstreamTls>>,
    pub proto: UpstreamProto,
    pub idle_timeout: Duration,
    pub max_requests_per_conn: u32,
    pub outlier: Option<OutlierCfg>,
    pub max_connections: u32,
    pub max_pending: u32,
    pub max_requests: u32,
    pub max_retries: u32,
    /// Istio の `metadata.filter_metadata.istio.services`（観測の destination_service 等）
    pub service_host: Bytes,
    pub service_name: Bytes,
    pub service_namespace: Bytes,
    /// EDS のサービス名（EDS の更新を探す鍵）
    pub eds_name: String,
    /// 観測の宛先サービスのラベル（`service_*` から作る）
    pub svc_labels: Option<Arc<crate::mesh::stats::ServiceLabels>>,
    /// ORIGINAL_DST のポートの置き換え（`original_dst_lb_config.upstream_port_override`。HBONE は 15008）
    pub port_override: Option<u16>,
}

impl ClusterCfg {
    /// エンドポイントのメタデータに対応する TLS を選ぶ。
    pub fn tls_for(&self, ep: &MeshEndpoint) -> Option<&Arc<UpstreamTls>> {
        if self.tls_matches.is_empty() {
            return self.tls_default.as_ref();
        }
        for (m, tls) in &self.tls_matches {
            if m.iter()
                .all(|(k, v)| ep.socket_match.iter().any(|(ek, ev)| ek == k && ev == v))
            {
                return tls.as_ref();
            }
        }
        self.tls_default.as_ref()
    }
}

#[derive(Debug, Clone)]
pub struct OutlierCfg {
    pub consecutive_5xx: u32,
    pub consecutive_gateway: u32,
    pub base_ejection: Duration,
    pub max_ejection_percent: u32,
    pub interval: Duration,
}

/// 上流への TLS（mTLS のクライアント側）。ALPN ごとに rustls の設定を持つ。
#[derive(Debug)]
pub struct UpstreamTls {
    pub sni: String,
    pub san_matchers: Vec<StringMatcher>,
    /// xDS の ALPN（TCP の転送で使う）
    pub tcp: Arc<rustls::ClientConfig>,
    /// HTTP/1.1 の要求（Istio は `istio-http/1.1, istio, http/1.1`）
    pub h1: Arc<rustls::ClientConfig>,
    /// HTTP/2 の要求（Istio は `istio-h2, istio, h2`）
    pub h2: Arc<rustls::ClientConfig>,
    /// メッシュの mTLS（Istio の ISTIO_MUTUAL。テレメトリの connection_security_policy）
    pub mesh_mtls: bool,
    /// Linkerd の opaque transport（ALPN `transport.l5d.io/v1`。F-223）
    pub transport: Option<Arc<rustls::ClientConfig>>,
}

#[derive(Debug, Default)]
pub struct EndpointSet {
    pub endpoints: Vec<MeshEndpoint>,
    /// 健康なエンドポイントの添字（優先度 0 のうち健康なもの。全滅なら全員＝パニック）
    pub healthy: Vec<usize>,
    pub rr: AtomicUsize,
    pub total_weight: u64,
}

#[derive(Debug)]
pub struct MeshEndpoint {
    pub addr: SocketAddr,
    pub weight: u32,
    pub healthy: bool,
    pub priority: u32,
    pub locality: (String, String, String),
    /// `envoy.transport_socket_match` のメタデータ
    pub socket_match: Vec<(String, String)>,
    /// Istio の `istio.workload`（"name;namespace;canonical;revision;cluster"）
    pub workload: Bytes,
    /// 内部アドレス（`envoy_internal_address`）の内部リスナーの名前。`addr` はそのとき元の宛先
    /// （メタデータ `envoy.filters.listener.original_dst.local`）
    pub internal: Option<Arc<str>>,
    /// エンドポイントごとの TLS（Linkerd の `TlsIdentity`。クラスタの TLS より優先する）
    pub tls: Option<Arc<UpstreamTls>>,
    /// HTTP/1.1 の要求を HTTP/2 に昇格して送る（Linkerd の `ProtocolHint.H2`、`l5d-orig-proto`）
    pub h2_upgrade: bool,
    /// opaque transport の受け口のポート（Linkerd の `OpaqueTransport.inbound_port`。transport header を送る）
    pub tagged_port: Option<u16>,
    /// 観測のラベル（Prometheus の書式で連結済み。Linkerd の `metric_labels`）
    pub metric_labels: Option<Arc<str>>,
    /// 同時要求数（LEAST_REQUEST）
    pub active: AtomicUsize,
    /// 外れ値検出の状態
    pub consecutive_5xx: AtomicUsize,
    pub ejected_until_ms: AtomicU64,
}

impl MeshEndpoint {
    pub fn new(addr: SocketAddr) -> Self {
        MeshEndpoint {
            addr,
            weight: 1,
            healthy: true,
            priority: 0,
            locality: Default::default(),
            socket_match: Vec::new(),
            workload: Bytes::new(),
            internal: None,
            tls: None,
            h2_upgrade: false,
            tagged_port: None,
            metric_labels: None,
            active: AtomicUsize::new(0),
            consecutive_5xx: AtomicUsize::new(0),
            ejected_until_ms: AtomicU64::new(0),
        }
    }

    pub fn is_ejected(&self, now_ms: u64) -> bool {
        self.ejected_until_ms.load(Ordering::Relaxed) > now_ms
    }
}

impl EndpointSet {
    pub fn new(endpoints: Vec<MeshEndpoint>) -> Self {
        let mut healthy: Vec<usize> = endpoints
            .iter()
            .enumerate()
            .filter(|(_, e)| e.healthy && e.priority == 0)
            .map(|(i, _)| i)
            .collect();
        if healthy.is_empty() {
            // 優先度 0 が全滅なら、健康な次の優先度を使う
            let min_prio = endpoints
                .iter()
                .filter(|e| e.healthy)
                .map(|e| e.priority)
                .min();
            if let Some(p) = min_prio {
                healthy = endpoints
                    .iter()
                    .enumerate()
                    .filter(|(_, e)| e.healthy && e.priority == p)
                    .map(|(i, _)| i)
                    .collect();
            }
        }
        if healthy.is_empty() {
            // パニックモード（全員を対象にする。Envoy の healthy_panic_threshold）
            healthy = (0..endpoints.len()).collect();
        }
        let total_weight = healthy
            .iter()
            .map(|&i| endpoints[i].weight.max(1) as u64)
            .sum();
        EndpointSet {
            endpoints,
            healthy,
            rr: AtomicUsize::new(0),
            total_weight,
        }
    }
}
