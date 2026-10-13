//! VDS（Veil Discovery Service）の参照制御プレーン（F-04 / F-199）。
//!
//! スナップショット（veil の config.toml と同じ書式の `[[route]]` と `[upstreams.<name>]` だけを書いた TOML）を
//! 監視し、変わったら配信する。xDS の State-of-the-World と同じ意味論（version_info・nonce・ACK/NACK）で、
//! 1 つのポートに次を載せる（HTTP/1.1・h2c、`--tls-cert`/`--tls-key` で TLS）:
//!
//! - gRPC `veil.vds.v1.VeilDiscoveryService/StreamSnapshots`（双方向ストリーム。版が変わったらすぐ押し出す）
//! - REST `POST /v1/discovery`（長輪詢。`tools/vds/vds_server.py` と同じ）
//! - `GET /v1/status`（ノードごとの ACK 済みの版・最後の NACK）、`GET /healthz`
//!
//! NACK された版は内容が変わるまで送り直さない（xDS と同じ）。

use axum::extract::State as AxState;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response as AxResponse};
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, watch};
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};

pub mod pb {
    tonic::include_proto!("veil.vds.v1");
}
use pb::veil_discovery_service_server::{VeilDiscoveryService, VeilDiscoveryServiceServer};

const TYPE_SNAPSHOT: &str = "type.veil.dev/veil.v1.Snapshot";
const TYPE_ROUTE: &str = "type.veil.dev/veil.v1.Route";
const TYPE_UPSTREAM: &str = "type.veil.dev/veil.v1.Upstream";

/// 現在の版。
#[derive(Clone, Default)]
struct Snapshot {
    version: String,
    /// REST 版の形（`@type`・`name` と本体）
    resources: Vec<Value>,
}

#[derive(Default)]
struct NodeState {
    acked_version: String,
    last_nack: Option<Value>,
    nacks: u64,
    rejected_version: Option<String>,
    transport: &'static str,
}

struct Shared {
    snap: watch::Receiver<Snapshot>,
    nodes: Mutex<HashMap<String, NodeState>>,
    nonce: std::sync::atomic::AtomicU64,
    token: Option<String>,
    hold_max: f64,
    load_error: Mutex<Option<String>>,
}

impl Shared {
    fn next_nonce(&self) -> String {
        (self.nonce.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1).to_string()
    }

    /// 要求の version_info・error_detail をノードの状態に記録し、そのノードへ送るべきでない版（NACK 済み）を返す。
    fn record(&self, node: &str, version: &str, nack: Option<&str>, nonce: &str, transport: &'static str) -> Option<String> {
        let mut nodes = self.nodes.lock().unwrap();
        let st = nodes.entry(node.to_string()).or_default();
        st.transport = transport;
        match nack {
            Some(msg) => {
                st.last_nack = Some(json!({"nonce": nonce, "message": msg}));
                st.nacks += 1;
                st.rejected_version = Some(self.snap.borrow().version.clone());
                println!("[vds] NACK from {} ({}): {}", node, transport, msg);
            }
            None => st.acked_version = version.to_string(),
        }
        st.rejected_version.clone()
    }

    fn authorized(&self, auth: Option<&str>) -> bool {
        match &self.token {
            None => true,
            Some(t) => auth.is_some_and(|a| constant_eq(a.as_bytes(), format!("Bearer {}", t).as_bytes())),
        }
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// スナップショットのファイルを読んで版を作る（構文エラーは `Err`、前の版を保つ）。
fn load(path: &str) -> Result<Option<(Vec<u8>, Snapshot)>, String> {
    let raw = match std::fs::read(path) {
        Ok(r) => r,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.to_string()),
    };
    let text = String::from_utf8(raw.clone()).map_err(|e| e.to_string())?;
    let doc: toml::Table = toml::from_str(&text).map_err(|e| e.to_string())?;
    let mut resources = Vec::new();
    if let Some(toml::Value::Array(routes)) = doc.get("route") {
        for r in routes {
            let mut v = serde_json::to_value(r).map_err(|e| e.to_string())?;
            v.as_object_mut().ok_or("route must be a table")?.insert("@type".into(), json!(TYPE_ROUTE));
            resources.push(v);
        }
    }
    if let Some(toml::Value::Table(ups)) = doc.get("upstreams") {
        for (name, u) in ups {
            let mut v = serde_json::to_value(u).map_err(|e| e.to_string())?;
            let o = v.as_object_mut().ok_or("upstream must be a table")?;
            o.insert("@type".into(), json!(TYPE_UPSTREAM));
            o.insert("name".into(), json!(name));
            resources.push(v);
        }
    }
    let version: String = Sha256::digest(&raw).iter().take(8).map(|b| format!("{:02x}", b)).collect();
    Ok(Some((raw, Snapshot { version, resources })))
}

// ===================== gRPC =====================

fn to_pb(snap: &Snapshot, nonce: String) -> pb::DiscoveryResponse {
    let resources = snap
        .resources
        .iter()
        .map(|r| {
            let mut body = r.as_object().cloned().unwrap_or_default();
            let type_url = body.remove("@type").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
            let name = body.remove("name").and_then(|v| v.as_str().map(String::from)).unwrap_or_default();
            pb::Resource { type_url, name, json: Value::Object(body).to_string() }
        })
        .collect();
    pb::DiscoveryResponse { version_info: snap.version.clone(), nonce, type_url: TYPE_SNAPSHOT.into(), resources }
}

struct Grpc(Arc<Shared>);

type RespStream = Pin<Box<dyn Stream<Item = Result<pb::DiscoveryResponse, Status>> + Send>>;

#[tonic::async_trait]
impl VeilDiscoveryService for Grpc {
    type StreamSnapshotsStream = RespStream;

    async fn stream_snapshots(
        &self,
        req: Request<Streaming<pb::DiscoveryRequest>>,
    ) -> Result<Response<RespStream>, Status> {
        let auth = req.metadata().get("authorization").and_then(|v| v.to_str().ok());
        if !self.0.authorized(auth) {
            return Err(Status::unauthenticated("invalid or missing bearer token"));
        }
        let shared = self.0.clone();
        let mut inbound = req.into_inner();
        let (tx, rx) = mpsc::channel::<Result<pb::DiscoveryResponse, Status>>(4);
        tokio::spawn(async move {
            let mut snap_rx = shared.snap.clone();
            let mut node = String::from("unknown");
            let mut client_version: Option<String> = None;
            let mut rejected: Option<String> = None;
            // 送ったが ACK/NACK を待っている版
            let mut in_flight = false;
            loop {
                tokio::select! {
                    m = inbound.next() => {
                        let r = match m {
                            Some(Ok(r)) => r,
                            Some(Err(e)) => { println!("[vds] gRPC stream from {} failed: {}", node, e); break; }
                            None => break,
                        };
                        if let Some(n) = &r.node { node = n.id.clone(); }
                        let nack = r.error_detail.as_ref().map(|e| e.message.as_str());
                        rejected = shared.record(&node, &r.version_info, nack, &r.response_nonce, "grpc");
                        client_version = Some(r.version_info.clone());
                        in_flight = false;
                    }
                    c = snap_rx.changed() => { if c.is_err() { break; } }
                }
                let snap = snap_rx.borrow().clone();
                let Some(cv) = &client_version else { continue };
                if in_flight || snap.version.is_empty() || *cv == snap.version || rejected.as_deref() == Some(&snap.version) {
                    continue;
                }
                let resp = to_pb(&snap, shared.next_nonce());
                if tx.send(Ok(resp)).await.is_err() {
                    break;
                }
                in_flight = true;
            }
        });
        let out = tokio_stream::wrappers::ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(out)))
    }
}

// ===================== REST =====================

async fn discovery(AxState(sh): AxState<Arc<Shared>>, headers: HeaderMap, body: axum::body::Bytes) -> AxResponse {
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
    if !sh.authorized(auth) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    let req: Value = match serde_json::from_slice(if body.is_empty() { b"{}" } else { &body }) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid json"}))).into_response(),
    };
    let node = req["node"]["id"].as_str().unwrap_or("unknown").to_string();
    let version = req["version_info"].as_str().unwrap_or("").to_string();
    let nack = req["error_detail"]["message"].as_str().map(String::from);
    let rejected = sh.record(&node, &version, nack.as_deref(), req["response_nonce"].as_str().unwrap_or(""), "rest");
    let mut rx = sh.snap.clone();
    let current = rx.borrow().version.clone();
    let known = if rejected.as_deref() == Some(current.as_str()) { current.clone() } else { version };
    let hold = req["poll_timeout_secs"].as_f64().unwrap_or(sh.hold_max).min(sh.hold_max).max(0.0);
    if known == rx.borrow().version {
        let changed = tokio::time::timeout(Duration::from_secs_f64(hold), async {
            loop {
                if rx.changed().await.is_err() {
                    return false;
                }
                if rx.borrow().version != known {
                    return true;
                }
            }
        })
        .await;
        if !matches!(changed, Ok(true)) {
            return StatusCode::NOT_MODIFIED.into_response();
        }
    }
    let snap = rx.borrow().clone();
    Json(json!({
        "version_info": snap.version,
        "nonce": sh.next_nonce(),
        "type_url": TYPE_SNAPSHOT,
        "resources": snap.resources,
    }))
    .into_response()
}

async fn status(AxState(sh): AxState<Arc<Shared>>) -> Json<Value> {
    let nodes = sh.nodes.lock().unwrap();
    let nodes: serde_json::Map<String, Value> = nodes
        .iter()
        .map(|(k, s)| {
            (k.clone(), json!({
                "acked_version": s.acked_version,
                "last_nack": s.last_nack,
                "nacks": s.nacks,
                "transport": s.transport,
            }))
        })
        .collect();
    Json(json!({
        "version": sh.snap.borrow().version,
        "nodes": nodes,
        "load_error": *sh.load_error.lock().unwrap(),
    }))
}

// ===================== main =====================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: veil-vds-server --snapshot FILE [--listen ADDR] [--token-file FILE] [--hold SECS] [--tls-cert PEM --tls-key PEM]");
        return Ok(());
    }
    let snapshot = arg("--snapshot").ok_or("--snapshot is required")?;
    let listen: std::net::SocketAddr = arg("--listen").unwrap_or_else(|| "127.0.0.1:18000".into()).parse()?;
    let token = match arg("--token-file") {
        Some(p) => Some(std::fs::read_to_string(p)?.trim().to_string()),
        None => None,
    };
    let hold_max: f64 = arg("--hold").map(|h| h.parse()).transpose()?.unwrap_or(30.0);

    let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
    let shared = Arc::new(Shared {
        snap: snap_rx,
        nodes: Mutex::new(HashMap::new()),
        nonce: Default::default(),
        token,
        hold_max,
        load_error: Mutex::new(None),
    });

    // スナップショットの監視（内容のハッシュが変わったら新しい版）
    {
        let shared = shared.clone();
        tokio::spawn(async move {
            let mut last_raw: Option<Vec<u8>> = None;
            loop {
                match load(&snapshot) {
                    Ok(Some((raw, snap))) => {
                        *shared.load_error.lock().unwrap() = None;
                        if last_raw.as_ref() != Some(&raw) {
                            last_raw = Some(raw);
                            if snap.version != snap_tx.borrow().version {
                                println!("[vds] snapshot version {} ({} resources)", snap.version, snap.resources.len());
                                let _ = snap_tx.send(snap);
                            }
                        }
                    }
                    Ok(None) => {}
                    Err(e) => {
                        let mut le = shared.load_error.lock().unwrap();
                        if le.as_deref() != Some(e.as_str()) {
                            eprintln!("[vds] snapshot parse error: {}", e);
                        }
                        *le = Some(e);
                    }
                }
                tokio::time::sleep(Duration::from_millis(500)).await;
            }
        });
    }

    let rest = axum::Router::new()
        .route("/v1/discovery", post(discovery))
        .route("/v1/discovery/", post(discovery))
        .route("/v1/status", get(status))
        .route("/healthz", get(|| async { Json(json!({"ok": true})) }))
        .with_state(shared.clone());
    let routes = tonic::service::Routes::new(VeilDiscoveryServiceServer::new(Grpc(shared)))
        .into_axum_router()
        .merge(rest);

    let mut server = tonic::transport::Server::builder().accept_http1(true);
    if let (Some(cert), Some(key)) = (arg("--tls-cert"), arg("--tls-key")) {
        let identity = tonic::transport::Identity::from_pem(std::fs::read(cert)?, std::fs::read(key)?);
        server = server.tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))?;
    }
    println!("[vds] listening on {} (gRPC + REST)", listen);
    server
        .add_routes(tonic::service::Routes::from(routes))
        .serve_with_shutdown(listen, async {
            // コンテナの停止（SIGTERM）と Ctrl-C（SIGINT）
            let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("SIGTERM handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
        })
        .await?;
    Ok(())
}
