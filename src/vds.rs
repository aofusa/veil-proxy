//! VDS（Veil Discovery Service）: 制御プレーンからのルート・アップストリームの動的配信（F-04）。
//!
//! Envoy の xDS の REST-JSON / State-of-the-World と同じ意味論の最小サブセット:
//!
//! - クライアント（veil）は `POST {server}/v1/discovery` に `DiscoveryRequest` を送る
//!   （`version_info`・`node`・`type_url`・`response_nonce`・`error_detail`）。
//! - 制御プレーンは `version_info` が手元と同じなら変化があるまで保留し（長輪詢）、
//!   タイムアウトしたら `304 Not Modified`。変化があれば `200` で `DiscoveryResponse`
//!   （`version_info`・`nonce`・`resources[]`）を返す。
//! - 適用に成功したら次の要求で新しい `version_info` を送る（ACK）。失敗したら前の
//!   `version_info` と `error_detail` を送る（NACK）。前の設定はそのまま残る。
//!
//! 受け取ったリソース（`type.veil.dev/veil.v1.Route` = `[[route]]`、`type.veil.dev/veil.v1.Upstream` =
//! `[upstreams.<name>]`）は 1 個のスナップショットとして「VDS オーバーレイ」に置き、設定ロードの単一
//! チョークポイント（`config_override::apply_to_toml_str`）で `route` 配列・`upstreams` 表を置き換える。
//! 既存のリロード経路（検証 → ArcSwap）をそのまま使うので、SIGHUP リロードでも同じオーバーレイが当たる。
//! 優先順位: config.toml < VDS < 環境変数 `VEIL_OVERRIDE_<n>` < CLI `--override`。
//! listen・TLS・L4 は対象外（再起動が必要なため）。
//!
//! 転送は 2 種類（`[vds] transport`）:
//!
//! - `"grpc"`（既定、F-199）: `veil.vds.v1.VeilDiscoveryService/StreamSnapshots` の双方向ストリーム
//!   （`proto/veil/vds/v1/vds.proto`、`src/vds_grpc.rs`）。サーバが版を押し出すので長輪詢の待ちが無い。
//! - `"rest"`: 上の `POST /v1/discovery` の長輪詢。
//!
//! 適用・ACK/NACK・状態は両者で共通（`apply_snapshot`・`record_applied`・`record_rejected`）。
//!
//! 通信は専用スレッドで行い、データプレーン（ワーカーのイベントループ）には一切触れない。

use crate::config::SHUTDOWN_FLAG;
use ftlog::{error, info, warn};
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// スナップショット全体の型（xDS の ADS 相当で、ルートとアップストリームを 1 回で配信する）。
pub const TYPE_SNAPSHOT: &str = "type.veil.dev/veil.v1.Snapshot";
/// `[[route]]` 1 件。
pub const TYPE_ROUTE: &str = "type.veil.dev/veil.v1.Route";
/// `[upstreams.<name>]` 1 件（`name` キーが表の名前）。
pub const TYPE_UPSTREAM: &str = "type.veil.dev/veil.v1.Upstream";

/// `[vds]` セクション。
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct VdsConfig {
    pub enabled: bool,
    /// 制御プレーンの URL（`http://host:port[/base]` / `https://...`）。
    pub server: String,
    /// 制御プレーンを複数（冗長化、F-211）。指定すると `server` より優先し、つながらない・ストリームが
    /// 失敗したら次を試す（全部を 1 周したらバックオフ）。
    pub servers: Vec<String>,
    /// ノード ID（空ならホスト名）。
    pub node_id: String,
    pub cluster: String,
    /// Bearer トークンを読むファイル（要求ごとに読み直す。空なら送らない）。
    pub token_file: String,
    /// https の追加の信頼 CA（PEM、空なら webpki-roots のみ）。
    pub ca_cert: String,
    /// mTLS のクライアント証明書・鍵（PEM、両方指定したときだけ使う）。
    pub client_cert: String,
    pub client_key: String,
    /// 転送（`"grpc"` = gRPC の双方向ストリーム、`"rest"` = REST の長輪詢）。
    pub transport: String,
    /// gRPC の接続の TCP キープアライブ（秒、0 で無効）。応答が来ない長寿命のストリームで死活を見る。
    pub keepalive_secs: u64,
    /// 長輪詢の保留時間の上限（秒、制御プレーンへ伝える。REST）。
    pub poll_timeout_secs: u64,
    pub retry_initial_ms: u64,
    pub retry_max_ms: u64,
    /// 応答本文の上限（バイト）。
    pub max_response_bytes: usize,
    /// リソース数の上限。
    pub max_resources: usize,
    /// 起動時に最初のスナップショットを適用するまでリスナーを開かずに待つ上限（ミリ秒、0 = 待たない。F-211）。
    /// 期限を過ぎたらファイルの設定で開始する（Envoy の `initial_fetch_timeout` 相当）。
    pub initial_fetch_timeout_ms: u64,
}

impl Default for VdsConfig {
    fn default() -> Self {
        VdsConfig {
            enabled: false,
            server: String::new(),
            servers: Vec::new(),
            node_id: String::new(),
            cluster: String::new(),
            token_file: String::new(),
            ca_cert: String::new(),
            client_cert: String::new(),
            client_key: String::new(),
            transport: "grpc".into(),
            keepalive_secs: 30,
            poll_timeout_secs: 30,
            retry_initial_ms: 500,
            retry_max_ms: 30_000,
            max_response_bytes: 16 * 1024 * 1024,
            max_resources: 10_000,
            initial_fetch_timeout_ms: 0,
        }
    }
}

impl VdsConfig {
    /// 試す制御プレーンの一覧（`servers`、無ければ `server`）。
    pub fn server_list(&self) -> Vec<String> {
        if !self.servers.is_empty() {
            return self.servers.clone();
        }
        if self.server.is_empty() {
            return Vec::new();
        }
        vec![self.server.clone()]
    }
}

/// 制御プレーンの一覧を順に試す位置（失敗したら次へ。1 周したらバックオフする）。
pub(crate) struct ServerRotation {
    len: usize,
    idx: usize,
    fails: usize,
}

impl ServerRotation {
    pub(crate) fn new(len: usize) -> Self {
        ServerRotation {
            len: len.max(1),
            idx: 0,
            fails: 0,
        }
    }

    pub(crate) fn current(&self) -> usize {
        self.idx
    }

    /// 応答を受け取れた（1 周の数え直し）。
    pub(crate) fn succeeded(&mut self) {
        self.fails = 0;
    }

    /// 今の制御プレーンで失敗した。次へ進め、1 周したら `true`（バックオフしてから試す）。
    pub(crate) fn failed(&mut self) -> bool {
        self.idx = (self.idx + 1) % self.len;
        self.fails += 1;
        if self.fails >= self.len {
            self.fails = 0;
            return true;
        }
        false
    }
}

#[derive(Deserialize, Default)]
struct VdsRoot {
    #[serde(default)]
    vds: VdsConfig,
}

/// 設定ファイル（上書き込み）から `[vds]` を読む。
pub fn load_vds_config(path: &std::path::Path) -> std::io::Result<VdsConfig> {
    let text = crate::config::read_config_text(path)?;
    let root: VdsRoot = crate::config_override::apply_to_toml_str(&text)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    Ok(root.vds)
}

// ===================== オーバーレイ =====================

/// 受け取ったスナップショットを、設定の `route` 配列・`upstreams` 表へ当てる値に変換する。
///
/// 戻り値は `{ route = [...], upstreams = { name = {...} } }`（スナップショットに含まれる種類だけ）。
pub fn resources_to_overlay(resources: &[Value], max: usize) -> Result<toml::Value, String> {
    if resources.len() > max {
        return Err(format!(
            "too many resources: {} (max_resources = {})",
            resources.len(),
            max
        ));
    }
    let mut routes = Vec::new();
    let mut upstreams = toml::Table::new();
    let mut has_routes = false;
    let mut has_upstreams = false;
    for (i, r) in resources.iter().enumerate() {
        let obj = r
            .as_object()
            .ok_or_else(|| format!("resource[{}] must be an object", i))?;
        let ty = obj.get("@type").and_then(|t| t.as_str()).unwrap_or("");
        let mut body = obj.clone();
        body.remove("@type");
        match ty {
            TYPE_ROUTE => {
                has_routes = true;
                body.remove("name");
                routes.push(
                    json_to_toml(Value::Object(body))
                        .map_err(|e| format!("resource[{}]: {}", i, e))?,
                );
            }
            TYPE_UPSTREAM => {
                has_upstreams = true;
                let name = body
                    .remove("name")
                    .and_then(|n| n.as_str().map(|s| s.to_string()))
                    .ok_or_else(|| format!("resource[{}]: upstream requires a 'name'", i))?;
                upstreams.insert(
                    name,
                    json_to_toml(Value::Object(body))
                        .map_err(|e| format!("resource[{}]: {}", i, e))?,
                );
            }
            other => return Err(format!("resource[{}]: unknown @type '{}'", i, other)),
        }
    }
    let mut overlay = toml::Table::new();
    // 空のスナップショット（resources = []）は「ルート・アップストリームとも空」を意味する（SotW）。
    if has_routes || resources.is_empty() {
        overlay.insert("route".into(), toml::Value::Array(routes));
    }
    if has_upstreams || resources.is_empty() {
        overlay.insert("upstreams".into(), toml::Value::Table(upstreams));
    }
    Ok(toml::Value::Table(overlay))
}

/// JSON 値を TOML 値に変換する（`null` は取り除く。TOML に null は無い）。
fn json_to_toml(v: Value) -> Result<toml::Value, String> {
    Ok(match v {
        Value::Null => return Err("null is not allowed".into()),
        Value::Bool(b) => toml::Value::Boolean(b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml::Value::Integer(i)
            } else if let Some(f) = n.as_f64() {
                toml::Value::Float(f)
            } else {
                return Err(format!("number out of range: {}", n));
            }
        }
        Value::String(s) => toml::Value::String(s),
        Value::Array(a) => toml::Value::Array(
            a.into_iter()
                .filter(|x| !x.is_null())
                .map(json_to_toml)
                .collect::<Result<_, _>>()?,
        ),
        Value::Object(o) => {
            let mut t = toml::Table::new();
            for (k, v) in o {
                if v.is_null() {
                    continue;
                }
                t.insert(k, json_to_toml(v)?);
            }
            toml::Value::Table(t)
        }
    })
}

/// オーバーレイを設定の値木へ当てる（`config_override::apply_to_toml_str` から呼ばれる）。
pub fn apply_overlay(root: &mut toml::Value, overlay: &toml::Value) {
    let (Some(root), Some(ov)) = (root.as_table_mut(), overlay.as_table()) else {
        return;
    };
    for key in ["route", "upstreams"] {
        if let Some(v) = ov.get(key) {
            root.insert(key.into(), v.clone());
        }
    }
}

// ===================== 状態・ループ =====================

/// 現在の VDS の状態（ログ・管理用）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct VdsStatus {
    pub connected: bool,
    /// 今つないでいる（最後に試した）制御プレーン。
    pub server: String,
    pub version_info: String,
    pub last_error: Option<String>,
    pub updates_acked: u64,
    pub updates_nacked: u64,
    /// 最後に適用・拒否した時刻（UNIX 秒）。
    pub last_update_unix: Option<u64>,
}

static STATUS: std::sync::Mutex<Option<VdsStatus>> = std::sync::Mutex::new(None);

/// 現在の状態（VDS 無効なら `None`）。
pub fn status() -> Option<VdsStatus> {
    STATUS.lock().unwrap().clone()
}

/// 管理エンドポイント（`GET {path_prefix}/vds`）の JSON。VDS が無効なら `{"enabled":false}`。
pub fn status_json() -> String {
    match status() {
        Some(s) => {
            let mut v = serde_json::to_value(&s).unwrap_or_else(|_| json!({}));
            v["enabled"] = json!(true);
            v.to_string()
        }
        None => "{\"enabled\":false}".into(),
    }
}

fn update_status(f: impl FnOnce(&mut VdsStatus)) {
    let mut g = STATUS.lock().unwrap();
    f(g.get_or_insert_with(VdsStatus::default));
}

fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// 最初のスナップショットを適用したか（`initial_fetch_timeout_ms` の待ち）。
static INITIAL: (std::sync::Mutex<bool>, std::sync::Condvar) =
    (std::sync::Mutex::new(false), std::sync::Condvar::new());

/// 最初のスナップショットの適用を `timeout` まで待つ（起動時だけ。適用したら `true`）。
fn wait_initial(timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    let mut done = INITIAL.0.lock().unwrap();
    while !*done && !SHUTDOWN_FLAG.load(Ordering::Relaxed) {
        let now = std::time::Instant::now();
        if now >= deadline {
            break;
        }
        // 停止を見逃さないよう短い間隔で起きる
        let step = (deadline - now).min(Duration::from_millis(200));
        done = INITIAL.1.wait_timeout(done, step).unwrap().0;
    }
    *done
}

/// `[vds] enabled = true` なら専用スレッドで制御プレーンへの購読を始める。
pub fn spawn_if_enabled(config_path: PathBuf) {
    let cfg = match load_vds_config(&config_path) {
        Ok(c) => c,
        Err(e) => {
            error!("[VDS] failed to read [vds]: {}", e);
            return;
        }
    };
    if !cfg.enabled {
        return;
    }
    let servers = cfg.server_list();
    if servers.is_empty() {
        error!("[VDS] vds.server (or vds.servers) is required");
        return;
    }
    for s in &servers {
        if let Err(e) = crate::ctrl_http::parse_url(s) {
            error!("[VDS] vds.server '{}': {}", s, e);
            return;
        }
    }
    let wait = Duration::from_millis(cfg.initial_fetch_timeout_ms);
    if cfg.transport == "grpc" {
        start_status(&servers[0]);
        let spawned = std::thread::Builder::new()
            .name("veil-vds".into())
            .spawn(move || crate::vds_grpc::run(cfg, config_path));
        if let Err(e) = spawned {
            error!("[VDS] failed to spawn the VDS thread: {}", e);
            return;
        }
        wait_initial_snapshot(wait);
        return;
    }
    if cfg.transport != "rest" {
        error!(
            "[VDS] vds.transport must be \"grpc\" or \"rest\" (got \"{}\")",
            cfg.transport
        );
        return;
    }
    let tls = crate::ctrl_http::TlsOptions {
        ca_cert: Some(cfg.ca_cert.clone()),
        client_cert: Some(cfg.client_cert.clone()),
        client_key: Some(cfg.client_key.clone()),
    };
    let client = match crate::ctrl_http::HttpClient::new(
        &tls,
        Duration::from_secs(cfg.poll_timeout_secs.max(1) + 15),
    ) {
        Ok(c) => c,
        Err(e) => {
            error!("[VDS] TLS setup failed: {}", e);
            return;
        }
    };
    start_status(&servers[0]);
    let spawned = std::thread::Builder::new()
        .name("veil-vds".into())
        .spawn(move || run_loop(cfg, client, config_path));
    if let Err(e) = spawned {
        error!("[VDS] failed to spawn the VDS thread: {}", e);
        return;
    }
    wait_initial_snapshot(wait);
}

fn start_status(server: &str) {
    update_status(|s| s.server = server.to_string());
    #[cfg(feature = "metrics")]
    crate::metrics::vds_set_connected(false);
}

/// `initial_fetch_timeout_ms` > 0 なら、最初のスナップショットを適用するまで起動を止める（リスナーはまだ開いていない）。
fn wait_initial_snapshot(wait: Duration) {
    if wait.is_zero() {
        return;
    }
    info!(
        "[VDS] waiting up to {} ms for the initial snapshot before opening listeners",
        wait.as_millis()
    );
    if wait_initial(wait) {
        info!("[VDS] initial snapshot applied; starting");
    } else {
        warn!(
            "[VDS] no snapshot within initial_fetch_timeout_ms ({} ms); starting with the file configuration",
            wait.as_millis()
        );
    }
}

pub(crate) fn node_id(cfg: &VdsConfig) -> String {
    if !cfg.node_id.is_empty() {
        return cfg.node_id.clone();
    }
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "veil".into())
}

fn run_loop(cfg: VdsConfig, client: crate::ctrl_http::HttpClient, config_path: PathBuf) {
    let node = node_id(&cfg);
    let servers = cfg.server_list();
    let mut rot = ServerRotation::new(servers.len());
    info!(
        "[VDS] subscribing to {} as node '{}'",
        servers.join(", "),
        node
    );
    let mut version_info = String::new();
    let mut nonce = String::new();
    let mut error_detail: Option<String> = None;
    let mut backoff = cfg.retry_initial_ms.max(50);
    while !SHUTDOWN_FLAG.load(Ordering::Relaxed) {
        let mut req = json!({
            "version_info": version_info,
            "node": {
                "id": node,
                "cluster": cfg.cluster,
                "user_agent_name": "veil",
                "user_agent_version": env!("CARGO_PKG_VERSION"),
            },
            "type_url": TYPE_SNAPSHOT,
            "response_nonce": nonce,
            "poll_timeout_secs": cfg.poll_timeout_secs,
        });
        if let Some(msg) = &error_detail {
            req["error_detail"] = json!({ "code": 3, "message": msg });
        }
        let token = if cfg.token_file.is_empty() {
            None
        } else {
            match crate::ctrl_http::read_file(&cfg.token_file) {
                Ok(t) => Some(String::from_utf8_lossy(&t).trim().to_string()),
                Err(e) => {
                    warn!("[VDS] cannot read token_file {}: {}", cfg.token_file, e);
                    None
                }
            }
        };
        let body = serde_json::to_vec(&req).unwrap_or_default();
        // NACK は 1 回だけ伝える（制御プレーンは拒否された版を内容が変わるまで送り直さない）
        error_detail = None;
        let server = &servers[rot.current()];
        let url = format!("{}/v1/discovery", server.trim_end_matches('/'));
        let auth = token.as_ref().map(|t| format!("Bearer {}", t));
        let mut headers: Vec<(&str, &str)> = vec![
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
        ];
        if let Some(a) = &auth {
            headers.push(("Authorization", a.as_str()));
        }
        match client.request("POST", &url, &headers, &body, cfg.max_response_bytes) {
            Ok(r) if r.status == 304 => {
                backoff = cfg.retry_initial_ms.max(50);
                rot.succeeded();
                record_connection_to(server);
            }
            Ok(r) if r.status == 200 => {
                backoff = cfg.retry_initial_ms.max(50);
                rot.succeeded();
                record_connection_to(server);
                match handle_response(&r.body, &cfg, &config_path) {
                    Ok((v, n)) => {
                        record_applied(&v);
                        version_info = v;
                        nonce = n;
                        error_detail = None;
                    }
                    Err((n, e)) => {
                        // NACK: 前の version_info のまま error_detail を返す（前の設定は残る）
                        record_rejected(&version_info, &e);
                        nonce = n;
                        error_detail = Some(e);
                    }
                }
                // ACK/NACK はすぐ次の要求で伝える
                continue;
            }
            Ok(r) => {
                warn!(
                    "[VDS] control plane returned {}: {}",
                    r.status,
                    String::from_utf8_lossy(&r.body[..r.body.len().min(200)])
                );
                record_connection(false, Some(format!("HTTP {}", r.status)));
                // 制御プレーンが替わったら nonce は意味を持たない（版は SotW なのでそのまま伝える）
                nonce.clear();
                if rot.failed() {
                    sleep_backoff(&mut backoff, cfg.retry_max_ms);
                }
            }
            Err(e) => {
                warn!("[VDS] request to {} failed: {}", server, e);
                record_connection(false, Some(e));
                nonce.clear();
                if rot.failed() {
                    sleep_backoff(&mut backoff, cfg.retry_max_ms);
                }
            }
        }
    }
    info!("[VDS] stopped");
}

/// 適用に成功した版を記録する（ACK の前）。
pub(crate) fn record_applied(version_info: &str) {
    info!("[VDS] applied snapshot version '{}'", version_info);
    let now = unix_now();
    update_status(|s| {
        s.connected = true;
        s.version_info = version_info.to_string();
        s.last_error = None;
        s.updates_acked += 1;
        s.last_update_unix = Some(now);
    });
    #[cfg(feature = "metrics")]
    crate::metrics::vds_record_update(Some(version_info), now);
    let mut done = INITIAL.0.lock().unwrap();
    if !*done {
        *done = true;
        INITIAL.1.notify_all();
    }
}

/// 拒否した版を記録する（NACK の前）。
pub(crate) fn record_rejected(kept_version: &str, error: &str) {
    warn!(
        "[VDS] rejected snapshot (keeping version '{}'): {}",
        kept_version, error
    );
    let now = unix_now();
    update_status(|s| {
        s.last_error = Some(error.to_string());
        s.updates_nacked += 1;
        s.last_update_unix = Some(now);
    });
    #[cfg(feature = "metrics")]
    crate::metrics::vds_record_update(None, now);
}

/// 接続状態を記録する。
pub(crate) fn record_connection(connected: bool, error: Option<String>) {
    update_status(|s| {
        s.connected = connected;
        if error.is_some() {
            s.last_error = error;
        }
    });
    #[cfg(feature = "metrics")]
    crate::metrics::vds_set_connected(connected);
}

/// `server` から応答を受け取れた（接続中）。
pub(crate) fn record_connection_to(server: &str) {
    update_status(|s| {
        s.connected = true;
        if s.server != server {
            s.server = server.to_string();
        }
    });
    #[cfg(feature = "metrics")]
    crate::metrics::vds_set_connected(true);
}

fn sleep_backoff(backoff: &mut u64, max: u64) {
    let mut left = *backoff;
    while left > 0 && !SHUTDOWN_FLAG.load(Ordering::Relaxed) {
        let step = left.min(200);
        crate::server::cap_safe_sleep(Duration::from_millis(step));
        left -= step;
    }
    *backoff = (*backoff * 2).min(max.max(100));
}

#[derive(Deserialize)]
struct DiscoveryResponse {
    version_info: String,
    #[serde(default)]
    nonce: String,
    #[serde(default)]
    type_url: String,
    #[serde(default)]
    resources: Vec<Value>,
}

/// スナップショットを検証・適用する。成功は (version_info, nonce)、失敗は (nonce, 理由)。
fn handle_response(
    body: &[u8],
    cfg: &VdsConfig,
    config_path: &std::path::Path,
) -> Result<(String, String), (String, String)> {
    let resp: DiscoveryResponse = serde_json::from_slice(body)
        .map_err(|e| (String::new(), format!("invalid DiscoveryResponse: {}", e)))?;
    apply_snapshot(
        resp.version_info,
        resp.nonce,
        &resp.type_url,
        &resp.resources,
        cfg,
        config_path,
    )
}

/// スナップショットを検証・適用する（REST・gRPC 共通）。`resources` は REST 版の形
/// （`@type`・`name` と本体のキーを持つオブジェクト）。成功は (version_info, nonce)、失敗は (nonce, 理由)。
pub(crate) fn apply_snapshot(
    version_info: String,
    nonce: String,
    type_url: &str,
    resources: &[Value],
    cfg: &VdsConfig,
    config_path: &std::path::Path,
) -> Result<(String, String), (String, String)> {
    if !type_url.is_empty() && type_url != TYPE_SNAPSHOT {
        return Err((nonce, format!("unexpected type_url '{}'", type_url)));
    }
    let overlay =
        resources_to_overlay(resources, cfg.max_resources).map_err(|e| (nonce.clone(), e))?;
    let previous = crate::config_override::swap_vds_overlay(Some(Arc::new(overlay)));
    if let Err(e) = crate::config::reload_config(config_path) {
        // 失敗: オーバーレイを戻す（現在の設定は reload 失敗で変わっていない）
        crate::config_override::swap_vds_overlay(previous);
        return Err((nonce, e.to_string()));
    }
    Ok((version_info, nonce))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resources_become_overlay() {
        let res = vec![
            json!({"@type": TYPE_ROUTE, "name": "r1",
                   "conditions": {"path": "/a"}, "action": {"type": "Proxy", "upstream": "pool"}}),
            json!({"@type": TYPE_UPSTREAM, "name": "pool", "servers": ["127.0.0.1:1"], "x": null}),
        ];
        let ov = resources_to_overlay(&res, 10).unwrap();
        assert_eq!(ov["route"][0]["conditions"]["path"].as_str(), Some("/a"));
        assert!(ov["route"][0].get("name").is_none());
        assert_eq!(
            ov["upstreams"]["pool"]["servers"][0].as_str(),
            Some("127.0.0.1:1")
        );
        assert!(ov["upstreams"]["pool"].get("x").is_none());

        let mut root: toml::Value = toml::from_str(
            "[server]\nlisten = \"x\"\n[[route]]\naction = { type = \"File\", path = \"/\" }\n",
        )
        .unwrap();
        apply_overlay(&mut root, &ov);
        assert_eq!(root["route"].as_array().unwrap().len(), 1);
        assert_eq!(root["route"][0]["conditions"]["path"].as_str(), Some("/a"));
        assert_eq!(root["server"]["listen"].as_str(), Some("x"));
    }

    #[test]
    fn server_list_and_rotation() {
        let mut c = VdsConfig {
            server: "http://a".into(),
            ..Default::default()
        };
        assert_eq!(c.server_list(), vec!["http://a".to_string()]);
        c.servers = vec!["http://b".into(), "http://c".into()];
        assert_eq!(
            c.server_list(),
            vec!["http://b".to_string(), "http://c".to_string()]
        );
        c.servers.clear();
        c.server.clear();
        assert!(c.server_list().is_empty());

        // 失敗したら次へ。全部を 1 周したらバックオフ（true）
        let mut r = ServerRotation::new(3);
        assert_eq!(r.current(), 0);
        assert!(!r.failed());
        assert_eq!(r.current(), 1);
        r.succeeded();
        assert!(!r.failed());
        assert!(!r.failed());
        assert!(r.failed());
        assert_eq!(r.current(), 1);
        let mut one = ServerRotation::new(1);
        assert!(one.failed());
        assert_eq!(one.current(), 0);
    }

    #[test]
    fn overlay_limits_and_errors() {
        let res = vec![json!({"@type": TYPE_ROUTE}); 3];
        assert!(resources_to_overlay(&res, 2)
            .unwrap_err()
            .contains("too many"));
        assert!(resources_to_overlay(&[json!({"@type": "x"})], 5).is_err());
        assert!(resources_to_overlay(&[json!({"@type": TYPE_UPSTREAM})], 5).is_err());
        // 空のスナップショットは「全部空」
        let ov = resources_to_overlay(&[], 5).unwrap();
        assert_eq!(ov["route"].as_array().unwrap().len(), 0);
    }
}
