//! 分散トレース（F-221。Istio の Telemetry API の tracing = Envoy の `envoy.tracers.zipkin`）。
//!
//! - B3（`x-b3-traceid`・`x-b3-spanid`・`x-b3-parentspanid`・`x-b3-sampled`、単一ヘッダの `b3`）を読み、
//!   この hop のスパンを作って上流へ伝播する。サンプリングは来た判断に従い、無ければ HCM の
//!   `random_sampling`（Istio の randomSamplingPercentage）。
//! - サンプルしたスパンだけ Zipkin v2 の JSON にして専用スレッドへ渡し、1 秒ごとにまとめて
//!   コレクタ（`collector_service` のクラスタ）へ POST する。データプレーンは送信を待たない。
//! - サンプルしない要求ではスパンを作らない（ヘッダの伝播だけ）。

use super::model::ClusterSlot;
use super::xds::proto as p;
use bytes::Bytes;
use once_cell::sync::Lazy;
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::{Arc, Mutex};

pub const T_ZIPKIN: &str = "type.googleapis.com/envoy.config.trace.v3.ZipkinConfig";

#[derive(Clone, PartialEq, prost::Message)]
pub struct HcmTracingFull {
    #[prost(message, optional, tag = "4")]
    pub random_sampling: Option<p::Percent>,
    #[prost(message, repeated, tag = "8")]
    pub custom_tags: Vec<CustomTag>,
    #[prost(message, optional, tag = "9")]
    pub provider: Option<TracingHttp>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct TracingHttp {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(message, optional, tag = "3")]
    pub typed_config: Option<p::Any>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct CustomTag {
    #[prost(string, tag = "1")]
    pub tag: String,
    #[prost(message, optional, tag = "2")]
    pub literal: Option<Literal>,
    #[prost(message, optional, tag = "4")]
    pub request_header: Option<HeaderTag>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct Literal {
    #[prost(string, tag = "1")]
    pub value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HeaderTag {
    #[prost(string, tag = "1")]
    pub name: String,
    #[prost(string, tag = "2")]
    pub default_value: String,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct ZipkinConfig {
    #[prost(string, tag = "1")]
    pub collector_cluster: String,
    #[prost(string, tag = "2")]
    pub collector_endpoint: String,
    #[prost(bool, tag = "3")]
    pub trace_id_128bit: bool,
    #[prost(int32, tag = "5")]
    pub collector_endpoint_version: i32,
    #[prost(string, tag = "6")]
    pub collector_hostname: String,
    #[prost(message, optional, tag = "9")]
    pub collector_service: Option<HttpService>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpService {
    #[prost(message, optional, tag = "1")]
    pub http_uri: Option<HttpUri>,
}

#[derive(Clone, PartialEq, prost::Message)]
pub struct HttpUri {
    #[prost(string, tag = "1")]
    pub uri: String,
    #[prost(string, tag = "2")]
    pub cluster: String,
}

#[derive(Debug)]
pub struct TracerCfg {
    pub collector: Arc<ClusterSlot>,
    pub path: String,
    pub host: String,
    pub trace_id_128: bool,
    /// サンプリング（百万分率）
    pub sampling: u32,
    /// 固定値のタグ
    pub tags: Vec<(String, String)>,
    /// 要求ヘッダから取るタグ（タグ名, ヘッダ名, 既定値）
    pub header_tags: Vec<(String, String, String)>,
}

/// HCM の tracing を変換する（Zipkin のプロバイダが無ければ `None`）。
pub fn from_full(full: &HcmTracingFull, reg: &mut crate::mesh::state::Registry) -> Option<TracerCfg> {
    let prov = full.provider.as_ref()?;
    let any = prov.typed_config.as_ref()?;
    if any.type_url != T_ZIPKIN {
        ftlog::debug!("[mesh] unsupported tracer {}", any.type_url);
        return None;
    }
    let z: ZipkinConfig = p::decode_any(any, T_ZIPKIN)?;
    let (cluster, uri) = match &z.collector_service {
        Some(HttpService { http_uri: Some(u) }) => (u.cluster.clone(), u.uri.clone()),
        _ => (z.collector_cluster.clone(), z.collector_endpoint.clone()),
    };
    if cluster.is_empty() {
        return None;
    }
    // uri は "http://host/path" か "/path"
    let (host, path) = match uri.strip_prefix("http://").or_else(|| uri.strip_prefix("https://")) {
        Some(rest) => match rest.find('/') {
            Some(i) => (rest[..i].to_string(), rest[i..].to_string()),
            None => (rest.to_string(), "/api/v2/spans".to_string()),
        },
        None => (
            if z.collector_hostname.is_empty() { cluster.clone() } else { z.collector_hostname.clone() },
            if uri.is_empty() { "/api/v2/spans".to_string() } else { uri },
        ),
    };
    let mut tags = Vec::new();
    let mut header_tags = Vec::new();
    for t in &full.custom_tags {
        if let Some(l) = &t.literal {
            tags.push((t.tag.clone(), l.value.clone()));
        } else if let Some(h) = &t.request_header {
            header_tags.push((t.tag.clone(), h.name.to_ascii_lowercase(), h.default_value.clone()));
        }
    }
    Some(TracerCfg {
        collector: reg.cluster_slot(&cluster),
        path,
        host,
        trace_id_128: z.trace_id_128bit,
        sampling: full
            .random_sampling
            .as_ref()
            .map(|p| (p.value * 10_000.0) as u32)
            .unwrap_or(10_000),
        tags,
        header_tags,
    })
}

/// この hop のスパン。
#[derive(Debug, Clone)]
pub struct Span {
    pub trace_hi: u64,
    pub trace_lo: u64,
    pub id: u64,
    pub parent: Option<u64>,
    pub sampled: bool,
    pub start_us: u64,
}

fn parse_hex(s: &[u8]) -> Option<u64> {
    if s.is_empty() || s.len() > 16 {
        return None;
    }
    u64::from_str_radix(std::str::from_utf8(s).ok()?, 16).ok()
}

fn now_us() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

/// 要求のヘッダから文脈を読み、この hop のスパンを作る。
pub fn begin(cfg: &TracerCfg, header: &dyn Fn(&str) -> Option<Bytes>) -> Span {
    let mut trace_hi = 0u64;
    let mut trace_lo = 0u64;
    let mut parent = None;
    let mut sampled: Option<bool> = None;
    if let Some(tid) = header("x-b3-traceid") {
        if tid.len() == 32 {
            trace_hi = parse_hex(&tid[..16]).unwrap_or(0);
            trace_lo = parse_hex(&tid[16..]).unwrap_or(0);
        } else {
            trace_lo = parse_hex(&tid).unwrap_or(0);
        }
        parent = header("x-b3-spanid").and_then(|s| parse_hex(&s));
        sampled = header("x-b3-sampled").map(|s| s.as_ref() == b"1" || s.as_ref() == b"true");
    } else if let Some(b3) = header("b3") {
        // traceid-spanid-sampled-parent
        let parts: Vec<&[u8]> = b3.split(|&b| b == b'-').collect();
        if parts.len() >= 2 {
            let tid = parts[0];
            if tid.len() == 32 {
                trace_hi = parse_hex(&tid[..16]).unwrap_or(0);
                trace_lo = parse_hex(&tid[16..]).unwrap_or(0);
            } else {
                trace_lo = parse_hex(tid).unwrap_or(0);
            }
            parent = parse_hex(parts[1]);
            sampled = parts.get(2).map(|s| *s == b"1" || *s == b"d");
        } else if b3.as_ref() == b"0" {
            sampled = Some(false);
        }
    }
    if trace_lo == 0 && trace_hi == 0 {
        trace_lo = super::httpcore::rand_u64() | 1;
        if cfg.trace_id_128 {
            trace_hi = super::httpcore::rand_u64();
        }
        parent = None;
    }
    let sampled = sampled.unwrap_or_else(|| {
        cfg.sampling >= 1_000_000 || super::httpcore::rand_u32() % 1_000_000 < cfg.sampling
    });
    Span {
        trace_hi,
        trace_lo,
        id: super::httpcore::rand_u64() | 1,
        parent,
        sampled,
        start_us: now_us(),
    }
}

fn hex16(v: u64) -> String {
    format!("{:016x}", v)
}

impl Span {
    pub fn trace_id(&self) -> String {
        if self.trace_hi != 0 {
            format!("{:016x}{:016x}", self.trace_hi, self.trace_lo)
        } else {
            hex16(self.trace_lo)
        }
    }

    /// 上流へ伝播する B3 のヘッダ（このスパンを親にする）。
    pub fn upstream_headers(&self) -> [(Bytes, Bytes); 4] {
        [
            (Bytes::from_static(b"x-b3-traceid"), Bytes::from(self.trace_id())),
            (Bytes::from_static(b"x-b3-spanid"), Bytes::from(hex16(self.id))),
            (
                Bytes::from_static(b"x-b3-parentspanid"),
                Bytes::from(self.parent.map(hex16).unwrap_or_else(|| hex16(self.id))),
            ),
            (
                Bytes::from_static(b"x-b3-sampled"),
                Bytes::from_static(if self.sampled { b"1" } else { b"0" }),
            ),
        ]
    }
}

/// スパンを終える（サンプルしたものだけ送る）。
#[allow(clippy::too_many_arguments)]
pub fn finish(
    cfg: &Arc<TracerCfg>,
    span: &Span,
    name: &str,
    kind: &str,
    method: &[u8],
    url: &str,
    status: u16,
    upstream_cluster: &str,
    header: &dyn Fn(&str) -> Option<Bytes>,
) {
    if !span.sampled {
        return;
    }
    let local = super::peer::local();
    let service = local
        .map(|l| format!("{}.{}", l.workload.canonical_service, l.workload.namespace))
        .unwrap_or_else(|| "veil".into());
    let ip = local
        .and_then(|l| l.instance_ips.first().map(|i| i.to_string()))
        .unwrap_or_default();
    let mut tags = serde_json::Map::new();
    for (k, v) in &cfg.tags {
        tags.insert(k.clone(), serde_json::Value::String(v.clone()));
    }
    for (k, h, d) in &cfg.header_tags {
        let v = header(h)
            .map(|b| String::from_utf8_lossy(&b).into_owned())
            .unwrap_or_else(|| d.clone());
        tags.insert(k.clone(), serde_json::Value::String(v));
    }
    tags.insert("http.method".into(), String::from_utf8_lossy(method).into());
    tags.insert("http.url".into(), url.into());
    tags.insert("http.status_code".into(), status.to_string().into());
    tags.insert("upstream_cluster".into(), upstream_cluster.into());
    tags.insert("component".into(), "proxy".into());
    tags.insert(
        "node_id".into(),
        local.map(|l| l.id.clone()).unwrap_or_default().into(),
    );
    if status >= 500 || status == 0 {
        tags.insert("error".into(), "true".into());
    }
    let mut v = serde_json::json!({
        "traceId": span.trace_id(),
        "id": hex16(span.id),
        "name": name,
        "kind": kind,
        "timestamp": span.start_us,
        "duration": now_us().saturating_sub(span.start_us).max(1),
        "localEndpoint": {"serviceName": service, "ipv4": ip},
        "tags": tags,
    });
    if let Some(p) = span.parent {
        v["parentId"] = serde_json::Value::String(hex16(p));
    }
    if let Some(tx) = reporter(cfg) {
        let _ = tx.try_send(v.to_string());
    }
}

type Reporters = Vec<(usize, SyncSender<String>)>;
static REPORTERS: Lazy<Mutex<Reporters>> = Lazy::new(|| Mutex::new(Vec::new()));

fn reporter(cfg: &Arc<TracerCfg>) -> Option<SyncSender<String>> {
    // コレクタのスロット（クラスタ）ごとに 1 本の送信スレッド
    let key = Arc::as_ptr(&cfg.collector) as usize;
    let mut g = REPORTERS.lock().ok()?;
    if let Some((_, tx)) = g.iter().find(|(k, _)| *k == key) {
        return Some(tx.clone());
    }
    let (tx, rx) = sync_channel::<String>(4096);
    let slot = cfg.collector.clone();
    let host = cfg.host.clone();
    let path = cfg.path.clone();
    std::thread::Builder::new()
        .name("veil-tracing".into())
        .spawn(move || report_loop(rx, slot, host, path))
        .ok()?;
    g.push((key, tx.clone()));
    Some(tx)
}

// 理由付き allow: トレースの送信専用スレッド（データプレーン外）でブロッキングの HTTP を使う
#[allow(clippy::disallowed_methods)]
fn report_loop(rx: std::sync::mpsc::Receiver<String>, slot: Arc<ClusterSlot>, host: String, path: String) {
    use std::io::{Read, Write};
    loop {
        let first = match rx.recv() {
            Ok(s) => s,
            Err(_) => return,
        };
        let mut batch = vec![first];
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
        while batch.len() < 512 {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match rx.recv_timeout(left) {
                Ok(s) => batch.push(s),
                Err(_) => break,
            }
        }
        let body = format!("[{}]", batch.join(","));
        let eps = slot.endpoints.load();
        let Some(ep) = eps.healthy.first().and_then(|&i| eps.endpoints.get(i)) else {
            continue;
        };
        let Ok(mut s) = std::net::TcpStream::connect_timeout(&ep.addr, std::time::Duration::from_secs(2)) else {
            continue;
        };
        let _ = s.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let req = format!(
            "POST {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            path,
            host,
            body.len()
        );
        if s.write_all(req.as_bytes()).is_ok() && s.write_all(body.as_bytes()).is_ok() {
            let mut buf = [0u8; 512];
            let _ = s.read(&mut buf);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> TracerCfg {
        TracerCfg {
            collector: Arc::new(ClusterSlot::new("c")),
            path: "/api/v2/spans".into(),
            host: "c".into(),
            trace_id_128: true,
            sampling: 1_000_000,
            tags: vec![],
            header_tags: vec![],
        }
    }

    #[test]
    fn continues_incoming_b3_context() {
        let h = |n: &str| match n {
            "x-b3-traceid" => Some(Bytes::from_static(b"463ac35c9f6413ad48485a3953bb6124")),
            "x-b3-spanid" => Some(Bytes::from_static(b"a2fb4a1d1a96d312")),
            "x-b3-sampled" => Some(Bytes::from_static(b"1")),
            _ => None,
        };
        let s = begin(&cfg(), &h);
        assert_eq!(s.trace_id(), "463ac35c9f6413ad48485a3953bb6124");
        assert_eq!(s.parent, Some(0xa2fb4a1d1a96d312));
        assert!(s.sampled);
        let up = s.upstream_headers();
        assert_eq!(up[0].1, Bytes::from_static(b"463ac35c9f6413ad48485a3953bb6124"));
        assert_eq!(up[2].1, Bytes::from_static(b"a2fb4a1d1a96d312"));
        // 来た判断が 0 なら送らない
        let h0 = |n: &str| (n == "b3").then(|| Bytes::from_static(b"0"));
        assert!(!begin(&cfg(), &h0).sampled);
        // 文脈が無ければ新しい 128bit のトレース
        let none = |_: &str| None;
        assert_eq!(begin(&cfg(), &none).trace_id().len(), 32);
    }
}
