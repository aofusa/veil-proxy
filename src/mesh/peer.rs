//! ワークロードの属性と Istio の peer metadata の交換（F-221）。
//!
//! Istio の Envoy は HTTP の要求・応答に `X-Envoy-Peer-Metadata`（ワークロードの属性を protobuf の
//! `google.protobuf.Struct` にして base64 したもの）と `X-Envoy-Peer-Metadata-Id`（node id）を付けて相手に
//! 自分を知らせる（`istio.metadata_exchange` の `istio_headers`）。標準メトリクスの送信元・宛先のラベルは
//! これで決まる。受け取った値は id ごとにスレッドローカルにキャッシュし、要求ごとに解析し直さない。

use super::xds::proto::{Struct, Value};
use bytes::Bytes;
use once_cell::sync::OnceCell;
use prost::Message;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

/// ワークロードの属性（観測のラベル）。
#[derive(Debug, Default, Clone, PartialEq, Eq, Hash)]
pub struct Workload {
    pub name: String,
    pub namespace: String,
    pub app: String,
    pub version: String,
    pub canonical_service: String,
    pub canonical_revision: String,
    pub cluster: String,
    /// SPIFFE ID（mTLS の相手。メタデータからは決まらない）
    pub principal: String,
}

pub const HDR_METADATA: &[u8] = b"x-envoy-peer-metadata";
pub const HDR_METADATA_ID: &[u8] = b"x-envoy-peer-metadata-id";

/// 自分（このプロキシ）の属性。
pub struct LocalNode {
    pub id: String,
    pub workload: Arc<Workload>,
    /// `X-Envoy-Peer-Metadata` の値（base64）
    pub header_value: Bytes,
    pub header_id: Bytes,
    /// メッシュの ID（`MESH_ID`）・信頼ドメイン
    pub mesh_id: String,
    /// `INSTANCE_IPS`
    pub instance_ips: Vec<std::net::IpAddr>,
}

static LOCAL: OnceCell<LocalNode> = OnceCell::new();

pub fn local() -> Option<&'static LocalNode> {
    LOCAL.get()
}

/// ブートストラップの `node.metadata`（Struct）から自分の属性を作る。
pub fn init_local(node_id: &str, metadata: &Struct) {
    let s = |k: &str| metadata.get_str(k).unwrap_or("").to_string();
    let labels = metadata.get_struct("LABELS");
    let l = |k: &str| labels.and_then(|l| l.get_str(k)).unwrap_or("").to_string();
    let workload_name = s("WORKLOAD_NAME");
    let canonical = {
        let c = l("service.istio.io/canonical-name");
        if c.is_empty() {
            let a = l("app");
            if a.is_empty() {
                workload_name.clone()
            } else {
                a
            }
        } else {
            c
        }
    };
    let revision = {
        let r = l("service.istio.io/canonical-revision");
        if r.is_empty() {
            let v = l("version");
            if v.is_empty() {
                "latest".to_string()
            } else {
                v
            }
        } else {
            r
        }
    };
    let workload = Workload {
        name: workload_name.clone(),
        namespace: s("NAMESPACE"),
        app: l("app"),
        version: l("version"),
        canonical_service: canonical.clone(),
        canonical_revision: revision.clone(),
        cluster: s("CLUSTER_ID"),
        principal: String::new(),
    };
    // 送るメタデータ（Envoy と同じ項目）
    let mut out = Struct::default();
    let mut put = |k: &str, v: &str| {
        if !v.is_empty() {
            out.fields.insert(k.to_string(), Value::string(v));
        }
    };
    put("CLUSTER_ID", &workload.cluster);
    put("NAME", &s("NAME"));
    put("NAMESPACE", &workload.namespace);
    put("OWNER", &s("OWNER"));
    put("WORKLOAD_NAME", &workload.name);
    let mut lab = Struct::default();
    for (k, v) in [
        ("app", workload.app.as_str()),
        ("version", workload.version.as_str()),
        ("service.istio.io/canonical-name", canonical.as_str()),
        ("service.istio.io/canonical-revision", revision.as_str()),
    ] {
        if !v.is_empty() {
            lab.fields.insert(k.to_string(), Value::string(v));
        }
    }
    out.fields.insert(
        "LABELS".to_string(),
        Value {
            kind: Some(super::xds::proto::value::Kind::StructValue(lab)),
        },
    );
    let header_value = Bytes::from(super::httpcore::base64_encode(&out.encode_to_vec()));
    let instance_ips = s("INSTANCE_IPS")
        .split(',')
        .filter_map(|ip| ip.trim().parse().ok())
        .collect();
    let _ = LOCAL.set(LocalNode {
        id: node_id.to_string(),
        workload: Arc::new(workload),
        header_value,
        header_id: Bytes::from(node_id.to_string()),
        mesh_id: s("MESH_ID"),
        instance_ips,
    });
}

/// 受け取った `X-Envoy-Peer-Metadata` を解析する。
pub fn decode(value: &[u8]) -> Option<Workload> {
    let raw = super::httpcore::base64_decode(value)?;
    let st = Struct::decode(raw.as_slice()).ok()?;
    let s = |k: &str| st.get_str(k).unwrap_or("").to_string();
    let labels = st.get_struct("LABELS");
    let l = |k: &str| labels.and_then(|l| l.get_str(k)).unwrap_or("").to_string();
    let name = s("WORKLOAD_NAME");
    let mut canonical = l("service.istio.io/canonical-name");
    if canonical.is_empty() {
        canonical = l("app");
    }
    if canonical.is_empty() {
        canonical = name.clone();
    }
    let mut revision = l("service.istio.io/canonical-revision");
    if revision.is_empty() {
        revision = l("version");
    }
    if revision.is_empty() {
        revision = "latest".to_string();
    }
    Some(Workload {
        name,
        namespace: s("NAMESPACE"),
        app: l("app"),
        version: l("version"),
        canonical_service: canonical,
        canonical_revision: revision,
        cluster: s("CLUSTER_ID"),
        principal: String::new(),
    })
}

/// EDS のエンドポイントの `istio.workload`（"name;namespace;canonical;revision;cluster"）から属性を作る。
pub fn from_endpoint_workload(w: &[u8]) -> Option<Workload> {
    let s = std::str::from_utf8(w).ok()?;
    let mut it = s.split(';');
    let name = it.next()?.to_string();
    let namespace = it.next().unwrap_or("").to_string();
    let canonical = it.next().unwrap_or("").to_string();
    let revision = it.next().unwrap_or("").to_string();
    let cluster = it.next().unwrap_or("").to_string();
    Some(Workload {
        name,
        namespace,
        app: canonical.clone(),
        version: revision.clone(),
        canonical_service: canonical,
        canonical_revision: revision,
        cluster,
        principal: String::new(),
    })
}

/// 送信元の IP → ワークロード（EDS のエンドポイントの `istio.workload` から。Envoy の `workload_discovery` の代わり）。
/// 受け側で要求に peer metadata のヘッダが無いとき（Envoy のクライアント等）に引く。xDS のスレッドが EDS の応答ごとに
/// 作り直して差し替え、データプレーンは読むだけ（接続ごとに 1 回）。
static BY_IP: once_cell::sync::Lazy<arc_swap::ArcSwap<HashMap<std::net::IpAddr, Arc<Workload>>>> =
    once_cell::sync::Lazy::new(|| arc_swap::ArcSwap::from_pointee(HashMap::new()));
/// EDS の名前 → そのエンドポイントの (IP, ワークロード)（xDS のスレッドだけが触る）
static BY_EDS: std::sync::Mutex<Option<HashMap<String, Vec<(std::net::IpAddr, Arc<Workload>)>>>> =
    std::sync::Mutex::new(None);

/// EDS の 1 リソースのエンドポイントのワークロードを登録する（`None` は削除）。`publish_endpoint_workloads` で反映。
pub fn set_endpoint_workloads(eds: &str, eps: Option<Vec<(std::net::IpAddr, Arc<Workload>)>>) {
    let mut g = BY_EDS.lock().unwrap_or_else(|e| e.into_inner());
    let m = g.get_or_insert_with(HashMap::new);
    match eps {
        Some(v) => {
            m.insert(eds.to_string(), v);
        }
        None => {
            m.remove(eds);
        }
    }
}

/// 登録したものから IP の表を作り直して差し替える（EDS の応答ごとに 1 回）。
pub fn publish_endpoint_workloads() {
    let g = BY_EDS.lock().unwrap_or_else(|e| e.into_inner());
    let mut out = HashMap::new();
    if let Some(m) = g.as_ref() {
        // EDS 由来を先に、WDS（`wds:`）を後に入れる（同じ IP なら WDS のワークロード名を使う）
        for wds in [false, true] {
            for (k, v) in m.iter() {
                if k.starts_with("wds:") != wds {
                    continue;
                }
                for (ip, w) in v {
                    out.insert(*ip, w.clone());
                }
            }
        }
    }
    BY_IP.store(Arc::new(out));
}

/// 送信元の IP のワークロード。
pub fn by_ip(ip: std::net::IpAddr) -> Option<Arc<Workload>> {
    BY_IP.load().get(&ip).cloned()
}

thread_local! {
    /// id（または値）→ 解析済みの属性。上限を超えたら捨てて作り直す
    static CACHE: RefCell<HashMap<Bytes, Arc<Workload>>> = RefCell::new(HashMap::new());
}

const CACHE_MAX: usize = 4096;

/// 要求の peer metadata（id があれば id をキーにキャッシュ）。
pub fn cached_decode(id: Option<&Bytes>, value: &Bytes) -> Option<Arc<Workload>> {
    let key = id.unwrap_or(value);
    if let Some(w) = CACHE.with(|c| c.borrow().get(key).cloned()) {
        return Some(w);
    }
    let w = Arc::new(decode(value)?);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_MAX {
            c.clear();
        }
        c.insert(key.clone(), w.clone());
    });
    Some(w)
}

/// エンドポイントの `istio.workload` のキャッシュ版。
pub fn cached_endpoint(w: &Bytes) -> Option<Arc<Workload>> {
    if w.is_empty() {
        return None;
    }
    if let Some(v) = CACHE.with(|c| c.borrow().get(w).cloned()) {
        return Some(v);
    }
    let v = Arc::new(from_endpoint_workload(w)?);
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_MAX {
            c.clear();
        }
        c.insert(w.clone(), v.clone());
    });
    Some(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_envoy_peer_metadata() {
        // Istio 1.31 の Envoy が実際に付けた値（client.demo）
        let v = b"ChoKCkNMVVNURVJfSUQSDBoKS3ViZXJuZXRlcwp7CgZMQUJFTFMScSpvCg8KA2FwcBIIGgZjbGllbnQKKwofc2VydmljZS5pc3Rpby5pby9jYW5vbmljYWwtbmFtZRIIGgZjbGllbnQKLwojc2VydmljZS5pc3Rpby5pby9jYW5vbmljYWwtcmV2aXNpb24SCBoGbGF0ZXN0CiEKBE5BTUUSGRoXY2xpZW50LTc3NDU5ZDg0NjktbHc3ajgKEwoJTkFNRVNQQUNFEgYaBGRlbW8KRwoFT1dORVISPho8a3ViZXJuZXRlczovL2FwaXMvYXBwcy92MS9uYW1lc3BhY2VzL2RlbW8vZGVwbG95bWVudHMvY2xpZW50ChkKDVdPUktMT0FEX05BTUUSCBoGY2xpZW50";
        let w = decode(v).unwrap();
        assert_eq!(w.name, "client");
        assert_eq!(w.namespace, "demo");
        assert_eq!(w.app, "client");
        assert_eq!(w.canonical_service, "client");
        assert_eq!(w.canonical_revision, "latest");
        assert_eq!(w.cluster, "Kubernetes");
    }

    #[test]
    fn endpoint_workload() {
        let w = from_endpoint_workload(b"whoami;demo;whoami;v1;Kubernetes").unwrap();
        assert_eq!(w.name, "whoami");
        assert_eq!(w.canonical_revision, "v1");
    }
}
