//! Linkerd のプロキシのメトリクス（`/metrics`。F-223・F-221）。
//!
//! 本家の linkerd2-proxy の名前（`request_total`・`response_total`・`response_latency_ms`・`tcp_*`）と
//! ラベル（`direction`・`authority`・`target_addr`・`tls`・`dst_*`・`classification`・`status_code`）を
//! 出す。viz（Prometheus）は Pod のラベル（`namespace`・`pod`・`deployment` 等）をスクレイプ側で足す。
//! 値はワーカーごとの表に足し、`/metrics` のときに合算する（要求ごとにロックしない）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// 1 本の要求の記録。
pub struct Req<'a> {
    pub inbound: bool,
    pub authority: &'a str,
    pub target_addr: std::net::SocketAddr,
    pub tls: bool,
    pub client_id: Option<&'a str>,
    pub server_id: Option<&'a str>,
    /// 宛先のラベル（`dst_*="..."` を連結したもの。Destination の metric_labels）
    pub dst_labels: Option<&'a str>,
    pub status: u16,
    pub grpc: Option<u32>,
    pub latency_ms: f64,
}

const BUCKETS: [f64; 26] = [
    1.0, 2.0, 3.0, 4.0, 5.0, 10.0, 20.0, 30.0, 40.0, 50.0, 100.0, 200.0, 300.0, 400.0, 500.0,
    1000.0, 2000.0, 3000.0, 4000.0, 5000.0, 10000.0, 20000.0, 30000.0, 40000.0, 50000.0,
    f64::INFINITY,
];

#[derive(Default, Clone)]
struct Series {
    requests: u64,
    responses: HashMap<(u16, bool, Option<u32>), u64>,
    buckets: Vec<u64>,
    sum: f64,
    count: u64,
}

#[derive(Default)]
struct Table {
    series: HashMap<String, Series>,
}

/// ワーカーの表（スレッドの終了まで生きる。`/metrics` は全ワーカーの表を合算する）
static TABLES: Mutex<Vec<Arc<Mutex<Table>>>> = Mutex::new(Vec::new());

thread_local! {
    static LOCAL: RefCell<Option<Arc<Mutex<Table>>>> = const { RefCell::new(None) };
}

fn labels(r: &Req<'_>) -> String {
    let mut s = String::with_capacity(160);
    s.push_str(if r.inbound {
        "direction=\"inbound\""
    } else {
        "direction=\"outbound\""
    });
    s.push_str(",authority=\"");
    s.push_str(&r.authority.replace('"', ""));
    s.push_str("\",target_addr=\"");
    s.push_str(&r.target_addr.to_string());
    s.push('"');
    if r.tls {
        s.push_str(",tls=\"true\"");
        if let Some(c) = r.client_id {
            s.push_str(",client_id=\"");
            s.push_str(c);
            s.push('"');
        }
        if let Some(sv) = r.server_id {
            s.push_str(",server_id=\"");
            s.push_str(sv);
            s.push('"');
        }
    } else {
        s.push_str(",tls=\"no_identity\",no_tls_reason=\"no_tls_from_remote\"");
    }
    if let Some(d) = r.dst_labels {
        s.push(',');
        s.push_str(d);
    }
    s
}

/// HTTP の要求 1 本（メッシュの HTTP の処理から渡す）。
pub struct HttpDone<'a> {
    pub view: &'a crate::mesh::l7::ReqView<'a>,
    pub upstream: Option<std::net::SocketAddr>,
    pub status: u16,
    pub grpc: Option<u32>,
    pub latency: std::time::Duration,
    pub resp_bytes: u64,
    pub ep_labels: Option<&'a str>,
    pub server_id: Option<&'a str>,
}

/// HTTP の要求 1 本を記録する（Linkerd の接続だけ。tap の購読があればイベントも積む）。
pub fn record_http(ctx: &crate::mesh::l7::ConnCtx, d: &HttpDone<'_>) {
    let inbound = ctx.direction == crate::mesh::model::Direction::Inbound;
    let Some(target) = (if inbound { ctx.orig_dst } else { d.upstream.or(ctx.orig_dst) }) else {
        return;
    };
    let (status, grpc, ep_labels, server_id) = (d.status, d.grpc, d.ep_labels, d.server_id);
    let latency_ms = d.latency.as_secs_f64() * 1000.0;
    let authority = std::str::from_utf8(d.view.authority).unwrap_or("");
    if super::tap::active() {
        super::tap::emit(&super::tap::TapReq {
            inbound,
            source: ctx.peer,
            destination: target,
            dst_labels: if inbound { None } else { ep_labels },
            client_id: if inbound { ctx.peer_dns.first().map(|s| s.as_str()) } else { None },
            method: d.view.method,
            scheme: d.view.scheme,
            authority: d.view.authority,
            path: d.view.path,
            status,
            grpc,
            latency: d.latency,
            resp_bytes: d.resp_bytes,
        });
    }
    record(&Req {
        inbound,
        authority,
        target_addr: target,
        tls: if inbound { ctx.tls } else { server_id.is_some() },
        client_id: if inbound { ctx.peer_dns.first().map(|s| s.as_str()) } else { None },
        server_id: if inbound { None } else { server_id },
        dst_labels: if inbound { None } else { ep_labels },
        status,
        grpc,
        latency_ms,
    });
}

pub fn record(r: &Req<'_>) {
    let key = labels(r);
    let failure = r.status >= 500 || r.grpc.is_some_and(|g| g != 0);
    LOCAL.with(|l| {
        let mut l = l.borrow_mut();
        let t = l.get_or_insert_with(|| {
            let t = Arc::new(Mutex::new(Table::default()));
            TABLES.lock().unwrap_or_else(|e| e.into_inner()).push(t.clone());
            t
        });
        let mut t = t.lock().unwrap_or_else(|e| e.into_inner());
        let s = t.series.entry(key).or_insert_with(|| Series {
            buckets: vec![0; BUCKETS.len()],
            ..Default::default()
        });
        s.requests += 1;
        *s.responses.entry((r.status, failure, r.grpc)).or_insert(0) += 1;
        if let Some(i) = BUCKETS.iter().position(|b| r.latency_ms <= *b) {
            s.buckets[i] += 1;
        }
        s.sum += r.latency_ms;
        s.count += 1;
    });
}

// ---------------------------------------------------------------------------
// TCP（接続の数。viz の edges・stat の TCP_CONN）
// ---------------------------------------------------------------------------

#[derive(Default, Clone, Copy)]
struct TcpStats {
    open_total: u64,
    close_total: u64,
    open: i64,
}

static TCP: Mutex<Option<HashMap<Arc<str>, TcpStats>>> = Mutex::new(None);

/// 接続 1 本（作ってから捨てるまで `tcp_open_connections` に数える）。
pub struct TcpGuard {
    key: Arc<str>,
}

impl TcpGuard {
    fn new(key: String) -> Self {
        let key: Arc<str> = Arc::from(key);
        let mut g = TCP.lock().unwrap_or_else(|e| e.into_inner());
        let e = g.get_or_insert_with(HashMap::new).entry(key.clone()).or_default();
        e.open_total += 1;
        e.open += 1;
        TcpGuard { key }
    }

    /// inbound の接続（相手の ID は mTLS のときだけ）。
    pub fn inbound(target: std::net::SocketAddr, client_id: Option<&str>) -> Self {
        let mut s = format!("direction=\"inbound\",peer=\"src\",target_addr=\"{}\"", target);
        match client_id {
            Some(c) => s.push_str(&format!(",tls=\"true\",client_id=\"{}\"", c)),
            None => s.push_str(",tls=\"no_identity\",no_tls_reason=\"no_tls_from_remote\""),
        }
        Self::new(s)
    }

    /// 相手のプロキシへの mTLS の接続（宛先のラベル付き）。
    pub fn outbound(target: std::net::SocketAddr, server_id: &str, dst_labels: Option<&str>) -> Self {
        let mut s = format!(
            "direction=\"outbound\",peer=\"dst\",target_addr=\"{}\",tls=\"true\",server_id=\"{}\"",
            target, server_id
        );
        if let Some(d) = dst_labels {
            s.push(',');
            s.push_str(d);
        }
        Self::new(s)
    }
}

impl Drop for TcpGuard {
    fn drop(&mut self) {
        let mut g = TCP.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(e) = g.get_or_insert_with(HashMap::new).get_mut(&self.key) {
            e.close_total += 1;
            e.open -= 1;
        }
    }
}

fn tcp_text(out: &mut String) {
    let g = TCP.lock().unwrap_or_else(|e| e.into_inner());
    let Some(m) = g.as_ref() else { return };
    out.push_str("# HELP tcp_open_total Total count of opened connections\n# TYPE tcp_open_total counter\n");
    for (k, s) in m {
        out.push_str(&format!("tcp_open_total{{{}}} {}\n", k, s.open_total));
    }
    out.push_str("# HELP tcp_close_total Total count of closed connections\n# TYPE tcp_close_total counter\n");
    for (k, s) in m {
        out.push_str(&format!("tcp_close_total{{{},errno=\"\"}} {}\n", k, s.close_total));
    }
    out.push_str("# HELP tcp_open_connections Number of currently-open connections\n# TYPE tcp_open_connections gauge\n");
    for (k, s) in m {
        out.push_str(&format!("tcp_open_connections{{{}}} {}\n", k, s.open.max(0)));
    }
}

pub fn text() -> String {
    let mut all: HashMap<String, Series> = HashMap::new();
    for t in TABLES.lock().unwrap_or_else(|e| e.into_inner()).iter() {
        let t = t.lock().unwrap_or_else(|e| e.into_inner());
        for (k, s) in &t.series {
            let e = all.entry(k.clone()).or_insert_with(|| Series {
                buckets: vec![0; BUCKETS.len()],
                ..Default::default()
            });
            e.requests += s.requests;
            for (rk, n) in &s.responses {
                *e.responses.entry(*rk).or_insert(0) += n;
            }
            for (i, b) in s.buckets.iter().enumerate() {
                e.buckets[i] += b;
            }
            e.sum += s.sum;
            e.count += s.count;
        }
    }
    let mut out = String::new();
    out.push_str("# HELP request_total Total count of HTTP requests.\n# TYPE request_total counter\n");
    for (k, s) in &all {
        out.push_str(&format!("request_total{{{}}} {}\n", k, s.requests));
    }
    out.push_str("# HELP response_total Total count of HTTP responses.\n# TYPE response_total counter\n");
    for (k, s) in &all {
        for ((status, failure, grpc), n) in &s.responses {
            let mut l = format!(
                "{},status_code=\"{}\",classification=\"{}\"",
                k,
                status,
                if *failure { "failure" } else { "success" }
            );
            if let Some(g) = grpc {
                l.push_str(&format!(",grpc_status=\"{}\"", g));
            }
            out.push_str(&format!("response_total{{{}}} {}\n", l, n));
        }
    }
    out.push_str("# HELP response_latency_ms Elapsed times between a request's headers being received and its response stream completing\n# TYPE response_latency_ms histogram\n");
    for (k, s) in &all {
        let mut cum = 0u64;
        for (i, b) in BUCKETS.iter().enumerate() {
            cum += s.buckets[i];
            let le = if b.is_infinite() {
                "+Inf".to_string()
            } else {
                format!("{}", b)
            };
            out.push_str(&format!("response_latency_ms_bucket{{{},le=\"{}\"}} {}\n", k, le, cum));
        }
        out.push_str(&format!("response_latency_ms_sum{{{}}} {}\n", k, s.sum));
        out.push_str(&format!("response_latency_ms_count{{{}}} {}\n", k, s.count));
    }
    tcp_text(&mut out);
    let (o, i, b) = super::discovery::counts();
    out.push_str(&format!(
        "# TYPE veil_linkerd_watches gauge\nveil_linkerd_watches{{kind=\"outbound\"}} {}\nveil_linkerd_watches{{kind=\"inbound\"}} {}\nveil_linkerd_watches{{kind=\"destination\"}} {}\n",
        o, i, b
    ));
    out.push_str(&format!(
        "# TYPE identity_cert_expiration_timestamp_seconds gauge\nidentity_cert_expiration_timestamp_seconds {}\n",
        super::identity::EXPIRY.load(std::sync::atomic::Ordering::Relaxed)
    ));
    out
}
