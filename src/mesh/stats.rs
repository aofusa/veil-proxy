//! メッシュの観測（F-221）: Istio の標準メトリクスと、pilot-agent が見る Envoy の統計。
//!
//! - 要求ごとはラベルの組（ポインタと小さな整数の組）をキーにした表への加算だけ。表はワーカーごとに 1 つ
//!   （`Mutex` は持ち主のワーカーとスクレイプのときしか取らないので競合しない）。ラベルの文字列は組が
//!   初めて現れたときに 1 回だけ作る。
//! - スクレイプ（`/stats/prometheus`）で全ワーカーの表を合わせて Prometheus のテキストにする。

use super::httpcore::ResponseFlag;
use super::peer::Workload;
use bytes::Bytes;
use once_cell::sync::Lazy;
use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Reporter {
    Source,
    Destination,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Proto {
    Http,
    Grpc,
    Tcp,
}

/// 宛先のサービス（クラスタの `istio.services`）。
#[derive(Debug, Default, PartialEq, Eq, Hash)]
pub struct ServiceLabels {
    pub host: String,
    pub name: String,
    pub namespace: String,
}

/// 要求 1 件の記録に使うラベルの組。
#[derive(Clone)]
pub struct ReqLabels {
    pub reporter: Reporter,
    /// 相手のワークロード（outbound は宛先、inbound は送信元）
    pub peer: Option<Arc<Workload>>,
    /// 相手の SPIFFE ID（mTLS のとき）
    pub peer_principal: Option<Arc<str>>,
    pub service: Option<Arc<ServiceLabels>>,
    pub proto: Proto,
    pub mtls: bool,
}

#[derive(PartialEq, Eq, Hash, Clone, Copy)]
struct Key {
    reporter: u8,
    peer: usize,
    principal: usize,
    service: usize,
    proto: u8,
    code: u16,
    grpc: u16,
    flag: ResponseFlag,
    mtls: bool,
}

/// Istio の既定のバケット（ms）。
const DURATION_BUCKETS: [f64; 19] = [
    0.5, 1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0, 1000.0, 2500.0, 5000.0, 10000.0, 30000.0,
    60000.0, 300000.0, 600000.0, 1800000.0, 3600000.0,
];
const BYTES_BUCKETS: [f64; 9] = [1.0, 10.0, 100.0, 1000.0, 10000.0, 100000.0, 1e6, 1e7, 1e8];

#[derive(Clone)]
struct Hist<const N: usize> {
    buckets: [u64; N],
    sum: f64,
    count: u64,
}

impl<const N: usize> Hist<N> {
    fn new() -> Self {
        Hist {
            buckets: [0; N],
            sum: 0.0,
            count: 0,
        }
    }
    fn observe(&mut self, v: f64, bounds: &[f64; N]) {
        for (i, b) in bounds.iter().enumerate() {
            if v <= *b {
                self.buckets[i] += 1;
            }
        }
        self.sum += v;
        self.count += 1;
    }
    fn merge(&mut self, o: &Self) {
        for i in 0..N {
            self.buckets[i] += o.buckets[i];
        }
        self.sum += o.sum;
        self.count += o.count;
    }
}

#[derive(Clone)]
struct Entry {
    labels: Arc<str>,
    requests: u64,
    duration: Hist<19>,
    req_bytes: Hist<9>,
    resp_bytes: Hist<9>,
    // TCP
    tcp_sent: u64,
    tcp_received: u64,
    tcp_opened: u64,
    tcp_closed: u64,
    /// 生かしておく（キーのポインタが他の値に再利用されないように）
    _hold: (
        Option<Arc<Workload>>,
        Option<Arc<str>>,
        Option<Arc<ServiceLabels>>,
    ),
}

type Table = Arc<Mutex<HashMap<Key, Entry>>>;

static TABLES: Lazy<Mutex<Vec<Table>>> = Lazy::new(|| Mutex::new(Vec::new()));

thread_local! {
    static LOCAL: Table = {
        let t: Table = Arc::new(Mutex::new(HashMap::new()));
        if let Ok(mut v) = TABLES.lock() {
            v.push(t.clone());
        }
        t
    };
}

/// ラベルの組を表すキー。
fn key(l: &ReqLabels, code: u16, grpc: Option<u32>, flag: ResponseFlag) -> Key {
    Key {
        reporter: l.reporter as u8,
        peer: l.peer.as_ref().map_or(0, |p| Arc::as_ptr(p) as usize),
        principal: l
            .peer_principal
            .as_ref()
            .map_or(0, |p| Arc::as_ptr(p) as *const u8 as usize),
        service: l.service.as_ref().map_or(0, |s| Arc::as_ptr(s) as usize),
        proto: l.proto as u8,
        code,
        grpc: grpc.map_or(u16::MAX, |g| g as u16),
        flag,
        mtls: l.mtls,
    }
}

fn esc(out: &mut String, v: &str) {
    for c in v.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            c => out.push(c),
        }
    }
}

fn or_unknown(s: &str) -> &str {
    if s.is_empty() {
        "unknown"
    } else {
        s
    }
}

/// ラベルの文字列（`{` と `}` を除く）を作る（組が初めて現れたときだけ）。
fn render_labels(l: &ReqLabels, code: u16, grpc: Option<u32>, flag: ResponseFlag) -> String {
    let local = super::peer::local()
        .map(|n| n.workload.clone())
        .unwrap_or_default();
    let unknown = Workload::default();
    let peer = l.peer.as_deref().unwrap_or(&unknown);
    let (src, dst) = match l.reporter {
        Reporter::Source => (&*local, peer),
        Reporter::Destination => (peer, &*local),
    };
    let local_principal = super::identity::cert("default")
        .map(|c| c.uri.clone())
        .unwrap_or_default();
    let peer_principal = l.peer_principal.as_deref().unwrap_or("");
    let (src_p, dst_p) = match l.reporter {
        Reporter::Source => (local_principal.as_str(), peer_principal),
        Reporter::Destination => (peer_principal, local_principal.as_str()),
    };
    let svc = l.service.as_deref();
    let mut o = String::with_capacity(768);
    let mut kv = |k: &str, v: &str| {
        if !o.is_empty() {
            o.push(',');
        }
        o.push_str(k);
        o.push_str("=\"");
        esc(&mut o, v);
        o.push('"');
    };
    kv(
        "reporter",
        match l.reporter {
            Reporter::Source => "source",
            Reporter::Destination => "destination",
        },
    );
    kv("source_workload", or_unknown(&src.name));
    kv(
        "source_canonical_service",
        or_unknown(&src.canonical_service),
    );
    kv(
        "source_canonical_revision",
        or_unknown(&src.canonical_revision),
    );
    kv("source_workload_namespace", or_unknown(&src.namespace));
    kv("source_principal", or_unknown(src_p));
    kv("source_app", or_unknown(&src.app));
    kv("source_version", or_unknown(&src.version));
    kv("source_cluster", or_unknown(&src.cluster));
    kv("destination_workload", or_unknown(&dst.name));
    kv("destination_workload_namespace", or_unknown(&dst.namespace));
    kv("destination_principal", or_unknown(dst_p));
    kv("destination_app", or_unknown(&dst.app));
    kv("destination_version", or_unknown(&dst.version));
    kv(
        "destination_service",
        or_unknown(svc.map(|s| s.host.as_str()).unwrap_or("")),
    );
    kv(
        "destination_canonical_service",
        or_unknown(&dst.canonical_service),
    );
    kv(
        "destination_canonical_revision",
        or_unknown(&dst.canonical_revision),
    );
    kv(
        "destination_service_name",
        or_unknown(svc.map(|s| s.name.as_str()).unwrap_or("")),
    );
    kv(
        "destination_service_namespace",
        or_unknown(svc.map(|s| s.namespace.as_str()).unwrap_or("")),
    );
    kv("destination_cluster", or_unknown(&dst.cluster));
    kv(
        "request_protocol",
        match l.proto {
            Proto::Http => "http",
            Proto::Grpc => "grpc",
            Proto::Tcp => "tcp",
        },
    );
    let mut codebuf = itoa::Buffer::new();
    kv(
        "response_code",
        if l.proto == Proto::Tcp {
            "0"
        } else {
            codebuf.format(code)
        },
    );
    let mut gbuf = itoa::Buffer::new();
    kv(
        "grpc_response_status",
        match grpc {
            Some(g) => gbuf.format(g),
            None => "",
        },
    );
    kv("response_flags", flag.as_str());
    kv(
        "connection_security_policy",
        match (l.reporter, l.mtls) {
            (Reporter::Destination, true) => "mutual_tls",
            (Reporter::Destination, false) => "none",
            (Reporter::Source, _) => "unknown",
        },
    );
    o
}

fn with_entry(
    l: &ReqLabels,
    code: u16,
    grpc: Option<u32>,
    flag: ResponseFlag,
    f: impl FnOnce(&mut Entry),
) {
    let k = key(l, code, grpc, flag);
    LOCAL.with(|t| {
        let Ok(mut t) = t.lock() else { return };
        let e = t.entry(k).or_insert_with(|| Entry {
            labels: Arc::from(render_labels(l, code, grpc, flag)),
            requests: 0,
            duration: Hist::new(),
            req_bytes: Hist::new(),
            resp_bytes: Hist::new(),
            tcp_sent: 0,
            tcp_received: 0,
            tcp_opened: 0,
            tcp_closed: 0,
            _hold: (l.peer.clone(), l.peer_principal.clone(), l.service.clone()),
        });
        f(e);
    });
}

/// HTTP の要求 1 件を記録する。
pub fn record_request(
    l: &ReqLabels,
    code: u16,
    grpc: Option<u32>,
    flag: ResponseFlag,
    duration_ms: f64,
    req_bytes: u64,
    resp_bytes: u64,
) {
    with_entry(l, code, grpc, flag, |e| {
        e.requests += 1;
        e.duration.observe(duration_ms, &DURATION_BUCKETS);
        e.req_bytes.observe(req_bytes as f64, &BYTES_BUCKETS);
        e.resp_bytes.observe(resp_bytes as f64, &BYTES_BUCKETS);
    });
}

/// TCP の接続を記録する（開始は `opened`、終了時にバイト数と `closed`）。
pub fn record_tcp(
    l: &ReqLabels,
    opened: bool,
    closed: bool,
    sent: u64,
    received: u64,
    flag: ResponseFlag,
) {
    with_entry(l, 0, None, flag, |e| {
        if opened {
            e.tcp_opened += 1;
        }
        if closed {
            e.tcp_closed += 1;
        }
        e.tcp_sent += sent;
        e.tcp_received += received;
    });
}

// ---------------------------------------------------------------------------
// Envoy の統計（pilot-agent・`/stats`）
// ---------------------------------------------------------------------------

pub static DOWNSTREAM_CX_ACTIVE: AtomicI64 = AtomicI64::new(0);
pub static DOWNSTREAM_CX_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static DOWNSTREAM_RQ_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static UPSTREAM_CX_TOTAL: AtomicU64 = AtomicU64::new(0);
pub static UPSTREAM_CX_CONNECT_FAIL: AtomicU64 = AtomicU64::new(0);
pub static WORKERS_STARTED: AtomicU64 = AtomicU64::new(0);

/// 下流の接続数（Drop で減らす）。
pub struct CxGuard;

impl CxGuard {
    pub fn new() -> Self {
        DOWNSTREAM_CX_ACTIVE.fetch_add(1, Ordering::Relaxed);
        DOWNSTREAM_CX_TOTAL.fetch_add(1, Ordering::Relaxed);
        CxGuard
    }
}

impl Default for CxGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CxGuard {
    fn drop(&mut self) {
        DOWNSTREAM_CX_ACTIVE.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Envoy の `/stats` 形式（`name: value`）。pilot-agent の準備確認が読む項目を含む。
pub fn envoy_stats_text(filter: Option<&str>) -> String {
    let st = super::state::state();
    let workers = WORKERS_STARTED.load(Ordering::Relaxed) > 0;
    let items: Vec<(String, i64)> = vec![
        (
            "cluster_manager.cds.update_success".into(),
            st.cds_updates.load(Ordering::Relaxed) as i64,
        ),
        (
            "cluster_manager.cds.update_rejected".into(),
            st.cds_rejected.load(Ordering::Relaxed) as i64,
        ),
        (
            "listener_manager.lds.update_success".into(),
            st.lds_updates.load(Ordering::Relaxed) as i64,
        ),
        (
            "listener_manager.lds.update_rejected".into(),
            st.lds_rejected.load(Ordering::Relaxed) as i64,
        ),
        ("listener_manager.workers_started".into(), workers as i64),
        (
            "server.state".into(),
            // 0 = LIVE, 1 = DRAINING, 2 = PRE_INITIALIZING, 3 = INITIALIZING
            if st.draining.load(Ordering::Relaxed) {
                1
            } else if st.ready() && workers {
                0
            } else {
                3
            },
        ),
        ("server.live".into(), (st.ready() && workers) as i64),
        (
            "http.inbound.downstream_cx_active".into(),
            DOWNSTREAM_CX_ACTIVE.load(Ordering::Relaxed),
        ),
        (
            "listener.0.0.0.0_15006.downstream_cx_active".into(),
            DOWNSTREAM_CX_ACTIVE.load(Ordering::Relaxed),
        ),
        (
            "server.total_connections".into(),
            DOWNSTREAM_CX_ACTIVE.load(Ordering::Relaxed),
        ),
        (
            "http.downstream_rq_total".into(),
            DOWNSTREAM_RQ_TOTAL.load(Ordering::Relaxed) as i64,
        ),
        (
            "cluster_manager.upstream_cx_total".into(),
            UPSTREAM_CX_TOTAL.load(Ordering::Relaxed) as i64,
        ),
    ];
    let re = filter.and_then(|f| regex::Regex::new(f).ok());
    let mut out = String::new();
    for (k, v) in items {
        if let Some(re) = &re {
            if !re.is_match(&k) {
                continue;
            }
        }
        let _ = writeln!(out, "{}: {}", k, v);
    }
    out
}

/// Prometheus のテキスト（15090 の `/stats/prometheus`）。
pub fn prometheus_text() -> String {
    // 全ワーカーの表をラベル文字列で合わせる
    let mut merged: BTreeMap<Arc<str>, Entry> = BTreeMap::new();
    if let Ok(tables) = TABLES.lock() {
        for t in tables.iter() {
            let Ok(t) = t.lock() else { continue };
            for e in t.values() {
                match merged.get_mut(&e.labels) {
                    Some(m) => {
                        m.requests += e.requests;
                        m.duration.merge(&e.duration);
                        m.req_bytes.merge(&e.req_bytes);
                        m.resp_bytes.merge(&e.resp_bytes);
                        m.tcp_sent += e.tcp_sent;
                        m.tcp_received += e.tcp_received;
                        m.tcp_opened += e.tcp_opened;
                        m.tcp_closed += e.tcp_closed;
                    }
                    None => {
                        merged.insert(e.labels.clone(), e.clone());
                    }
                }
            }
        }
    }
    let mut out = String::with_capacity(4096 + merged.len() * 2048);
    let http: Vec<&Entry> = merged.values().filter(|e| e.requests > 0).collect();
    let tcp: Vec<&Entry> = merged
        .values()
        .filter(|e| e.tcp_opened + e.tcp_closed + e.tcp_sent + e.tcp_received > 0)
        .collect();

    out.push_str("# TYPE istio_requests_total counter\n");
    for e in &http {
        let _ = writeln!(out, "istio_requests_total{{{}}} {}", e.labels, e.requests);
    }
    hist_text(
        &mut out,
        "istio_request_duration_milliseconds",
        &http,
        |e| &e.duration,
        &DURATION_BUCKETS,
    );
    hist_text(
        &mut out,
        "istio_request_bytes",
        &http,
        |e| &e.req_bytes,
        &BYTES_BUCKETS,
    );
    hist_text(
        &mut out,
        "istio_response_bytes",
        &http,
        |e| &e.resp_bytes,
        &BYTES_BUCKETS,
    );
    for (name, f) in [
        (
            "istio_tcp_sent_bytes_total",
            (|e: &Entry| e.tcp_sent) as fn(&Entry) -> u64,
        ),
        ("istio_tcp_received_bytes_total", |e: &Entry| e.tcp_received),
        ("istio_tcp_connections_opened_total", |e: &Entry| {
            e.tcp_opened
        }),
        ("istio_tcp_connections_closed_total", |e: &Entry| {
            e.tcp_closed
        }),
    ] {
        let _ = writeln!(out, "# TYPE {} counter", name);
        for e in &tcp {
            let _ = writeln!(out, "{}{{{}}} {}", name, e.labels, f(e));
        }
    }
    // Envoy 互換の主要な値（ダッシュボードの補助）
    let _ = writeln!(out, "# TYPE envoy_server_live gauge");
    let st = super::state::state();
    let _ = writeln!(out, "envoy_server_live {}", st.ready() as u8);
    let _ = writeln!(out, "# TYPE envoy_http_downstream_cx_active gauge");
    let _ = writeln!(
        out,
        "envoy_http_downstream_cx_active {}",
        DOWNSTREAM_CX_ACTIVE.load(Ordering::Relaxed)
    );
    let _ = writeln!(out, "# TYPE envoy_http_downstream_rq_total counter");
    let _ = writeln!(
        out,
        "envoy_http_downstream_rq_total {}",
        DOWNSTREAM_RQ_TOTAL.load(Ordering::Relaxed)
    );
    out
}

fn hist_text<const N: usize>(
    out: &mut String,
    name: &str,
    entries: &[&Entry],
    get: impl Fn(&Entry) -> &Hist<N>,
    bounds: &[f64; N],
) {
    let _ = writeln!(out, "# TYPE {} histogram", name);
    for e in entries {
        let h = get(e);
        for (i, b) in bounds.iter().enumerate() {
            let _ = writeln!(
                out,
                "{}_bucket{{{},le=\"{}\"}} {}",
                name, e.labels, b, h.buckets[i]
            );
        }
        let _ = writeln!(
            out,
            "{}_bucket{{{},le=\"+Inf\"}} {}",
            name, e.labels, h.count
        );
        let _ = writeln!(out, "{}_sum{{{}}} {}", name, e.labels, h.sum);
        let _ = writeln!(out, "{}_count{{{}}} {}", name, e.labels, h.count);
    }
}

/// 宛先サービスのラベルを作る（クラスタの変換時）。
pub fn service_labels(host: &Bytes, name: &Bytes, namespace: &Bytes) -> Option<Arc<ServiceLabels>> {
    if host.is_empty() {
        return None;
    }
    Some(Arc::new(ServiceLabels {
        host: String::from_utf8_lossy(host).into_owned(),
        name: String::from_utf8_lossy(name).into_owned(),
        namespace: String::from_utf8_lossy(namespace).into_owned(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_and_renders() {
        let l = ReqLabels {
            reporter: Reporter::Destination,
            peer: Some(Arc::new(Workload {
                name: "client".into(),
                namespace: "demo".into(),
                ..Default::default()
            })),
            peer_principal: Some(Arc::from("spiffe://cluster.local/ns/demo/sa/client")),
            service: Some(Arc::new(ServiceLabels {
                host: "whoami.demo.svc.cluster.local".into(),
                name: "whoami".into(),
                namespace: "demo".into(),
            })),
            proto: Proto::Http,
            mtls: true,
        };
        record_request(&l, 200, None, ResponseFlag::None, 3.0, 10, 100);
        record_request(&l, 200, None, ResponseFlag::None, 7.0, 10, 100);
        let t = prometheus_text();
        assert!(
            t.contains("istio_requests_total{reporter=\"destination\",source_workload=\"client\""),
            "{t}"
        );
        assert!(
            t.contains("connection_security_policy=\"mutual_tls\"} 2"),
            "{t}"
        );
        assert!(t.contains("istio_request_duration_milliseconds_bucket{"));
    }
}
