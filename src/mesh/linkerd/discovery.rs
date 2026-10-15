//! Linkerd の宛先・ポリシーの購読とメッシュモデルへの変換（F-223）。
//!
//! - outbound: 元の宛先（IP:ポート）ごとに `OutboundPolicies.Watch` を購読し、プロトコル（検出・HTTP・opaque）・
//!   HTTPRoute のルール（一致・フィルタ・バックエンドの重み・タイムアウト・リトライ）・failure accrual を
//!   HTTP の接続管理（[`HttpCm`]）と L4（[`TcpProxyCfg`]）にする。バックエンドは `Destination.Get`
//!   （サービスの authority）を購読してエンドポイント（アドレス・重み・相手の ID・HTTP/2 のヒント・
//!   opaque transport のポート）を [`ClusterSlot`] に入れる。
//! - inbound: ポートごとに `InboundServerPolicies.WatchPort` を購読し、プロトコルと認可（Server・
//!   AuthorizationPolicy・ルート単位の認可）を RBAC にする。
//! - 購読は制御のスレッド（[`super::control`]）で動き、結果を `ArcSwap` に入れる。ワーカーは最初の接続で
//!   依頼して、届くまで少し待つ（届かなければ素通し）。

use super::control::{self, CtlTarget, Request};
use super::env::Config;
use super::proto as p;
use crate::mesh::model::*;
use arc_swap::{ArcSwap, ArcSwapOption};
use bytes::Bytes;
use once_cell::sync::Lazy;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OUTBOUND_WATCH: &str = "/io.linkerd.proxy.outbound.OutboundPolicies/Watch";
const INBOUND_WATCH: &str = "/io.linkerd.proxy.inbound.InboundServerPolicies/WatchPort";
const DST_GET: &str = "/io.linkerd.proxy.destination.Destination/Get";

/// 制御プレーンの購読が黙っていてよい時間（本家の `CONTROL_STREAM_IDLE_TIMEOUT` より長め）
const WATCH_IDLE: Duration = Duration::from_secs(15 * 60);

// ---------------------------------------------------------------------------
// 状態（ワーカーが読む）
// ---------------------------------------------------------------------------

/// outbound の宛先の設定。
pub enum OutCfg {
    /// HTTP（`detect` なら先読みで HTTP でなければ `opaque` へ）
    Http {
        hcm: Arc<HttpCm>,
        detect: bool,
        opaque: Option<Arc<TcpProxyCfg>>,
    },
    Opaque(Arc<TcpProxyCfg>),
}

pub struct OutTarget {
    pub cfg: ArcSwapOption<OutCfg>,
}

/// inbound のポートの設定。
pub struct InCfg {
    pub proto: InProto,
    /// HTTP（ルートと認可）
    pub hcm: Arc<HttpCm>,
    /// L4 の認可（Server の authorizations）
    pub tcp_rbac: Arc<RbacCfg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InProto {
    Detect,
    Opaque,
    Http1,
    Http2,
    Tls,
}

pub struct InPort {
    pub cfg: ArcSwapOption<InCfg>,
}

static OUT: Lazy<ArcSwap<HashMap<SocketAddr, Arc<OutTarget>>>> =
    Lazy::new(|| ArcSwap::from_pointee(HashMap::new()));
static IN: Lazy<ArcSwap<HashMap<u16, Arc<InPort>>>> =
    Lazy::new(|| ArcSwap::from_pointee(HashMap::new()));
/// バックエンド（`Destination.Get` の path・forward の宛先）→ クラスタ（制御のスレッドだけが触る）
static BACKENDS: Lazy<Mutex<HashMap<String, Arc<ClusterSlot>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// 相手の ID → TLS の設定
static TLS: Lazy<Mutex<HashMap<String, Arc<UpstreamTls>>>> =
    Lazy::new(|| Mutex::new(HashMap::new()));
/// 最初の更新が届いたバックエンド（届くまで要求を待たせる。本家の queue と同じ考え方）
static RESOLVED: Lazy<Mutex<std::collections::HashSet<String>>> =
    Lazy::new(|| Mutex::new(std::collections::HashSet::new()));

fn resolved(name: &str) -> bool {
    RESOLVED
        .lock()
        .map(|r| r.contains(name))
        .unwrap_or(true)
}

fn mark_resolved(name: &str) {
    if let Ok(mut r) = RESOLVED.lock() {
        r.insert(name.to_string());
    }
}

/// 設定が参照するバックエンドのクラスタ。
fn backends_of(oc: &OutCfg) -> Vec<Arc<ClusterSlot>> {
    let mut v = Vec::new();
    let mut tcp = |t: &TcpProxyCfg| v.extend(t.clusters.iter().map(|(s, _)| s.clone()));
    let hcm = match oc {
        OutCfg::Http { hcm, opaque, .. } => {
            if let Some(t) = opaque {
                tcp(t);
            }
            Some(hcm)
        }
        OutCfg::Opaque(t) => {
            tcp(t);
            None
        }
    };
    if let Some(RouteRef::Inline(t)) = hcm.map(|h| &h.routes) {
        for vh in &t.vhosts {
            for r in &vh.routes {
                if let RouteAction::Forward(fa) = &r.action {
                    match &fa.target {
                        ClusterTarget::Single(c) => v.push(c.clone()),
                        ClusterTarget::Weighted { clusters, .. } => {
                            v.extend(clusters.iter().map(|w| w.cluster.clone()))
                        }
                        ClusterTarget::Header(_) => {}
                    }
                }
            }
        }
    }
    v
}

async fn wait<T>(slot: &ArcSwapOption<T>, timeout: Duration) -> Option<Arc<T>> {
    let start = std::time::Instant::now();
    loop {
        if let Some(c) = slot.load_full() {
            return Some(c);
        }
        if start.elapsed() >= timeout {
            return None;
        }
        crate::runtime::time::sleep(Duration::from_millis(2)).await;
    }
}

/// outbound の宛先の設定（初めての宛先なら購読を依頼して届くまで待つ）。
pub async fn outbound(dst: SocketAddr) -> Option<Arc<OutCfg>> {
    let t = match OUT.load().get(&dst) {
        Some(t) => t.clone(),
        None => {
            let t = Arc::new(OutTarget {
                cfg: ArcSwapOption::empty(),
            });
            let mut created = None;
            OUT.rcu(|m| {
                let mut m = (**m).clone();
                created = Some(m.entry(dst).or_insert_with(|| t.clone()).clone());
                m
            });
            control::request(Request::Outbound(dst));
            created.unwrap_or(t)
        }
    };
    let start = std::time::Instant::now();
    let oc = wait(&t.cfg, Duration::from_secs(3)).await?;
    // バックエンドのエンドポイントが届くまで待つ（最初の要求を `no healthy upstream` にしない）
    let slots = backends_of(&oc);
    while !slots.iter().all(|s| resolved(&s.name)) && start.elapsed() < Duration::from_secs(3) {
        crate::runtime::time::sleep(Duration::from_millis(2)).await;
    }
    Some(oc)
}

/// inbound のポートの設定。
pub async fn inbound(port: u16) -> Option<Arc<InCfg>> {
    let t = match IN.load().get(&port) {
        Some(t) => t.clone(),
        None => {
            let t = Arc::new(InPort {
                cfg: ArcSwapOption::empty(),
            });
            let mut created = None;
            IN.rcu(|m| {
                let mut m = (**m).clone();
                created = Some(m.entry(port).or_insert_with(|| t.clone()).clone());
                m
            });
            control::request(Request::Inbound(port));
            created.unwrap_or(t)
        }
    };
    wait(&t.cfg, Duration::from_secs(3)).await
}

/// 接続の途中でポリシーが更新されたときの現在の HTTP の設定（要求ごとに呼ぶ。表を 1 回引くだけ）。
pub fn current_hcm(direction: Direction, dst: SocketAddr) -> Option<Arc<HttpCm>> {
    match direction {
        Direction::Inbound => IN
            .load()
            .get(&dst.port())
            .and_then(|t| t.cfg.load_full())
            .map(|c| c.hcm.clone()),
        _ => match OUT.load().get(&dst).and_then(|t| t.cfg.load_full()).as_deref() {
            Some(OutCfg::Http { hcm, .. }) => Some(hcm.clone()),
            _ => None,
        },
    }
}

/// 管理 API 用: 購読中の宛先・ポートの数。
pub fn counts() -> (usize, usize, usize) {
    (
        OUT.load().len(),
        IN.load().len(),
        BACKENDS.lock().map(|b| b.len()).unwrap_or(0),
    )
}

// ---------------------------------------------------------------------------
// 購読（制御のスレッド）
// ---------------------------------------------------------------------------

fn policy_target(cfg: &Config) -> CtlTarget {
    CtlTarget {
        addr: cfg.policy_addr.clone(),
        name: cfg.policy_name.clone(),
        client_cert: true,
    }
}

async fn wait_identity() {
    while !super::identity::READY.load(Ordering::Relaxed) {
        crate::runtime::time::sleep(Duration::from_millis(50)).await;
    }
}

pub async fn watch_outbound(cfg: Arc<Config>, dst: SocketAddr) {
    let Some(t) = OUT.load().get(&dst).cloned() else {
        return;
    };
    wait_identity().await;
    let ct = policy_target(&cfg);
    let req = p::TrafficSpec {
        source_workload: cfg.workload.clone(),
        target: Some(p::traffic_spec::Target::Addr(p::TcpAddress {
            ip: Some(p::IpAddress::from_ip(dst.ip())),
            port: dst.port() as u32,
        })),
    };
    let mut backoff = Duration::from_millis(100);
    loop {
        if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            return;
        }
        match control::call(&ct, OUTBOUND_WATCH, &req).await {
            Ok(mut c) => loop {
                match c.next::<p::OutboundPolicy>(WATCH_IDLE).await {
                    Ok(Some(pol)) => {
                        backoff = Duration::from_millis(100);
                        match translate_outbound(&cfg, dst, &pol) {
                            Ok(oc) => t.cfg.store(Some(Arc::new(oc))),
                            Err(e) => ftlog::warn!("[linkerd] outbound policy for {}: {}", dst, e),
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        ftlog::debug!("[linkerd] outbound watch {}: {}", dst, e);
                        break;
                    }
                }
            },
            Err(e) => ftlog::warn!("[linkerd] outbound watch {}: {}", dst, e),
        }
        // 届かないうちは素通しにして待たせない
        if t.cfg.load().is_none() {
            t.cfg.store(Some(Arc::new(OutCfg::Opaque(forward_tcp(&cfg, dst)))));
        }
        crate::runtime::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

pub async fn watch_inbound(cfg: Arc<Config>, port: u16) {
    let Some(t) = IN.load().get(&port).cloned() else {
        return;
    };
    wait_identity().await;
    let ct = policy_target(&cfg);
    let req = p::PortSpec {
        workload: cfg.workload.clone(),
        port: port as u32,
    };
    let mut backoff = Duration::from_millis(100);
    loop {
        if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            return;
        }
        match control::call(&ct, INBOUND_WATCH, &req).await {
            Ok(mut c) => loop {
                match c.next::<p::Server>(WATCH_IDLE).await {
                    Ok(Some(srv)) => {
                        backoff = Duration::from_millis(100);
                        match translate_inbound(&cfg, port, &srv) {
                            Ok(ic) => {
                                if let RouteRef::Inline(rt) = &ic.hcm.routes {
                                    ftlog::debug!(
                                        "[linkerd] inbound {}: {:?} routes {:?}, {} server authorizations",
                                        port,
                                        ic.proto,
                                        rt.vhosts
                                            .iter()
                                            .flat_map(|v| v.routes.iter())
                                            .map(|r| format!("{} {:?}", r.name, r.path))
                                            .collect::<Vec<_>>(),
                                        srv.authorizations.len()
                                    );
                                }
                                t.cfg.store(Some(Arc::new(ic)))
                            }
                            Err(e) => ftlog::warn!("[linkerd] inbound policy for {}: {}", port, e),
                        }
                    }
                    Ok(None) => break,
                    Err(e) => {
                        ftlog::debug!("[linkerd] inbound watch {}: {}", port, e);
                        break;
                    }
                }
            },
            Err(e) => ftlog::warn!("[linkerd] inbound watch {}: {}", port, e),
        }
        if t.cfg.load().is_none() {
            t.cfg.store(Some(Arc::new(default_inbound(&cfg, port))));
        }
        crate::runtime::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

/// `Destination.Get` を購読してクラスタのエンドポイントを更新する。
async fn watch_destination(cfg: Arc<Config>, path: String, slot: Arc<ClusterSlot>) {
    wait_identity().await;
    let ct = CtlTarget {
        addr: cfg.dst_addr.clone(),
        name: cfg.dst_name.clone(),
        client_cert: true,
    };
    let req = p::GetDestination {
        scheme: "k8s".into(),
        path: path.clone(),
        context_token: cfg.dst_context.clone(),
    };
    let mut backoff = Duration::from_millis(100);
    loop {
        if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            return;
        }
        let mut current: HashMap<SocketAddr, (p::WeightedAddr, Arc<HashMap<String, String>>)> =
            HashMap::new();
        match control::call(&ct, DST_GET, &req).await {
            Ok(mut c) => loop {
                match c.next::<p::Update>(WATCH_IDLE).await {
                    Ok(Some(u)) => {
                        backoff = Duration::from_millis(100);
                        match u.update {
                            Some(p::update::Update::Add(set)) => {
                                let labels = Arc::new(set.metric_labels);
                                for wa in set.addrs {
                                    if let Some(a) = wa.addr.as_ref().and_then(|a| a.to_addr()) {
                                        current.insert(a, (wa, labels.clone()));
                                    }
                                }
                            }
                            Some(p::update::Update::Remove(set)) => {
                                for a in set.addrs.iter().filter_map(|a| a.to_addr()) {
                                    current.remove(&a);
                                }
                            }
                            Some(p::update::Update::NoEndpoints(_)) => current.clear(),
                            None => continue,
                        }
                        let eps: Vec<MeshEndpoint> = current
                            .values()
                            .filter_map(|(wa, labels)| endpoint(wa, labels))
                            .collect();
                        ftlog::debug!("[linkerd] {}: {} endpoints", path, eps.len());
                        slot.endpoints.store(Arc::new(EndpointSet::new(eps)));
                        mark_resolved(&slot.name);
                    }
                    Ok(None) => break,
                    Err(e) => {
                        ftlog::debug!("[linkerd] destination {}: {}", path, e);
                        break;
                    }
                }
            },
            Err(e) => ftlog::warn!("[linkerd] destination {}: {}", path, e),
        }
        crate::runtime::time::sleep(backoff).await;
        backoff = (backoff * 2).min(Duration::from_secs(10));
    }
}

// ---------------------------------------------------------------------------
// エンドポイント・TLS
// ---------------------------------------------------------------------------

/// 相手の ID（と SNI）の mTLS の設定。HTTP は ALPN なし（HTTP/2 はプリフェースで判別）、opaque transport は
/// `transport.l5d.io/v1`。
fn tls_for(identity: &str, sni: &str) -> Option<Arc<UpstreamTls>> {
    let key = format!("{}|{}", identity, sni);
    let mut cache = TLS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(t) = cache.get(&key) {
        return Some(t.clone());
    }
    let san = vec![StringMatcher::exact(identity)];
    let mk = |alpn: &[&[u8]]| {
        crate::mesh::identity::client_config(Some(super::CERT), Some(super::ROOTS), san.clone(), alpn)
            .ok()
    };
    let plain = mk(&[])?;
    let t = Arc::new(UpstreamTls {
        sni: sni.to_string(),
        san_matchers: san.clone(),
        tcp: plain.clone(),
        h1: plain.clone(),
        h2: plain,
        mesh_mtls: true,
        transport: mk(&[p::TRANSPORT_ALPN]),
    });
    cache.insert(key, t.clone());
    Some(t)
}

fn endpoint(wa: &p::WeightedAddr, set_labels: &HashMap<String, String>) -> Option<MeshEndpoint> {
    let addr = wa.addr.as_ref()?.to_addr()?;
    let mut me = MeshEndpoint::new(addr);
    me.weight = wa.weight.max(1);
    if let Some(id) = &wa.tls_identity {
        let name = match &id.strategy {
            Some(p::tls_identity::Strategy::DnsLikeIdentity(d)) => Some(d.name.clone()),
            Some(p::tls_identity::Strategy::UriLikeIdentity(u)) => Some(u.uri.clone()),
            None => None,
        };
        if let Some(name) = name {
            let sni = id
                .server_name
                .as_ref()
                .map(|s| s.name.clone())
                .unwrap_or_else(|| name.clone());
            me.tls = tls_for(&name, &sni);
        }
    }
    if let Some(h) = &wa.protocol_hint {
        me.h2_upgrade = matches!(h.protocol, Some(p::protocol_hint::Protocol::H2(_)));
        if me.tls.is_some() {
            me.tagged_port = h
                .opaque_transport
                .as_ref()
                .map(|o| o.inbound_port as u16)
                .filter(|p| *p != 0);
        }
    }
    let mut labels: Vec<(&String, &String)> = set_labels.iter().chain(wa.metric_labels.iter()).collect();
    labels.sort();
    labels.dedup_by(|a, b| a.0 == b.0);
    if !labels.is_empty() {
        let s = labels
            .iter()
            .map(|(k, v)| format!("dst_{}=\"{}\"", k, v.replace('"', "")))
            .collect::<Vec<_>>()
            .join(",");
        me.metric_labels = Some(Arc::from(s.as_str()));
    }
    Some(me)
}

fn backend_cfg(cfg: &Config, name: &str, outlier: Option<OutlierCfg>) -> ClusterCfg {
    ClusterCfg {
        name: name.to_string(),
        kind: ClusterKind::Eds,
        lb: LbPolicy::LeastRequest,
        connect_timeout: cfg.outbound_connect_timeout,
        bind: None,
        tls_matches: Vec::new(),
        tls_default: None,
        proto: UpstreamProto::UseDownstream,
        idle_timeout: Duration::from_secs(60),
        max_requests_per_conn: 0,
        outlier,
        max_connections: u32::MAX / 4,
        max_pending: u32::MAX / 4,
        max_requests: u32::MAX / 4,
        max_retries: u32::MAX / 4,
        service_host: Bytes::from(name.split(':').next().unwrap_or("").to_string()),
        service_name: Bytes::new(),
        service_namespace: Bytes::new(),
        eds_name: name.to_string(),
        svc_labels: None,
        port_override: None,
    }
}

/// バックエンドのクラスタ（`Destination.Get` を購読する。forward は宛先 1 つ）。
fn backend_slot(cfg: &Arc<Config>, b: &p::Backend, outlier: Option<OutlierCfg>) -> Option<Arc<ClusterSlot>> {
    let (key, path, forward) = match &b.kind {
        Some(p::backend::Kind::Balancer(bal)) => {
            let path = bal.discovery.as_ref()?.dst.as_ref()?.path.clone();
            (path.clone(), Some(path), None)
        }
        Some(p::backend::Kind::Forward(wa)) => {
            let a = wa.addr.as_ref()?.to_addr()?;
            (format!("forward:{}", a), None, Some(wa.clone()))
        }
        None => return None,
    };
    let mut backends = BACKENDS.lock().unwrap_or_else(|e| e.into_inner());
    let slot = match backends.get(&key) {
        Some(s) => s.clone(),
        None => {
            let s = Arc::new(ClusterSlot::new(&key));
            backends.insert(key.clone(), s.clone());
            if let Some(path) = path {
                crate::runtime::spawn(watch_destination(cfg.clone(), path, s.clone()));
            }
            if let Some(wa) = &forward {
                if let Some(ep) = endpoint(wa, &HashMap::new()) {
                    s.endpoints.store(Arc::new(EndpointSet::new(vec![ep])));
                }
                mark_resolved(&key);
            }
            s
        }
    };
    slot.cfg
        .store(Arc::new(Some(Arc::new(backend_cfg(cfg, &key, outlier)))));
    Some(slot)
}

/// 元の宛先へそのまま送る（ポリシーが無い宛先・購読の失敗）。
fn forward_tcp(cfg: &Config, dst: SocketAddr) -> Arc<TcpProxyCfg> {
    let key = format!("forward:{}", dst);
    let mut backends = BACKENDS.lock().unwrap_or_else(|e| e.into_inner());
    let slot = backends
        .entry(key.clone())
        .or_insert_with(|| {
            let s = Arc::new(ClusterSlot::new(&key));
            s.endpoints
                .store(Arc::new(EndpointSet::new(vec![MeshEndpoint::new(dst)])));
            s.cfg
                .store(Arc::new(Some(Arc::new(backend_cfg(cfg, &key, None)))));
            mark_resolved(&key);
            s
        })
        .clone();
    Arc::new(tcp_cfg(vec![(slot, 1)]))
}

fn tcp_cfg(clusters: Vec<(Arc<ClusterSlot>, u32)>) -> TcpProxyCfg {
    let total_weight = clusters.iter().map(|(_, w)| *w).sum();
    TcpProxyCfg {
        stat_prefix: "outbound".into(),
        clusters,
        total_weight,
        idle_timeout: None,
        rbac: None,
        access_logs: Vec::new(),
        tunnel: None,
    }
}

// ---------------------------------------------------------------------------
// outbound の変換
// ---------------------------------------------------------------------------

fn dur(d: &Option<p::Duration>) -> Option<Duration> {
    let d = d.as_ref()?;
    let v = Duration::new(d.seconds.max(0) as u64, d.nanos.max(0) as u32);
    (!v.is_zero()).then_some(v)
}

fn outlier(fa: Option<&p::FailureAccrual>) -> Option<OutlierCfg> {
    let (max, backoff) = match fa?.kind.as_ref()? {
        p::failure_accrual::Kind::ConsecutiveFailures(c) => (c.max_failures, c.backoff.as_ref()),
        p::failure_accrual::Kind::Unified(u) => (u.max_consecutive_failures, u.backoff.as_ref()),
    };
    if max == 0 {
        return None;
    }
    Some(OutlierCfg {
        consecutive_5xx: max,
        consecutive_gateway: max,
        base_ejection: backoff
            .and_then(|b| dur(&b.min_backoff))
            .unwrap_or(Duration::from_secs(1)),
        max_ejection_percent: 100,
        interval: Duration::from_secs(1),
    })
}

/// ルートのバックエンドの分配をクラスタの選び方にする。
fn distribution(
    cfg: &Arc<Config>,
    d: Option<&p::Distribution>,
    out: Option<OutlierCfg>,
) -> Vec<(Arc<ClusterSlot>, u32)> {
    match d.and_then(|d| d.kind.as_ref()) {
        Some(p::distribution::Kind::FirstAvailable(f)) => f
            .backends
            .iter()
            .filter_map(|rb| rb.backend.as_ref())
            .filter_map(|b| backend_slot(cfg, b, out.clone()))
            .take(1)
            .map(|s| (s, 1))
            .collect(),
        Some(p::distribution::Kind::RandomAvailable(r)) => r
            .backends
            .iter()
            .filter_map(|wb| {
                let b = wb.backend.as_ref()?.backend.as_ref()?;
                Some((backend_slot(cfg, b, out.clone())?, wb.weight))
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn header_ops(m: &p::HeaderModifier) -> HeaderOps {
    let mut ops = HeaderOps::default();
    for n in &m.remove {
        ops.remove.push(Bytes::from(n.to_ascii_lowercase()));
    }
    for (hs, action) in [(&m.set, AppendAction::Overwrite), (&m.add, AppendAction::Append)] {
        if let Some(hs) = hs {
            for h in &hs.headers {
                ops.add.push(HeaderAdd {
                    name: Bytes::from(h.name.to_ascii_lowercase()),
                    value: Bytes::from(h.value.clone()),
                    action,
                });
            }
        }
    }
    ops
}

fn ratio_per_million(r: &Option<p::Ratio>) -> u32 {
    match r {
        Some(r) if r.denominator > 0 => {
            ((r.numerator as u64 * 1_000_000) / r.denominator as u64).min(1_000_000) as u32
        }
        _ => 1_000_000,
    }
}

fn regex(re: &str) -> Result<regex::Regex, String> {
    regex::Regex::new(&format!("^(?:{})$", re)).map_err(|e| format!("regex {}: {}", re, e))
}

/// HTTPRoute の一致を [`MeshRoute`] の条件にする（パス・メソッド・ヘッダ・クエリ）。
fn route_match(m: Option<&p::HttpRouteMatch>) -> Result<(PathMatch, Vec<HeaderMatch>, Vec<QueryMatch>), String> {
    let Some(m) = m else {
        return Ok((PathMatch::Any, Vec::new(), Vec::new()));
    };
    let path = match m.path.as_ref().and_then(|p| p.kind.as_ref()) {
        Some(p::path_match::Kind::Exact(s)) => PathMatch::Exact(s.clone()),
        Some(p::path_match::Kind::Prefix(s)) if s.is_empty() || s == "/" => PathMatch::Any,
        // Gateway API の PathPrefix は区切り単位（`/a` は `/a`・`/a/..`）
        Some(p::path_match::Kind::Prefix(s)) => PathMatch::Separated(s.trim_end_matches('/').to_string()),
        Some(p::path_match::Kind::Regex(r)) => PathMatch::Regex(regex(r)?),
        None => PathMatch::Any,
    };
    let mut headers = Vec::new();
    if let Some(method) = m.method.as_ref().and_then(|m| m.as_str()) {
        headers.push(HeaderMatch {
            name: ":method".into(),
            kind: HeaderMatchKind::Str(StringMatcher::exact(&method)),
            invert: false,
            treat_missing_as_empty: false,
        });
    }
    for h in &m.headers {
        let sm = match &h.value {
            Some(p::header_match::Value::Exact(v)) => {
                StringMatcher::exact(&String::from_utf8_lossy(v))
            }
            Some(p::header_match::Value::Regex(r)) => StringMatcher {
                pattern: StrMatch::Regex(regex(r)?),
                ignore_case: false,
            },
            None => continue,
        };
        headers.push(HeaderMatch {
            name: h.name.to_ascii_lowercase(),
            kind: HeaderMatchKind::Str(sm),
            invert: false,
            treat_missing_as_empty: false,
        });
    }
    let mut query = Vec::new();
    for q in &m.query_params {
        let value = match &q.value {
            Some(p::query_param_match::Value::Exact(v)) => Some(StringMatcher::exact(v)),
            Some(p::query_param_match::Value::Regex(r)) => Some(StringMatcher {
                pattern: StrMatch::Regex(regex(r)?),
                ignore_case: false,
            }),
            None => None,
        };
        query.push(QueryMatch {
            name: q.name.clone(),
            value,
        });
    }
    Ok((path, headers, query))
}

/// Gateway API の優先順（完全一致のパス → 長い接頭辞 → メソッド → ヘッダの数 → クエリの数）。
fn precedence(r: &MeshRoute) -> (u8, usize, usize, usize) {
    let (kind, len) = match &r.path {
        PathMatch::Exact(s) => (3, s.len()),
        PathMatch::Separated(s) | PathMatch::Prefix(s) => (2, s.len()),
        PathMatch::Regex(re) => (1, re.as_str().len()),
        _ => (0, 0),
    };
    (kind, len, r.headers.len(), r.query.len())
}

fn base_route(name: &str, action: RouteAction) -> MeshRoute {
    MeshRoute {
        name: name.to_string(),
        path: PathMatch::Any,
        case_sensitive: true,
        headers: Vec::new(),
        query: Vec::new(),
        fraction: None,
        grpc_only: false,
        action,
        request_ops: HeaderOps::default(),
        response_ops: HeaderOps::default(),
        operation: Bytes::new(),
        fault: None,
        cors: None,
        rbac: None,
    }
}

fn forward_action(
    clusters: Vec<(Arc<ClusterSlot>, u32)>,
    timeouts: Option<&p::Timeouts>,
    request_timeout: &Option<p::Duration>,
    retry: Option<Arc<RetryPolicy>>,
) -> Option<ForwardAction> {
    let target = match clusters.len() {
        0 => return None,
        1 => ClusterTarget::Single(clusters[0].0.clone()),
        _ => {
            let total = clusters.iter().map(|(_, w)| *w).sum::<u32>().max(1);
            ClusterTarget::Weighted {
                clusters: clusters
                    .into_iter()
                    .map(|(cluster, weight)| WeightedTarget {
                        cluster,
                        weight,
                        request_ops: HeaderOps::default(),
                        response_ops: HeaderOps::default(),
                    })
                    .collect(),
                total,
            }
        }
    };
    let timeout = timeouts
        .and_then(|t| dur(&t.request).or_else(|| dur(&t.response)))
        .or_else(|| dur(request_timeout));
    Some(ForwardAction {
        target,
        timeout,
        idle_timeout: timeouts.and_then(|t| dur(&t.idle)),
        retry,
        prefix_rewrite: None,
        regex_rewrite: None,
        host_rewrite: None,
        auto_host_rewrite: false,
        mirrors: Vec::new(),
        hash: Vec::new(),
        websocket: Some(true),
        connect_terminate: false,
    })
}

fn http_retry(r: &p::HttpRetry) -> Option<Arc<RetryPolicy>> {
    if r.max_retries == 0 {
        return None;
    }
    let mut codes = Vec::new();
    for sr in r.conditions.as_ref().map(|c| c.status_ranges.as_slice()).unwrap_or(&[]) {
        let end = if sr.end == 0 { sr.start } else { sr.end };
        for c in sr.start..=end.min(599) {
            codes.push(c as u16);
        }
    }
    let on_5xx = codes.is_empty();
    Some(Arc::new(RetryPolicy {
        num_retries: r.max_retries,
        per_try_timeout: dur(&r.timeout),
        on_5xx,
        on_gateway_error: false,
        on_connect_failure: true,
        on_reset: false,
        on_refused_stream: true,
        on_reset_before_request: true,
        on_retriable_status: !codes.is_empty(),
        retriable_status_codes: codes,
        grpc_codes: Vec::new(),
    }))
}

fn grpc_retry(r: &p::GrpcRetry) -> Option<Arc<RetryPolicy>> {
    if r.max_retries == 0 {
        return None;
    }
    let c = r.conditions.clone().unwrap_or_default();
    let mut codes = Vec::new();
    for (on, code) in [
        (c.cancelled, 1),
        (c.deadline_exceeded, 4),
        (c.resource_exhausted, 8),
        (c.internal, 13),
        (c.unavailable, 14),
    ] {
        if on {
            codes.push(code);
        }
    }
    Some(Arc::new(RetryPolicy {
        num_retries: r.max_retries,
        per_try_timeout: dur(&r.timeout),
        on_5xx: false,
        on_gateway_error: false,
        on_connect_failure: true,
        on_reset: false,
        on_refused_stream: true,
        on_reset_before_request: true,
        on_retriable_status: false,
        retriable_status_codes: Vec::new(),
        grpc_codes: codes,
    }))
}

/// HTTP のフィルタをルートに入れる（失敗の注入・ヘッダ・リダイレクト）。
fn apply_http_filters(r: &mut MeshRoute, filters: &[p::HttpFilter]) {
    for f in filters {
        match &f.kind {
            Some(p::http_filter::Kind::RequestHeaderModifier(m)) => {
                let ops = header_ops(m);
                r.request_ops.remove.extend(ops.remove);
                r.request_ops.add.extend(ops.add);
            }
            Some(p::http_filter::Kind::ResponseHeaderModifier(m)) => {
                let ops = header_ops(m);
                r.response_ops.remove.extend(ops.remove);
                r.response_ops.add.extend(ops.add);
            }
            Some(p::http_filter::Kind::FailureInjector(fi)) => {
                r.fault = Some(Arc::new(FaultCfg {
                    delay: None,
                    abort_http: Some((fi.status.clamp(100, 599) as u16, ratio_per_million(&fi.ratio))),
                    abort_grpc: None,
                    headers: Vec::new(),
                }));
            }
            Some(p::http_filter::Kind::Redirect(rd)) => {
                r.action = RouteAction::Redirect(Arc::new(RedirectCfg {
                    host: (!rd.host.is_empty()).then(|| rd.host.clone()),
                    port: (rd.port != 0).then_some(rd.port as u16),
                    scheme: rd.scheme.as_ref().and_then(|s| match &s.r#type {
                        Some(p::scheme::Type::Registered(0)) => Some("http".to_string()),
                        Some(p::scheme::Type::Registered(_)) => Some("https".to_string()),
                        Some(p::scheme::Type::Unregistered(u)) => Some(u.clone()),
                        None => None,
                    }),
                    path: match rd.path.as_ref().and_then(|p| p.replace.as_ref()) {
                        Some(p::path_modifier::Replace::Full(f)) => Some(f.clone()),
                        _ => None,
                    },
                    prefix_rewrite: match rd.path.as_ref().and_then(|p| p.replace.as_ref()) {
                        Some(p::path_modifier::Replace::Prefix(f)) => Some(f.clone()),
                        _ => None,
                    },
                    strip_query: false,
                    status: if rd.status == 0 { 302 } else { rd.status as u16 },
                }));
            }
            None => {}
        }
    }
}

fn route_table(name: &str, mut routes: Vec<MeshRoute>) -> RouteTable {
    routes.sort_by_key(|r| std::cmp::Reverse(precedence(r)));
    let mut t = RouteTable::empty(name);
    t.vhosts.push(VirtualHost {
        name: "*".into(),
        routes,
        request_ops: HeaderOps::default(),
        response_ops: HeaderOps::default(),
        include_attempt_count: false,
        retry: None,
        cors: None,
        fault: None,
    });
    t.catch_all = Some(0);
    t
}

/// Linkerd の HTTP の接続管理（Envoy 固有のヘッダを付けない）。
pub fn http_cm(name: &str, table: RouteTable, direction: Direction, filters: Vec<HttpFilterCfg>) -> HttpCm {
    HttpCm {
        stat_prefix: name.to_string(),
        routes: RouteRef::Inline(Arc::new(table)),
        codec: Codec::Auto,
        server_name: Bytes::new(),
        xfcc: XfccMode::AlwaysForwardOnly,
        xfcc_subject: false,
        xfcc_uri: false,
        xfcc_dns: false,
        generate_request_id: false,
        use_remote_address: false,
        websocket: true,
        filters,
        peer_metadata: false,
        istio_alpn: false,
        direction,
        stream_idle_timeout: None,
        trace_sampling: 0,
        access_logs: Vec::new(),
        tracer: None,
        linkerd: true,
    }
}

fn outbound_http(
    cfg: &Arc<Config>,
    dst: SocketAddr,
    h: &p::OutboundHttp,
) -> Result<Arc<HttpCm>, String> {
    let out = outlier(h.failure_accrual.as_ref());
    let mut routes = Vec::new();
    for r in &h.routes {
        let rname = r.metadata.as_ref().map(|m| m.name()).unwrap_or_default();
        for rule in &r.rules {
            let clusters = distribution(cfg, rule.backends.as_ref(), out.clone());
            let retry = rule.retry.as_ref().and_then(http_retry);
            let action = match forward_action(clusters, rule.timeouts.as_ref(), &rule.request_timeout, retry) {
                Some(fa) => RouteAction::Forward(Arc::new(fa)),
                None => RouteAction::Direct {
                    status: 500,
                    body: Bytes::from_static(b"no backends"),
                },
            };
            let matches: Vec<Option<&p::HttpRouteMatch>> = if rule.matches.is_empty() {
                vec![None]
            } else {
                rule.matches.iter().map(Some).collect()
            };
            for m in matches {
                let (path, headers, query) = route_match(m)?;
                let mut mr = base_route(&rname, clone_action(&action));
                mr.path = path;
                mr.headers = headers;
                mr.query = query;
                apply_http_filters(&mut mr, &rule.filters);
                routes.push(mr);
            }
        }
    }
    Ok(Arc::new(http_cm(
        &format!("outbound:{}", dst),
        route_table(&dst.to_string(), routes),
        Direction::Outbound,
        Vec::new(),
    )))
}

fn outbound_grpc(cfg: &Arc<Config>, dst: SocketAddr, g: &p::OutboundGrpc) -> Result<Arc<HttpCm>, String> {
    let out = outlier(g.failure_accrual.as_ref());
    let mut routes = Vec::new();
    for r in &g.routes {
        let rname = r.metadata.as_ref().map(|m| m.name()).unwrap_or_default();
        for rule in &r.rules {
            let clusters = distribution(cfg, rule.backends.as_ref(), out.clone());
            let retry = rule.retry.as_ref().and_then(grpc_retry);
            let action = match forward_action(clusters, rule.timeouts.as_ref(), &rule.request_timeout, retry) {
                Some(fa) => RouteAction::Forward(Arc::new(fa)),
                None => RouteAction::Direct {
                    status: 500,
                    body: Bytes::from_static(b"no backends"),
                },
            };
            let matches: Vec<Option<&p::GrpcRouteMatch>> = if rule.matches.is_empty() {
                vec![None]
            } else {
                rule.matches.iter().map(Some).collect()
            };
            for m in matches {
                let mut mr = base_route(&rname, clone_action(&action));
                if let Some(m) = m {
                    grpc_match(&mut mr, m)?;
                }
                routes.push(mr);
            }
        }
    }
    Ok(Arc::new(http_cm(
        &format!("outbound:{}", dst),
        route_table(&dst.to_string(), routes),
        Direction::Outbound,
        Vec::new(),
    )))
}

fn grpc_match(mr: &mut MeshRoute, m: &p::GrpcRouteMatch) -> Result<(), String> {
    if let Some(rpc) = &m.rpc {
        mr.path = match (rpc.service.is_empty(), rpc.method.is_empty()) {
            (false, false) => PathMatch::Exact(format!("/{}/{}", rpc.service, rpc.method)),
            (false, true) => PathMatch::Prefix(format!("/{}/", rpc.service)),
            (true, false) => PathMatch::Regex(regex(&format!("/[^/]+/{}", regex::escape(&rpc.method)))?),
            (true, true) => PathMatch::Any,
        };
    }
    for h in &m.headers {
        let sm = match &h.value {
            Some(p::header_match::Value::Exact(v)) => StringMatcher::exact(&String::from_utf8_lossy(v)),
            Some(p::header_match::Value::Regex(r)) => StringMatcher {
                pattern: StrMatch::Regex(regex(r)?),
                ignore_case: false,
            },
            None => continue,
        };
        mr.headers.push(HeaderMatch {
            name: h.name.to_ascii_lowercase(),
            kind: HeaderMatchKind::Str(sm),
            invert: false,
            treat_missing_as_empty: false,
        });
    }
    Ok(())
}

fn clone_action(a: &RouteAction) -> RouteAction {
    match a {
        RouteAction::Forward(f) => RouteAction::Forward(f.clone()),
        RouteAction::Redirect(r) => RouteAction::Redirect(r.clone()),
        RouteAction::Direct { status, body } => RouteAction::Direct {
            status: *status,
            body: body.clone(),
        },
        RouteAction::NonForwarding => RouteAction::NonForwarding,
    }
}

fn outbound_opaque(cfg: &Arc<Config>, dst: SocketAddr, o: &p::OutboundOpaque) -> Arc<TcpProxyCfg> {
    let clusters = o
        .routes
        .iter()
        .flat_map(|r| r.rules.iter())
        .map(|rule| distribution(cfg, rule.backends.as_ref(), None))
        .find(|c| !c.is_empty())
        .unwrap_or_default();
    if clusters.is_empty() {
        return forward_tcp(cfg, dst);
    }
    Arc::new(tcp_cfg(clusters))
}

fn translate_outbound(cfg: &Arc<Config>, dst: SocketAddr, pol: &p::OutboundPolicy) -> Result<OutCfg, String> {
    use p::outbound_protocol::Kind as K;
    match pol.protocol.as_ref().and_then(|p| p.kind.as_ref()) {
        Some(K::Detect(d)) => {
            let http = d.http1.as_ref().or(d.http2.as_ref());
            let opaque = d.opaque.as_ref().map(|o| outbound_opaque(cfg, dst, o));
            match http {
                Some(h) => Ok(OutCfg::Http {
                    hcm: outbound_http(cfg, dst, h)?,
                    detect: true,
                    opaque,
                }),
                None => Ok(OutCfg::Opaque(opaque.unwrap_or_else(|| forward_tcp(cfg, dst)))),
            }
        }
        Some(K::Http1(h)) | Some(K::Http2(h)) => Ok(OutCfg::Http {
            hcm: outbound_http(cfg, dst, h)?,
            detect: false,
            opaque: None,
        }),
        Some(K::Grpc(g)) => Ok(OutCfg::Http {
            hcm: outbound_grpc(cfg, dst, g)?,
            detect: false,
            opaque: None,
        }),
        Some(K::Opaque(o)) => Ok(OutCfg::Opaque(outbound_opaque(cfg, dst, o))),
        Some(K::Tls(t)) => {
            let o = p::OutboundOpaque {
                routes: t
                    .routes
                    .iter()
                    .map(|r| p::OpaqueRoute {
                        metadata: r.metadata.clone(),
                        rules: r.rules.clone(),
                    })
                    .collect(),
            };
            Ok(OutCfg::Opaque(outbound_opaque(cfg, dst, &o)))
        }
        None => Ok(OutCfg::Opaque(forward_tcp(cfg, dst))),
    }
}

// ---------------------------------------------------------------------------
// inbound の変換
// ---------------------------------------------------------------------------

fn network_principal(n: &p::Network) -> Option<RbacPrincipal> {
    let net = n.net.as_ref()?;
    let ip = net.ip.as_ref()?.to_ip()?;
    let inc = RbacPrincipal::DirectRemoteIp(Cidr {
        addr: ip,
        len: net.prefix_len as u8,
    });
    if n.except.is_empty() {
        return Some(inc);
    }
    let mut v = vec![inc];
    for e in &n.except {
        if let Some(ip) = e.ip.as_ref().and_then(|i| i.to_ip()) {
            v.push(RbacPrincipal::Not(Box::new(RbacPrincipal::DirectRemoteIp(Cidr {
                addr: ip,
                len: e.prefix_len as u8,
            }))));
        }
    }
    Some(RbacPrincipal::And(v))
}

fn authn_principal(a: Option<&p::Authn>) -> RbacPrincipal {
    match a.and_then(|a| a.permit.as_ref()) {
        Some(p::authn::Permit::Unauthenticated(_)) | None => RbacPrincipal::Any,
        Some(p::authn::Permit::MeshTls(m)) => match &m.clients {
            Some(p::permit_mesh_tls::Clients::Identities(ids)) => {
                let mut v: Vec<RbacPrincipal> = ids
                    .identities
                    .iter()
                    .map(|i| RbacPrincipal::Authenticated(Some(StringMatcher::exact(&i.name))))
                    .collect();
                for s in &ids.suffixes {
                    if s.parts.is_empty() {
                        v.push(RbacPrincipal::Authenticated(None));
                    } else {
                        v.push(RbacPrincipal::Authenticated(Some(StringMatcher {
                            pattern: StrMatch::Suffix(format!(".{}", s.parts.join("."))),
                            ignore_case: true,
                        })));
                    }
                }
                RbacPrincipal::Or(v)
            }
            // mTLS なら誰でも（クライアント証明書が無い TLS も含む）
            _ => RbacPrincipal::Authenticated(None),
        },
    }
}

fn authz_policy(a: &p::Authz) -> RbacPolicy {
    let nets: Vec<RbacPrincipal> = a.networks.iter().filter_map(network_principal).collect();
    let net = if nets.is_empty() {
        RbacPrincipal::Any
    } else {
        RbacPrincipal::Or(nets)
    };
    RbacPolicy {
        permissions: vec![RbacPermission::Any],
        principals: vec![RbacPrincipal::And(vec![net, authn_principal(a.authentication.as_ref())])],
    }
}

/// 認可の一覧（いずれかに一致すれば許可。空なら全拒否）。
fn rbac(authzs: &[&p::Authz]) -> Arc<RbacCfg> {
    Arc::new(RbacCfg {
        action: RbacAction::Allow,
        policies: authzs
            .iter()
            .enumerate()
            .map(|(i, a)| {
                let name = a
                    .metadata
                    .as_ref()
                    .map(|m| m.name())
                    .unwrap_or_else(|| format!("authz-{}", i));
                (name, authz_policy(a))
            })
            .collect(),
        shadow: false,
    })
}

/// inbound のアプリへのクラスタ（元の宛先へ）。
fn app_slot(cfg: &Config) -> Arc<ClusterSlot> {
    static APP: Lazy<Arc<ClusterSlot>> = Lazy::new(|| Arc::new(ClusterSlot::new("inbound")));
    if APP.cfg.load().is_none() {
        let mut c = backend_cfg(cfg, "inbound", None);
        c.kind = ClusterKind::OriginalDst;
        c.connect_timeout = cfg.inbound_connect_timeout.max(Duration::from_millis(100));
        APP.cfg.store(Arc::new(Some(Arc::new(c))));
    }
    APP.clone()
}

fn app_action(cfg: &Config) -> RouteAction {
    RouteAction::Forward(Arc::new(ForwardAction {
        target: ClusterTarget::Single(app_slot(cfg)),
        timeout: None,
        idle_timeout: None,
        retry: None,
        prefix_rewrite: None,
        regex_rewrite: None,
        host_rewrite: None,
        auto_host_rewrite: false,
        mirrors: Vec::new(),
        hash: Vec::new(),
        websocket: Some(true),
        connect_terminate: false,
    }))
}

fn inbound_http_routes(cfg: &Config, routes: &[p::InboundHttpRoute], server: &[p::Authz]) -> Result<Vec<MeshRoute>, String> {
    let mut out = Vec::new();
    if routes.is_empty() {
        // 古い制御プレーン: 全部に一致する既定のルート（Server の認可）
        let mut r = base_route("default", app_action(cfg));
        r.rbac = Some(rbac(&server.iter().collect::<Vec<_>>()));
        out.push(r);
        return Ok(out);
    }
    for route in routes {
        let name = route.metadata.as_ref().map(|m| m.name()).unwrap_or_default();
        let authzs: Vec<&p::Authz> = route.authorizations.iter().chain(server.iter()).collect();
        let rb = rbac(&authzs);
        for rule in &route.rules {
            let matches: Vec<Option<&p::HttpRouteMatch>> = if rule.matches.is_empty() {
                vec![None]
            } else {
                rule.matches.iter().map(Some).collect()
            };
            for m in matches {
                let (path, headers, query) = route_match(m)?;
                let mut mr = base_route(&name, app_action(cfg));
                mr.path = path;
                mr.headers = headers;
                mr.query = query;
                mr.rbac = Some(rb.clone());
                apply_http_filters(&mut mr, &rule.filters);
                out.push(mr);
            }
        }
    }
    Ok(out)
}

fn inbound_grpc_routes(cfg: &Config, routes: &[p::InboundGrpcRoute], server: &[p::Authz]) -> Result<Vec<MeshRoute>, String> {
    let mut out = Vec::new();
    for route in routes {
        let name = route.metadata.as_ref().map(|m| m.name()).unwrap_or_default();
        let authzs: Vec<&p::Authz> = route.authorizations.iter().chain(server.iter()).collect();
        let rb = rbac(&authzs);
        for rule in &route.rules {
            let matches: Vec<Option<&p::GrpcRouteMatch>> = if rule.matches.is_empty() {
                vec![None]
            } else {
                rule.matches.iter().map(Some).collect()
            };
            for m in matches {
                let mut mr = base_route(&name, app_action(cfg));
                if let Some(m) = m {
                    grpc_match(&mut mr, m)?;
                }
                mr.rbac = Some(rb.clone());
                out.push(mr);
            }
        }
    }
    if out.is_empty() {
        let mut r = base_route("default", app_action(cfg));
        r.rbac = Some(rbac(&server.iter().collect::<Vec<_>>()));
        out.push(r);
    }
    Ok(out)
}

fn translate_inbound(cfg: &Config, port: u16, srv: &p::Server) -> Result<InCfg, String> {
    use p::inbound_protocol::Kind as K;
    let (proto, routes) = match srv.protocol.as_ref().and_then(|p| p.kind.as_ref()) {
        Some(K::Detect(d)) => (
            InProto::Detect,
            inbound_http_routes(cfg, &d.http_routes, &srv.authorizations)?,
        ),
        Some(K::Http1(h)) => (InProto::Http1, inbound_http_routes(cfg, &h.routes, &srv.authorizations)?),
        Some(K::Http2(h)) => (InProto::Http2, inbound_http_routes(cfg, &h.routes, &srv.authorizations)?),
        Some(K::Grpc(g)) => (InProto::Http2, inbound_grpc_routes(cfg, &g.routes, &srv.authorizations)?),
        Some(K::Opaque(_)) => (InProto::Opaque, Vec::new()),
        Some(K::Tls(_)) => (InProto::Tls, Vec::new()),
        None => (InProto::Detect, inbound_http_routes(cfg, &[], &srv.authorizations)?),
    };
    Ok(InCfg {
        proto,
        hcm: Arc::new(http_cm(
            &format!("inbound:{}", port),
            route_table(&format!("inbound:{}", port), routes),
            Direction::Inbound,
            Vec::new(),
        )),
        tcp_rbac: rbac(&srv.authorizations.iter().collect::<Vec<_>>()),
    })
}

/// ポリシーが届かないときの既定（`LINKERD2_PROXY_INBOUND_DEFAULT_POLICY`）。
fn default_inbound(cfg: &Config, port: u16) -> InCfg {
    let any_net = |permit: Option<p::authn::Permit>| p::Authz {
        networks: vec![p::Network {
            net: Some(p::IpNetwork {
                ip: Some(p::IpAddress::from_ip([0, 0, 0, 0].into())),
                prefix_len: 0,
            }),
            except: Vec::new(),
        }],
        authentication: Some(p::Authn { permit }),
        labels: HashMap::new(),
        metadata: None,
    };
    let authzs = match cfg.default_policy.as_str() {
        "deny" => Vec::new(),
        "all-authenticated" | "cluster-authenticated" => vec![any_net(Some(
            p::authn::Permit::MeshTls(p::PermitMeshTls {
                clients: Some(p::permit_mesh_tls::Clients::Identities(p::PermitClientIdentities {
                    identities: Vec::new(),
                    suffixes: vec![p::IdentitySuffix { parts: Vec::new() }],
                })),
            }),
        ))],
        _ => vec![any_net(Some(p::authn::Permit::Unauthenticated(p::Empty {})))],
    };
    let proto = if cfg.opaque_ports.contains(&port) {
        p::InboundProtocol {
            kind: Some(p::inbound_protocol::Kind::Opaque(p::Empty {})),
        }
    } else {
        p::InboundProtocol {
            kind: Some(p::inbound_protocol::Kind::Detect(p::InboundDetect::default())),
        }
    };
    let srv = p::Server {
        protocol: Some(proto),
        server_ips: Vec::new(),
        authorizations: authzs,
        labels: HashMap::new(),
    };
    translate_inbound(cfg, port, &srv).unwrap_or_else(|_| InCfg {
        proto: InProto::Opaque,
        hcm: Arc::new(http_cm("inbound", RouteTable::empty("inbound"), Direction::Inbound, Vec::new())),
        tcp_rbac: rbac(&[]),
    })
}

/// inbound の L4 のクラスタ（元の宛先へ）。
pub fn app_tcp(cfg: &Config) -> Arc<TcpProxyCfg> {
    static TCP: Lazy<Mutex<Option<Arc<TcpProxyCfg>>>> = Lazy::new(|| Mutex::new(None));
    let mut g = TCP.lock().unwrap_or_else(|e| e.into_inner());
    g.get_or_insert_with(|| {
        let mut t = tcp_cfg(vec![(app_slot(cfg), 1)]);
        t.stat_prefix = "inbound".into();
        Arc::new(t)
    })
    .clone()
}
