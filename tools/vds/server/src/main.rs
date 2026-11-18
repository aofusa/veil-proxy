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
//!
//! 認証（B-470）: `--token-file` は全ノードで共有するトークン（持つ者はどのノード ID も名乗れる）。`--node-tokens` は
//! ノードごとのトークン（1 行に `<ノード ID> <トークン>`）で、要求のノード ID はトークンのノードに限る。どちらも無ければ
//! ループバック以外では待ち受けない（`--allow-unauthenticated` で明示したときだけ）。

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
    /// ノードごとのトークン（トークン, ノード ID）。あれば `token` は使わない（B-470）
    node_tokens: Vec<(String, String)>,
    hold_max: f64,
    load_error: Mutex<Option<String>>,
}

/// 要求の資格情報で分かること。
enum Caller {
    Denied,
    /// 共有のトークン（またはトークン無し）: どのノード ID でもよい
    AnyNode,
    /// ノードごとのトークン: このノード ID だけ
    Node(String),
}

impl Caller {
    /// 要求のノード ID を名乗ってよいか（B-470: 共有のトークンではノード ID を確かめられない）。
    fn may_act_as(&self, node: &str) -> bool {
        match self {
            Caller::Denied => false,
            Caller::AnyNode => true,
            Caller::Node(n) => n == node,
        }
    }
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
        if nack.is_none() {
            // B-470: 前に ACK したノードが空の版で来たら、新しく起動したプロセス（前のプロセスの NACK を持ち越さず、
            // 今の版を送り直す。また NACK されれば同じく止める）。NACK した版を ACK したら、その NACK は消す
            if (version.is_empty() && !st.acked_version.is_empty())
                || st.rejected_version.as_deref() == Some(version)
            {
                st.rejected_version = None;
            }
        }
        match nack {
            Some(msg) => {
                st.last_nack = Some(json!({"nonce": nonce, "message": msg}));
                st.nacks += 1;
                st.rejected_version = Some(self.snap.borrow().version.clone());
                // B-456: ノード ID・NACK の文は要求の値。{:?} で制御文字を書き換え、偽の行を足させない
                println!("[vds] NACK from {:?} ({}): {:?}", clip(node), transport, clip(msg));
            }
            None => st.acked_version = version.to_string(),
        }
        st.rejected_version.clone()
    }

    fn authenticate(&self, auth: Option<&str>) -> Caller {
        if !self.node_tokens.is_empty() {
            let Some(got) = auth.and_then(|a| a.strip_prefix("Bearer ")) else {
                return Caller::Denied;
            };
            // 全部と比べる（一致した位置で時間を変えない）
            let mut found = None;
            for (t, node) in &self.node_tokens {
                if constant_eq(got.as_bytes(), t.as_bytes()) {
                    found = Some(node);
                }
            }
            return found.map_or(Caller::Denied, |n| Caller::Node(n.clone()));
        }
        match &self.token {
            None => Caller::AnyNode,
            Some(t) if auth.is_some_and(|a| constant_eq(a.as_bytes(), format!("Bearer {}", t).as_bytes())) => {
                Caller::AnyNode
            }
            Some(_) => Caller::Denied,
        }
    }
}

/// `--node-tokens` のファイル（1 行に `<ノード ID> <トークン>`、`#` からはコメント）を読む。
// 起動時に 1 回だけ読む（参照用の制御プレーン。veil のデータプレーンではない）
#[allow(clippy::disallowed_methods)]
fn load_node_tokens(path: &str) -> Result<Vec<(String, String)>, String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("{}: {}", path, e))?;
    let mut out: Vec<(String, String)> = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let mut it = line.split_whitespace();
        let (Some(node), Some(token), None) = (it.next(), it.next(), it.next()) else {
            return Err(format!("{}:{}: expected '<node-id> <token>'", path, i + 1));
        };
        if out.iter().any(|(t, n)| t == token || n == node) {
            return Err(format!("{}:{}: duplicate node id or token", path, i + 1));
        }
        out.push((token.to_string(), node.to_string()));
    }
    if out.is_empty() {
        return Err(format!("{}: no node tokens", path));
    }
    Ok(out)
}

/// ログに書く要求の値の長さを抑える（文字の境界で切る）。
fn clip(s: &str) -> &str {
    match s.char_indices().nth(512) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn constant_eq(a: &[u8], b: &[u8]) -> bool {
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// スナップショットのファイルを読んで版を作る（構文エラーは `Err`、前の版を保つ）。
// 0.5 秒ごとの監視のタスクから読む小さな設定ファイル（参照用の制御プレーン。veil のデータプレーンではない）
#[allow(clippy::disallowed_methods)]
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
        let caller = self.0.authenticate(auth);
        if matches!(caller, Caller::Denied) {
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
                            Some(Err(e)) => { println!("[vds] gRPC stream from {:?} failed: {}", clip(&node), e); break; }
                            None => break,
                        };
                        if let Some(n) = &r.node { node = n.id.clone(); }
                        // B-470: ノードごとのトークンは、そのノードとしてだけ要求・ACK・NACK できる
                        if !caller.may_act_as(&node) {
                            let _ = tx.send(Err(Status::permission_denied("the node id does not match the token"))).await;
                            break;
                        }
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
    let caller = sh.authenticate(auth);
    if matches!(caller, Caller::Denied) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
    let req: Value = match serde_json::from_slice(if body.is_empty() { b"{}" } else { &body }) {
        Ok(v) => v,
        Err(_) => return (StatusCode::BAD_REQUEST, Json(json!({"error": "invalid json"}))).into_response(),
    };
    let node = req["node"]["id"].as_str().unwrap_or("unknown").to_string();
    // B-470: ノードごとのトークンは、そのノードとしてだけ要求・ACK・NACK できる
    if !caller.may_act_as(&node) {
        return (StatusCode::FORBIDDEN, Json(json!({"error": "the node id does not match the token"}))).into_response();
    }
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

async fn status(AxState(sh): AxState<Arc<Shared>>, headers: HeaderMap) -> AxResponse {
    // B-456: ノードの一覧・NACK の文・スナップショットの解析エラー（該当の行を含む）を返すので、discovery と同じ
    // トークンを求める
    let auth = headers.get("authorization").and_then(|v| v.to_str().ok());
    if matches!(sh.authenticate(auth), Caller::Denied) {
        return (StatusCode::UNAUTHORIZED, Json(json!({"error": "unauthorized"}))).into_response();
    }
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
    .into_response()
}

// ===================== main =====================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args: Vec<String> = std::env::args().collect();
    let arg = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        println!("usage: veil-vds-server --snapshot FILE [--listen ADDR] [--token-file FILE | --node-tokens FILE] [--allow-unauthenticated] [--hold SECS] [--tls-cert PEM --tls-key PEM]");
        println!("  --token-file FILE    one bearer token shared by all nodes (a holder can act as any node id)");
        println!("  --node-tokens FILE   one '<node-id> <token>' per line; a token can only act as its node id");
        println!("  --allow-unauthenticated  listen on a non-loopback address without a token");
        return Ok(());
    }
    let snapshot = arg("--snapshot").ok_or("--snapshot is required")?;
    let listen: std::net::SocketAddr = arg("--listen").unwrap_or_else(|| "127.0.0.1:18000".into()).parse()?;
    // 起動時に 1 回だけ読む
    #[allow(clippy::disallowed_methods)]
    let token = match arg("--token-file") {
        Some(p) => Some(std::fs::read_to_string(p)?.trim().to_string()),
        None => None,
    };
    if token.as_deref() == Some("") {
        return Err("the token file is empty".into());
    }
    let node_tokens = match arg("--node-tokens") {
        Some(p) => load_node_tokens(&p)?,
        None => Vec::new(),
    };
    if token.is_some() && !node_tokens.is_empty() {
        return Err("use either --token-file or --node-tokens, not both".into());
    }
    // B-470: トークン無しの待ち受けは、明示しない限りループバックだけ（スナップショットには上流のアドレス・ヘッダの値が
    // 載り、ACK・NACK でノードの配信を止められる）
    if token.is_none()
        && node_tokens.is_empty()
        && !listen.ip().is_loopback()
        && !args.iter().any(|a| a == "--allow-unauthenticated")
    {
        return Err(format!(
            "refusing to listen on {} without --token-file or --node-tokens (pass --allow-unauthenticated to serve any client)",
            listen
        )
        .into());
    }
    let hold_max: f64 = arg("--hold").map(|h| h.parse()).transpose()?.unwrap_or(30.0);

    let (snap_tx, snap_rx) = watch::channel(Snapshot::default());
    let shared = Arc::new(Shared {
        snap: snap_rx,
        nodes: Mutex::new(HashMap::new()),
        nonce: Default::default(),
        token,
        node_tokens,
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
        // 起動時に 1 回だけ読む
        #[allow(clippy::disallowed_methods)]
        let identity = tonic::transport::Identity::from_pem(std::fs::read(cert)?, std::fs::read(key)?);
        server = server.tls_config(tonic::transport::ServerTlsConfig::new().identity(identity))?;
    }
    println!("[vds] listening on {} (gRPC + REST)", listen);
    server
        .add_routes(tonic::service::Routes::from(routes))
        .serve_with_shutdown(listen, shutdown_signal())
        .await?;
    Ok(())
}

/// コンテナの停止（SIGTERM）と Ctrl-C（SIGINT）
#[cfg(unix)]
async fn shutdown_signal() {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .expect("SIGTERM handler");
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {}
        _ = term.recv() => {}
    }
}

/// Windows は Ctrl-C（Ctrl-Break 等の他のコンソールイベントは使わない）
#[cfg(not(unix))]
async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shared(token: Option<&str>, node_tokens: Vec<(String, String)>) -> Shared {
        let (_tx, rx) = watch::channel(Snapshot { version: "v2".into(), resources: Vec::new() });
        Shared {
            snap: rx,
            nodes: Mutex::new(HashMap::new()),
            nonce: Default::default(),
            token: token.map(String::from),
            node_tokens,
            hold_max: 1.0,
            load_error: Mutex::new(None),
        }
    }

    // B-470: ノードごとのトークンはそのノード ID だけを名乗れる。共有のトークンはどれでも
    #[test]
    fn node_tokens_bind_the_node_id() {
        let sh = shared(None, vec![("t1".into(), "veil-1".into()), ("t2".into(), "veil-2".into())]);
        assert!(sh.authenticate(Some("Bearer t1")).may_act_as("veil-1"));
        assert!(!sh.authenticate(Some("Bearer t1")).may_act_as("veil-2"));
        assert!(matches!(sh.authenticate(Some("Bearer t3")), Caller::Denied));
        assert!(matches!(sh.authenticate(None), Caller::Denied));
        let sh = shared(Some("s"), Vec::new());
        assert!(sh.authenticate(Some("Bearer s")).may_act_as("veil-2"));
        assert!(matches!(sh.authenticate(Some("Bearer x")), Caller::Denied));
    }

    // B-470: 再起動したノード（前に ACK して空の版で来た）と、NACK した版を ACK したノードは、前の NACK を持ち越さない
    #[test]
    fn restarted_node_does_not_keep_an_old_nack() {
        let sh = shared(None, Vec::new());
        assert_eq!(sh.record("n", "v1", None, "", "rest"), None);
        assert_eq!(sh.record("n", "v1", Some("bad"), "1", "rest").as_deref(), Some("v2"));
        // NACK の後の要求は前の版のまま（送り直さない）
        assert_eq!(sh.record("n", "v1", None, "", "rest").as_deref(), Some("v2"));
        // 新しいプロセス
        assert_eq!(sh.record("n", "", None, "", "rest"), None);
        // まだ何も ACK していないノードの NACK は、空の版の要求でも消さない（送り直しを繰り返さない）
        assert_eq!(sh.record("m", "", Some("bad"), "2", "rest").as_deref(), Some("v2"));
        assert_eq!(sh.record("m", "", None, "", "rest").as_deref(), Some("v2"));
        assert_eq!(sh.record("m", "v2", None, "", "rest"), None);
    }

    // 試験の一時ファイルを書く
    #[allow(clippy::disallowed_methods)]
    #[test]
    fn node_token_file_format() {
        let dir = std::env::temp_dir().join(format!("vds-node-tokens-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("tokens");
        std::fs::write(&p, "# nodes\nveil-1 t1\n\nveil-2 t2 # second\n").unwrap();
        let t = load_node_tokens(p.to_str().unwrap()).unwrap();
        assert_eq!(t, vec![("t1".to_string(), "veil-1".to_string()), ("t2".into(), "veil-2".into())]);
        std::fs::write(&p, "veil-1 t1\nveil-2 t1\n").unwrap();
        assert!(load_node_tokens(p.to_str().unwrap()).is_err());
        std::fs::write(&p, "veil-1\n").unwrap();
        assert!(load_node_tokens(p.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
