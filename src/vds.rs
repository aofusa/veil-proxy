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
//! 通信は専用スレッドで行い、データプレーン（ワーカーのイベントループ）には一切触れない。

use crate::config::SHUTDOWN_FLAG;
use ftlog::{error, info, warn};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{Read, Write};
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
    /// 長輪詢の保留時間の上限（秒、制御プレーンへ伝える）。
    pub poll_timeout_secs: u64,
    pub retry_initial_ms: u64,
    pub retry_max_ms: u64,
    /// 応答本文の上限（バイト）。
    pub max_response_bytes: usize,
    /// リソース数の上限。
    pub max_resources: usize,
}

impl Default for VdsConfig {
    fn default() -> Self {
        VdsConfig {
            enabled: false,
            server: String::new(),
            node_id: String::new(),
            cluster: String::new(),
            token_file: String::new(),
            ca_cert: String::new(),
            client_cert: String::new(),
            client_key: String::new(),
            poll_timeout_secs: 30,
            retry_initial_ms: 500,
            retry_max_ms: 30_000,
            max_response_bytes: 16 * 1024 * 1024,
            max_resources: 10_000,
        }
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

// ===================== HTTP クライアント（専用スレッド・ブロッキング） =====================

struct Target {
    tls: bool,
    host: String,
    port: u16,
    base: String,
}

fn parse_server(url: &str) -> Result<Target, String> {
    let (tls, rest) = if let Some(r) = url.strip_prefix("https://") {
        (true, r)
    } else if let Some(r) = url.strip_prefix("http://") {
        (false, r)
    } else {
        return Err(format!(
            "vds.server must start with http:// or https:// ({})",
            url
        ));
    };
    let (authority, base) = match rest.find('/') {
        Some(i) => (&rest[..i], rest[i..].trim_end_matches('/').to_string()),
        None => (rest, String::new()),
    };
    let (host, port) = if let Some(stripped) = authority.strip_prefix('[') {
        // [v6]:port
        let end = stripped.find(']').ok_or("invalid IPv6 host")?;
        let host = stripped[..end].to_string();
        let port = stripped[end + 1..]
            .strip_prefix(':')
            .map(|p| p.parse::<u16>());
        (host, port)
    } else {
        match authority.rsplit_once(':') {
            Some((h, p)) => (h.to_string(), Some(p.parse::<u16>())),
            None => (authority.to_string(), None),
        }
    };
    let port = match port {
        Some(Ok(p)) => p,
        Some(Err(_)) => return Err(format!("invalid port in vds.server ({})", url)),
        None => {
            if tls {
                443
            } else {
                80
            }
        }
    };
    if host.is_empty() {
        return Err(format!("missing host in vds.server ({})", url));
    }
    Ok(Target {
        tls,
        host,
        port,
        base,
    })
}

trait Stream: Read + Write {}
impl<T: Read + Write> Stream for T {}

struct Client {
    target: Target,
    tls: Option<Arc<rustls::ClientConfig>>,
    io_timeout: Duration,
}

// 理由付き allow: VDS 専用スレッドでのみ実行するブロッキング I/O（データプレーン外）。
#[allow(clippy::disallowed_methods)]
fn read_file(path: &str) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

fn build_tls(cfg: &VdsConfig) -> Result<Arc<rustls::ClientConfig>, String> {
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, PrivateKeyDer};
    let mut roots = rustls::RootCertStore::empty();
    roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
    if !cfg.ca_cert.is_empty() {
        let pem = read_file(&cfg.ca_cert).map_err(|e| format!("vds.ca_cert: {}", e))?;
        for c in CertificateDer::pem_slice_iter(&pem) {
            let c = c.map_err(|e| format!("vds.ca_cert: {}", e))?;
            roots.add(c).map_err(|e| format!("vds.ca_cert: {}", e))?;
        }
    }
    let provider = Arc::new(crate::tls_provider::provider::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots);
    let config = if !cfg.client_cert.is_empty() && !cfg.client_key.is_empty() {
        let cert_pem =
            read_file(&cfg.client_cert).map_err(|e| format!("vds.client_cert: {}", e))?;
        let key_pem = read_file(&cfg.client_key).map_err(|e| format!("vds.client_key: {}", e))?;
        let certs: Vec<CertificateDer<'static>> = CertificateDer::pem_slice_iter(&cert_pem)
            .collect::<Result<_, _>>()
            .map_err(|e| format!("vds.client_cert: {}", e))?;
        let key = PrivateKeyDer::from_pem_slice(&key_pem)
            .map_err(|e| format!("vds.client_key: {}", e))?;
        builder
            .with_client_auth_cert(certs, key)
            .map_err(|e| format!("vds client certificate: {}", e))?
    } else {
        builder.with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// 応答（ステータス・本文）。
struct HttpResult {
    status: u16,
    body: Vec<u8>,
}

impl Client {
    // 理由付き allow: VDS 専用スレッドでのみ実行するブロッキングな接続・名前解決（データプレーン外）。
    #[allow(clippy::disallowed_methods)]
    fn connect(&self) -> Result<Box<dyn Stream>, String> {
        use std::net::ToSocketAddrs;
        let addrs: Vec<_> = (self.target.host.as_str(), self.target.port)
            .to_socket_addrs()
            .map_err(|e| format!("resolve {}: {}", self.target.host, e))?
            .collect();
        let mut last = String::from("no address");
        for a in addrs {
            match std::net::TcpStream::connect_timeout(&a, Duration::from_secs(5)) {
                Ok(s) => {
                    let _ = s.set_read_timeout(Some(self.io_timeout));
                    let _ = s.set_write_timeout(Some(Duration::from_secs(10)));
                    let _ = s.set_nodelay(true);
                    if let Some(cfg) = &self.tls {
                        let name =
                            rustls::pki_types::ServerName::try_from(self.target.host.clone())
                                .map_err(|e| e.to_string())?;
                        let conn = rustls::ClientConnection::new(cfg.clone(), name)
                            .map_err(|e| e.to_string())?;
                        return Ok(Box::new(rustls::StreamOwned::new(conn, s)));
                    }
                    return Ok(Box::new(s));
                }
                Err(e) => last = format!("connect {}: {}", a, e),
            }
        }
        Err(last)
    }

    fn post(
        &self,
        path: &str,
        body: &[u8],
        token: Option<&str>,
        limit: usize,
    ) -> Result<HttpResult, String> {
        let mut s = self.connect()?;
        let host = if self.target.host.contains(':') {
            format!("[{}]:{}", self.target.host, self.target.port)
        } else {
            format!("{}:{}", self.target.host, self.target.port)
        };
        let mut req = format!(
            "POST {}{} HTTP/1.1\r\nHost: {}\r\nUser-Agent: veil/{}\r\nContent-Type: application/json\r\nAccept: application/json\r\nContent-Length: {}\r\nConnection: close\r\n",
            self.target.base,
            path,
            host,
            env!("CARGO_PKG_VERSION"),
            body.len()
        );
        if let Some(t) = token {
            req.push_str("Authorization: Bearer ");
            req.push_str(t);
            req.push_str("\r\n");
        }
        req.push_str("\r\n");
        s.write_all(req.as_bytes())
            .map_err(|e| format!("write: {}", e))?;
        s.write_all(body).map_err(|e| format!("write: {}", e))?;
        s.flush().map_err(|e| format!("write: {}", e))?;
        let mut raw = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match s.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    raw.extend_from_slice(&buf[..n]);
                    if raw.len() > limit + 64 * 1024 {
                        return Err(format!("response exceeds max_response_bytes ({})", limit));
                    }
                }
                // TLS の close_notify 無しの切断は本文を読み終えていれば受け入れる
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof && !raw.is_empty() => break,
                Err(e) => return Err(format!("read: {}", e)),
            }
        }
        parse_http_response(&raw, limit)
    }
}

fn parse_http_response(raw: &[u8], limit: usize) -> Result<HttpResult, String> {
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or("malformed response (no header terminator)")?;
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| "non-UTF-8 response header")?;
    let mut lines = head.split("\r\n");
    let status: u16 = lines
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|s| s.parse().ok())
        .ok_or("malformed status line")?;
    let mut chunked = false;
    let mut length: Option<usize> = None;
    for l in lines {
        if let Some((n, v)) = l.split_once(':') {
            let (n, v) = (n.trim(), v.trim());
            if n.eq_ignore_ascii_case("transfer-encoding")
                && v.to_ascii_lowercase().contains("chunked")
            {
                chunked = true;
            } else if n.eq_ignore_ascii_case("content-length") {
                length = v.parse().ok();
            }
        }
    }
    let rest = &raw[head_end + 4..];
    let body = if chunked {
        crate::http_utils::decode_chunked_body(rest)
    } else if let Some(n) = length {
        rest.get(..n).ok_or("truncated response body")?.to_vec()
    } else {
        rest.to_vec()
    };
    if body.len() > limit {
        return Err(format!("response exceeds max_response_bytes ({})", limit));
    }
    Ok(HttpResult { status, body })
}

// ===================== 状態・ループ =====================

/// 現在の VDS の状態（ログ・管理用）。
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct VdsStatus {
    pub connected: bool,
    pub version_info: String,
    pub last_error: Option<String>,
    pub updates_acked: u64,
    pub updates_nacked: u64,
}

static STATUS: std::sync::Mutex<Option<VdsStatus>> = std::sync::Mutex::new(None);

/// 現在の状態（VDS 無効なら `None`）。
pub fn status() -> Option<VdsStatus> {
    STATUS.lock().unwrap().clone()
}

fn update_status(f: impl FnOnce(&mut VdsStatus)) {
    let mut g = STATUS.lock().unwrap();
    f(g.get_or_insert_with(VdsStatus::default));
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
    let target = match parse_server(&cfg.server) {
        Ok(t) => t,
        Err(e) => {
            error!("[VDS] {}", e);
            return;
        }
    };
    let tls = if target.tls {
        match build_tls(&cfg) {
            Ok(t) => Some(t),
            Err(e) => {
                error!("[VDS] TLS setup failed: {}", e);
                return;
            }
        }
    } else {
        None
    };
    let client = Client {
        target,
        tls,
        io_timeout: Duration::from_secs(cfg.poll_timeout_secs.max(1) + 15),
    };
    update_status(|_| {});
    let spawned = std::thread::Builder::new()
        .name("veil-vds".into())
        .spawn(move || run_loop(cfg, client, config_path));
    if let Err(e) = spawned {
        error!("[VDS] failed to spawn the VDS thread: {}", e);
    }
}

fn node_id(cfg: &VdsConfig) -> String {
    if !cfg.node_id.is_empty() {
        return cfg.node_id.clone();
    }
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "veil".into())
}

fn run_loop(cfg: VdsConfig, client: Client, config_path: PathBuf) {
    let node = node_id(&cfg);
    info!("[VDS] subscribing to {} as node '{}'", cfg.server, node);
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
            match read_file(&cfg.token_file) {
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
        match client.post(
            "/v1/discovery",
            &body,
            token.as_deref(),
            cfg.max_response_bytes,
        ) {
            Ok(r) if r.status == 304 => {
                backoff = cfg.retry_initial_ms.max(50);
                update_status(|s| s.connected = true);
            }
            Ok(r) if r.status == 200 => {
                backoff = cfg.retry_initial_ms.max(50);
                update_status(|s| s.connected = true);
                match handle_response(&r.body, &cfg, &config_path) {
                    Ok((v, n)) => {
                        info!("[VDS] applied snapshot version '{}'", v);
                        version_info = v;
                        nonce = n;
                        error_detail = None;
                        update_status(|s| {
                            s.version_info = version_info.clone();
                            s.last_error = None;
                            s.updates_acked += 1;
                        });
                    }
                    Err((n, e)) => {
                        // NACK: 前の version_info のまま error_detail を返す（前の設定は残る）
                        warn!(
                            "[VDS] rejected snapshot (keeping version '{}'): {}",
                            version_info, e
                        );
                        nonce = n;
                        error_detail = Some(e.clone());
                        update_status(|s| {
                            s.last_error = Some(e);
                            s.updates_nacked += 1;
                        });
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
                update_status(|s| {
                    s.connected = false;
                    s.last_error = Some(format!("HTTP {}", r.status));
                });
                sleep_backoff(&mut backoff, cfg.retry_max_ms);
            }
            Err(e) => {
                warn!("[VDS] request to {} failed: {}", cfg.server, e);
                update_status(|s| {
                    s.connected = false;
                    s.last_error = Some(e);
                });
                sleep_backoff(&mut backoff, cfg.retry_max_ms);
            }
        }
    }
    info!("[VDS] stopped");
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
    let nonce = resp.nonce.clone();
    if !resp.type_url.is_empty() && resp.type_url != TYPE_SNAPSHOT {
        return Err((nonce, format!("unexpected type_url '{}'", resp.type_url)));
    }
    let overlay =
        resources_to_overlay(&resp.resources, cfg.max_resources).map_err(|e| (nonce.clone(), e))?;
    let previous = crate::config_override::swap_vds_overlay(Some(Arc::new(overlay)));
    if let Err(e) = crate::config::reload_config(config_path) {
        // 失敗: オーバーレイを戻す（現在の設定は reload 失敗で変わっていない）
        crate::config_override::swap_vds_overlay(previous);
        return Err((nonce, e.to_string()));
    }
    Ok((resp.version_info, nonce))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_server_urls() {
        let t = parse_server("http://cp:18000").unwrap();
        assert!(!t.tls);
        assert_eq!(
            (t.host.as_str(), t.port, t.base.as_str()),
            ("cp", 18000, "")
        );
        let t = parse_server("https://cp.example/base/").unwrap();
        assert!(t.tls);
        assert_eq!((t.port, t.base.as_str()), (443, "/base"));
        let t = parse_server("http://[::1]:9").unwrap();
        assert_eq!((t.host.as_str(), t.port), ("::1", 9));
        assert!(parse_server("ftp://x").is_err());
        assert!(parse_server("http://x:bad").is_err());
    }

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

    #[test]
    fn http_response_parsing() {
        let r = parse_http_response(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nok", 10).unwrap();
        assert_eq!((r.status, r.body.as_slice()), (200, &b"ok"[..]));
        let r = parse_http_response(
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n2\r\nok\r\n0\r\n\r\n",
            10,
        )
        .unwrap();
        assert_eq!(r.body, b"ok");
        assert!(parse_http_response(
            b"HTTP/1.1 200 OK\r\nContent-Length: 20\r\n\r\n0123456789012345678",
            10
        )
        .is_err());
    }
}
