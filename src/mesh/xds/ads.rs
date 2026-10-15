//! Delta ADS のクライアント（F-222）: istiod の xDS を pilot-agent の UDS 経由で購読し、メッシュモデルへ適用する。
//!
//! - 専用スレッドで veil のランタイムを 1 本動かし（`runtime::block_on`）、UDS の上に HTTP/2（`upstream_mux`）で
//!   `DeltaAggregatedResources` の双方向ストリームを 1 本開く（`vds_grpc` と同じ作り。tokio・tonic は使わない）。
//! - CDS・LDS はワイルドカード、EDS・RDS は CDS・LDS が参照する名前だけを購読する（差分を送る）。
//! - 応答ごとに変換して適用し、ACK（`response_nonce`）か NACK（`error_detail`）を返す。変換できないリソースは
//!   前の版のまま残す（認可のフィルタを含むリスナーを部分的に適用しない）。
//! - 切れたら指数バックオフで張り直す。張り直しでは既知の版（`initial_resource_versions`）を伝える。

use super::proto::{self as p, decode_any};
use super::translate;
use crate::grpc::framing::GrpcFrameDecoder;
use crate::mesh::model::*;
use crate::mesh::state::{state, ListenerTable};
use bytes::{BufMut, Bytes, BytesMut};
use ftlog::{debug, info, warn};
use prost::Message;
use std::collections::{BTreeSet, HashMap};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub const PATH: &str =
    "/envoy.service.discovery.v3.AggregatedDiscoveryService/DeltaAggregatedResources";

#[derive(Clone)]
pub struct AdsConfig {
    pub uds_path: PathBuf,
    pub node: p::Node,
}

/// 生のリソース（管理 API の `config_dump`・再変換用）。
#[derive(Default)]
pub struct RawResources {
    pub listeners: HashMap<String, (String, p::Listener)>,
    pub clusters: HashMap<String, (String, p::Cluster)>,
    pub routes: HashMap<String, (String, p::RouteConfiguration)>,
    pub endpoints: HashMap<String, (String, p::ClusterLoadAssignment)>,
    /// ECDS（HTTP フィルタの設定。Istio の WasmPlugin）
    pub ecds: HashMap<String, (String, p::TypedExtensionConfig)>,
    /// 最後の NACK の理由（型 → 理由）
    pub last_nack: HashMap<String, String>,
}

pub static RAW: once_cell::sync::Lazy<Mutex<RawResources>> =
    once_cell::sync::Lazy::new(|| Mutex::new(RawResources::default()));

fn frame<M: Message>(m: &M) -> Bytes {
    let len = m.encoded_len();
    let mut b = BytesMut::with_capacity(5 + len);
    b.put_u8(0);
    b.put_u32(len as u32);
    let _ = m.encode(&mut b);
    b.freeze()
}

/// 購読の状態（接続をまたいで保つ）。
#[derive(Default)]
struct Subs {
    /// 購読中の RDS / EDS の名前
    rds: BTreeSet<String>,
    eds: BTreeSet<String>,
    /// 変換済みのリスナー（名前 → 版と変換結果）
    listeners: HashMap<String, Arc<MeshListener>>,
    /// EDS の名前 → それを使うクラスタのスロット
    eds_slots: HashMap<String, Vec<Arc<ClusterSlot>>>,
    /// ADS で購読中の秘密（ゲートウェイの `kubernetes://<credentialName>`）
    secrets: BTreeSet<String>,
    /// 購読中の ECDS の名前
    ecds: BTreeSet<String>,
}

pub fn spawn(cfg: AdsConfig) {
    let r = std::thread::Builder::new()
        .name("veil-xds".into())
        .spawn(move || {
            crate::runtime::block_on(async move { run(cfg).await });
            crate::runtime::shutdown_thread();
        });
    if let Err(e) = r {
        warn!("[xds] cannot spawn the xDS thread: {}", e);
    }
}

async fn run(cfg: AdsConfig) {
    let mut subs = Subs::default();
    let mut backoff = 200u64;
    let mut first = true;
    loop {
        if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            return;
        }
        let mut got = false;
        match session(&cfg, &mut subs, first, &mut got).await {
            Ok(()) => info!("[xds] stream ended; reconnecting"),
            Err(e) => warn!("[xds] stream failed: {}", e),
        }
        state().xds_connected.store(false, Ordering::Relaxed);
        first = false;
        if got {
            backoff = 200;
        }
        crate::runtime::time::sleep(Duration::from_millis(backoff)).await;
        backoff = (backoff * 2).min(10_000);
    }
}

async fn session(
    cfg: &AdsConfig,
    subs: &mut Subs,
    first: bool,
    got: &mut bool,
) -> Result<(), String> {
    use crate::http2::upstream_mux::{H2Mux, UpResp};
    let io = crate::runtime::time::timeout(
        Duration::from_secs(10),
        crate::runtime::tcp::TcpStream::connect_unix(&cfg.uds_path),
    )
    .await
    .map_err(|_| "connect timeout".to_string())?
    .map_err(|e| format!("connect {}: {}", cfg.uds_path.display(), e))?;
    let mux = H2Mux::start(io, false, Duration::ZERO);
    let headers: [(&[u8], &[u8]); 3] = [
        (b"content-type", b"application/grpc"),
        (b"te", b"trailers"),
        (b"user-agent", b"veil-envoy-compat"),
    ];
    let mut stream = mux
        .open(
            b"POST",
            PATH.as_bytes(),
            b"xds-grpc",
            headers.iter().copied(),
            false,
        )
        .ok_or("cannot open an HTTP/2 stream")?;

    // 初回: CDS・LDS はワイルドカード、既知の EDS・RDS は名前で（張り直しでは既知の版も伝える）
    let versions = |ty: &str| -> HashMap<String, String> {
        if first {
            return HashMap::new();
        }
        let raw = RAW.lock().unwrap_or_else(|e| e.into_inner());
        match ty {
            p::TYPE_CLUSTER => raw
                .clusters
                .iter()
                .map(|(k, v)| (k.clone(), v.0.clone()))
                .collect(),
            p::TYPE_LISTENER => raw
                .listeners
                .iter()
                .map(|(k, v)| (k.clone(), v.0.clone()))
                .collect(),
            _ => HashMap::new(),
        }
    };
    for (i, ty) in [p::TYPE_CLUSTER, p::TYPE_LISTENER].into_iter().enumerate() {
        send(
            &stream,
            p::DeltaDiscoveryRequest {
                node: (i == 0).then(|| cfg.node.clone()),
                type_url: ty.into(),
                resource_names_subscribe: vec!["*".into()],
                initial_resource_versions: versions(ty),
                ..Default::default()
            },
        )
        .await?;
    }
    if !subs.eds.is_empty() {
        send(
            &stream,
            p::DeltaDiscoveryRequest {
                type_url: p::TYPE_ENDPOINT.into(),
                resource_names_subscribe: subs.eds.iter().cloned().collect(),
                ..Default::default()
            },
        )
        .await?;
    }
    if !subs.rds.is_empty() {
        send(
            &stream,
            p::DeltaDiscoveryRequest {
                type_url: p::TYPE_ROUTE.into(),
                resource_names_subscribe: subs.rds.iter().cloned().collect(),
                ..Default::default()
            },
        )
        .await?;
    }
    if !subs.ecds.is_empty() {
        send(
            &stream,
            p::DeltaDiscoveryRequest {
                type_url: p::TYPE_ECDS.into(),
                resource_names_subscribe: subs.ecds.iter().cloned().collect(),
                ..Default::default()
            },
        )
        .await?;
    }
    if !subs.secrets.is_empty() {
        send(
            &stream,
            p::DeltaDiscoveryRequest {
                type_url: p::TYPE_SECRET.into(),
                resource_names_subscribe: subs.secrets.iter().cloned().collect(),
                ..Default::default()
            },
        )
        .await?;
    }

    let mut decoder = GrpcFrameDecoder::with_max_size(256 * 1024 * 1024);
    loop {
        let item =
            match crate::runtime::time::timeout(Duration::from_millis(500), stream.resp_rx.recv())
                .await
            {
                Err(_) => {
                    if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        stream.finish_request();
                        return Ok(());
                    }
                    continue;
                }
                Ok(i) => i,
            };
        match item {
            Some(UpResp::Head { status, headers }) => {
                if status != 200 {
                    return Err(format!("xDS server returned HTTP {}", status));
                }
                if let Some(e) = grpc_error(&headers) {
                    return Err(e);
                }
                state().xds_connected.store(true, Ordering::Relaxed);
            }
            Some(UpResp::Data(chunk)) => {
                decoder.push(&chunk);
                loop {
                    let f = match decoder.decode_next() {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(e) => return Err(format!("bad gRPC frame: {}", e)),
                    };
                    let resp = p::DeltaDiscoveryResponse::decode(&f.data[..])
                        .map_err(|e| format!("invalid DeltaDiscoveryResponse: {}", e))?;
                    *got = true;
                    for req in handle(resp, subs) {
                        send(&stream, req).await?;
                    }
                }
            }
            Some(UpResp::Trailers(t)) => {
                return match grpc_error(&t) {
                    Some(e) => Err(e),
                    None => Ok(()),
                };
            }
            Some(UpResp::Reset { .. }) => return Err("stream reset".into()),
            None => return Ok(()),
        }
    }
}

async fn send(
    stream: &crate::http2::upstream_mux::UpStream,
    req: p::DeltaDiscoveryRequest,
) -> Result<(), String> {
    match &stream.req_tx {
        Some(tx) => tx
            .send(frame(&req))
            .await
            .map_err(|_| "request stream closed".to_string()),
        None => Err("request stream closed".to_string()),
    }
}

fn grpc_error(headers: &[(Bytes, Bytes)]) -> Option<String> {
    let status = headers
        .iter()
        .find(|(k, _)| k.as_ref() == b"grpc-status")
        .map(|(_, v)| v.clone())?;
    if status.as_ref() == b"0" {
        return None;
    }
    let msg = headers
        .iter()
        .find(|(k, _)| k.as_ref() == b"grpc-message")
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        .unwrap_or_default();
    Some(format!(
        "grpc-status {}: {}",
        String::from_utf8_lossy(&status),
        msg
    ))
}

/// 応答 1 つを適用し、送る要求（ACK/NACK と購読の差分）を返す。
fn handle(resp: p::DeltaDiscoveryResponse, subs: &mut Subs) -> Vec<p::DeltaDiscoveryRequest> {
    let ty = resp.type_url.clone();
    let mut errors: Vec<String> = Vec::new();
    let mut out = Vec::new();
    match ty.as_str() {
        p::TYPE_CLUSTER => apply_clusters(&resp, subs, &mut errors, &mut out),
        p::TYPE_LISTENER => apply_listeners(&resp, subs, &mut errors, &mut out),
        p::TYPE_ROUTE => apply_routes(&resp, &mut errors),
        p::TYPE_ENDPOINT => apply_endpoints(&resp, subs, &mut errors),
        p::TYPE_SECRET => apply_secrets(&resp, &mut errors),
        p::TYPE_ECDS => apply_ecds(&resp, &mut errors),
        other => {
            // 購読していない型（Istio の拡張等）。ACK だけ返す
            debug!("[xds] ignoring resources of type {}", other);
        }
    }
    let error_detail = if errors.is_empty() {
        RAW.lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_nack
            .remove(&ty);
        None
    } else {
        let msg = errors.join("; ");
        warn!("[xds] NACK {}: {}", ty, msg);
        RAW.lock()
            .unwrap_or_else(|e| e.into_inner())
            .last_nack
            .insert(ty.clone(), msg.clone());
        if ty == p::TYPE_LISTENER {
            state().lds_rejected.fetch_add(1, Ordering::Relaxed);
        } else if ty == p::TYPE_CLUSTER {
            state().cds_rejected.fetch_add(1, Ordering::Relaxed);
        }
        Some(p::Status {
            code: 3,
            message: msg,
        })
    };
    // ゲートウェイの証明書（変換で参照された `kubernetes://...`）を ADS で購読する
    let wanted = crate::mesh::envoy::sds::wanted_ads();
    let sub: Vec<String> = wanted.difference(&subs.secrets).cloned().collect();
    if !sub.is_empty() {
        out.push(p::DeltaDiscoveryRequest {
            type_url: p::TYPE_SECRET.into(),
            resource_names_subscribe: sub,
            ..Default::default()
        });
        subs.secrets = wanted;
    }
    // ACK / NACK は購読の差分より先に送る
    out.insert(
        0,
        p::DeltaDiscoveryRequest {
            type_url: ty,
            response_nonce: resp.nonce,
            error_detail,
            ..Default::default()
        },
    );
    out
}

fn decode_resources<M: Message + Default>(
    resp: &p::DeltaDiscoveryResponse,
    type_url: &str,
    errors: &mut Vec<String>,
) -> Vec<(String, String, M)> {
    let mut v = Vec::with_capacity(resp.resources.len());
    for r in &resp.resources {
        let Some(any) = &r.resource else { continue };
        match decode_any::<M>(any, type_url) {
            Some(m) => v.push((r.name.clone(), r.version.clone(), m)),
            None => errors.push(format!("cannot decode {} ({})", r.name, any.type_url)),
        }
    }
    v
}

fn apply_clusters(
    resp: &p::DeltaDiscoveryResponse,
    subs: &mut Subs,
    errors: &mut Vec<String>,
    out: &mut Vec<p::DeltaDiscoveryRequest>,
) {
    let items: Vec<(String, String, p::Cluster)> = decode_resources(resp, p::TYPE_CLUSTER, errors);
    let mut reg = state().registry.lock().unwrap_or_else(|e| e.into_inner());
    let mut raw = RAW.lock().unwrap_or_else(|e| e.into_inner());
    for (name, version, c) in items {
        match translate::cluster(&c) {
            Ok(cfg) => {
                let slot = reg.cluster_slot(&name);
                if let Some(eps) = translate::static_endpoints(&c) {
                    slot.replace_endpoints(eps);
                }
                if cfg.kind == ClusterKind::OriginalDst {
                    slot.endpoints.store(Arc::new(EndpointSet::default()));
                }
                slot.cfg.store(Arc::new(Some(Arc::new(cfg))));
                raw.clusters.insert(name, (version, c));
            }
            Err(e) => errors.push(e),
        }
    }
    for name in &resp.removed_resources {
        raw.clusters.remove(name);
        if let Some(slot) = reg.clusters.get(name) {
            slot.cfg.store(Arc::new(None));
        }
    }
    // EDS の購読を作り直す
    let mut eds_slots: HashMap<String, Vec<Arc<ClusterSlot>>> = HashMap::new();
    for (name, (_, c)) in raw.clusters.iter() {
        if c.r#type == 3 {
            let eds_name = c
                .eds_cluster_config
                .as_ref()
                .map(|e| e.service_name.clone())
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| name.clone());
            eds_slots
                .entry(eds_name)
                .or_default()
                .push(reg.cluster_slot(name));
        }
    }
    // 参照も定義も無いスロットは登録簿から外す（参照している側は `Arc` を持っているので消えない）
    reg.clusters
        .retain(|k, s| raw.clusters.contains_key(k) || Arc::strong_count(s) > 1);
    let wanted: BTreeSet<String> = eds_slots.keys().cloned().collect();
    let sub: Vec<String> = wanted.difference(&subs.eds).cloned().collect();
    let unsub: Vec<String> = subs.eds.difference(&wanted).cloned().collect();
    for n in &unsub {
        raw.endpoints.remove(n);
    }
    // 既に届いている EDS は新しいスロットにも入れる
    for (eds_name, slots) in &eds_slots {
        if let Some((_, la)) = raw.endpoints.get(eds_name) {
            let set = Arc::new(translate::endpoint_set(la, false));
            for s in slots {
                if s.endpoints.load().endpoints.is_empty() {
                    s.endpoints.store(set.clone());
                }
            }
        }
    }
    subs.eds = wanted;
    subs.eds_slots = eds_slots;
    if !sub.is_empty() || !unsub.is_empty() {
        out.push(p::DeltaDiscoveryRequest {
            type_url: p::TYPE_ENDPOINT.into(),
            resource_names_subscribe: sub,
            resource_names_unsubscribe: unsub,
            ..Default::default()
        });
    }
    let st = state();
    st.cds_updates.fetch_add(1, Ordering::Relaxed);
    st.cds_ready.store(true, Ordering::Relaxed);
}

fn apply_listeners(
    resp: &p::DeltaDiscoveryResponse,
    subs: &mut Subs,
    errors: &mut Vec<String>,
    out: &mut Vec<p::DeltaDiscoveryRequest>,
) {
    let items: Vec<(String, String, p::Listener)> =
        decode_resources(resp, p::TYPE_LISTENER, errors);
    let mut reg = state().registry.lock().unwrap_or_else(|e| e.into_inner());
    let mut raw = RAW.lock().unwrap_or_else(|e| e.into_inner());
    for (name, version, l) in items {
        match translate::listener(&l, &mut reg) {
            Ok(ml) => {
                subs.listeners.insert(name.clone(), Arc::new(ml));
                raw.listeners.insert(name, (version, l));
            }
            Err(e) => errors.push(e),
        }
    }
    for name in &resp.removed_resources {
        subs.listeners.remove(name);
        raw.listeners.remove(name);
    }
    // 表を作り直す
    let mut table = ListenerTable {
        version: state().lds_updates.load(Ordering::Relaxed) + 1,
        ..Default::default()
    };
    for l in subs.listeners.values() {
        if l.internal {
            table.internal.insert(l.name.clone(), l.clone());
        } else if l.bind_to_port {
            table.bound.push(l.clone());
        } else if l.addr.ip().is_unspecified() {
            table.by_port.insert(l.addr.port(), l.clone());
        } else {
            table.by_addr.insert(l.addr, l.clone());
        }
    }
    table.bound.sort_by(|a, b| a.name.cmp(&b.name));
    state().listeners.store(Arc::new(table));

    // RDS の購読を作り直す（現在のリスナーが参照する名前）
    let mut wanted = BTreeSet::new();
    for l in subs.listeners.values() {
        for c in l.chains.iter().chain(l.default_chain.iter()) {
            if let ChainAction::Http(h) = &c.action {
                if let RouteRef::Rds(s) = &h.routes {
                    wanted.insert(s.name.clone());
                }
            }
        }
    }
    reg.routes
        .retain(|k, s| wanted.contains(k) || Arc::strong_count(s) > 1);
    let sub: Vec<String> = wanted.difference(&subs.rds).cloned().collect();
    let unsub: Vec<String> = subs.rds.difference(&wanted).cloned().collect();
    for n in &unsub {
        raw.routes.remove(n);
    }
    // 既に届いている RDS は（スロットが作り直されていても）入っている。未着のものを購読する
    subs.rds = wanted;
    if !sub.is_empty() || !unsub.is_empty() {
        out.push(p::DeltaDiscoveryRequest {
            type_url: p::TYPE_ROUTE.into(),
            resource_names_subscribe: sub,
            resource_names_unsubscribe: unsub,
            ..Default::default()
        });
    }
    // ECDS の購読を作り直す
    let mut wanted = BTreeSet::new();
    for l in subs.listeners.values() {
        for c in l.chains.iter().chain(l.default_chain.iter()) {
            if let ChainAction::Http(h) = &c.action {
                for f in &h.filters {
                    if let HttpFilterCfg::Ecds(s) = f {
                        wanted.insert(s.name.clone());
                    }
                }
            }
        }
    }
    reg.ecds
        .retain(|k, s| wanted.contains(k) || Arc::strong_count(s) > 1);
    let sub: Vec<String> = wanted.difference(&subs.ecds).cloned().collect();
    let unsub: Vec<String> = subs.ecds.difference(&wanted).cloned().collect();
    for n in &unsub {
        raw.ecds.remove(n);
    }
    subs.ecds = wanted;
    if !sub.is_empty() || !unsub.is_empty() {
        out.push(p::DeltaDiscoveryRequest {
            type_url: p::TYPE_ECDS.into(),
            resource_names_subscribe: sub,
            resource_names_unsubscribe: unsub,
            ..Default::default()
        });
    }
    let st = state();
    st.lds_updates.fetch_add(1, Ordering::Relaxed);
    st.lds_ready.store(true, Ordering::Relaxed);
}

fn apply_routes(resp: &p::DeltaDiscoveryResponse, errors: &mut Vec<String>) {
    let items: Vec<(String, String, p::RouteConfiguration)> =
        decode_resources(resp, p::TYPE_ROUTE, errors);
    let mut reg = state().registry.lock().unwrap_or_else(|e| e.into_inner());
    let mut raw = RAW.lock().unwrap_or_else(|e| e.into_inner());
    for (name, version, rc) in items {
        match translate::route_table(&rc, &mut reg) {
            Ok(t) => {
                let slot = reg.route_slot(&name);
                slot.table.store(Arc::new(t));
                slot.ready.store(true, Ordering::Relaxed);
                raw.routes.insert(name, (version, rc));
            }
            Err(e) => errors.push(format!("route {}: {}", name, e)),
        }
    }
    for name in &resp.removed_resources {
        raw.routes.remove(name);
        if let Some(s) = reg.routes.get(name) {
            s.table.store(Arc::new(RouteTable::empty(name)));
        }
    }
    state().rds_updates.fetch_add(1, Ordering::Relaxed);
}

fn apply_endpoints(resp: &p::DeltaDiscoveryResponse, subs: &mut Subs, errors: &mut Vec<String>) {
    let items: Vec<(String, String, p::ClusterLoadAssignment)> =
        decode_resources(resp, p::TYPE_ENDPOINT, errors);
    let mut raw = RAW.lock().unwrap_or_else(|e| e.into_inner());
    for (name, version, la) in items {
        if let Some(slots) = subs.eds_slots.get(&name) {
            // スロットごとに前の状態（同時要求数・外れ値検出）を引き継ぐ
            for s in slots {
                s.replace_endpoints(translate::endpoint_set(&la, false));
            }
        }
        raw.endpoints.insert(name, (version, la));
    }
    for name in &resp.removed_resources {
        raw.endpoints.remove(name);
        if let Some(slots) = subs.eds_slots.get(name) {
            for s in slots {
                s.endpoints.store(Arc::new(EndpointSet::default()));
            }
        }
    }
    state().eds_updates.fetch_add(1, Ordering::Relaxed);
}

fn apply_secrets(resp: &p::DeltaDiscoveryResponse, errors: &mut Vec<String>) {
    let items: Vec<(String, String, p::Secret)> = decode_resources(resp, p::TYPE_SECRET, errors);
    for (name, _version, mut sec) in items {
        if sec.name.is_empty() {
            sec.name = name.clone();
        }
        if let Err(e) = crate::mesh::envoy::sds::apply(&sec) {
            errors.push(format!("secret {}: {}", name, e));
        }
    }
}

/// ECDS（Istio の WasmPlugin）。Wasm のモジュールはここで読み込む（コンパイルは xDS のスレッドで済ませる）。
fn apply_ecds(resp: &p::DeltaDiscoveryResponse, errors: &mut Vec<String>) {
    let items: Vec<(String, String, p::TypedExtensionConfig)> =
        decode_resources(resp, p::TYPE_ECDS, errors);
    for (name, version, tc) in items {
        let name = if tc.name.is_empty() {
            name
        } else {
            tc.name.clone()
        };
        let Some(any) = &tc.typed_config else {
            errors.push(format!("ECDS {} without typed_config", name));
            continue;
        };
        match translate::ecds_filter(&name, any) {
            Ok(f) => {
                let slot = state()
                    .registry
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ecds_slot(&name);
                slot.filter.store(Some(Arc::new(f)));
                info!("[xds] ECDS {} applied", name);
                RAW.lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .ecds
                    .insert(name, (version, tc));
            }
            Err(e) => errors.push(format!("ECDS {}: {}", name, e)),
        }
    }
    for name in &resp.removed_resources {
        RAW.lock()
            .unwrap_or_else(|e| e.into_inner())
            .ecds
            .remove(name);
    }
}
