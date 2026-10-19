//! ロードバランシング（Upstream Group）
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// ロードバランシング（Upstream Group）
// ====================
//
// 複数のバックエンドサーバーへのリクエスト分散をサポートします。
//
// ## サポートするアルゴリズム
// - RoundRobin: 順番に振り分け（デフォルト）
// - LeastConnections: 接続数が最も少ないサーバーを選択
// - IpHash: クライアントIPに基づいて一貫したサーバーを選択
//
// ## 設定例
// ```toml
// [upstreams."backend-pool"]
// algorithm = "round_robin"
// servers = ["http://localhost:8080", "http://localhost:8081"]
//
// [[route]]
// conditions = { host = "example.com", path = "/api/*" }
// type = "Proxy"
// upstream = "backend-pool"
// ```
// ====================

/// Consistent Hash 用のハッシュキー
///
/// シリアライズ形式:
/// - `"ip"`              -> クライアント IP（デフォルト）
/// - `"header:X-User-Id"` -> 指定ヘッダーの値
/// - `"cookie:session_id"` -> 指定 Cookie の値
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub enum HashKey {
    /// クライアント IP アドレス
    #[default]
    Ip,
    /// 指定したリクエストヘッダーの値
    Header(String),
    /// 指定した Cookie の値
    Cookie(String),
}

/// Cookie ヘッダ値から指定名の値を取り出す（アロケーションなし）。
/// `name=value` を `;` 区切りで走査し、名前一致時に value スライスを返す。
pub(super) fn extract_cookie_value<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
    for part in cookie_header.split(';') {
        let part = part.trim();
        if part.is_empty() {
            continue;
        }
        if let Some((k, v)) = part.split_once('=') {
            if k.trim().eq_ignore_ascii_case(name) {
                return Some(v.trim());
            }
        }
    }
    None
}

impl HashKey {
    /// 文字列からパース（`"ip"`, `"header:X-Foo"`, `"cookie:session"`）
    pub fn parse(s: &str) -> Result<Self, String> {
        let s = s.trim();
        if s.eq_ignore_ascii_case("ip") {
            return Ok(HashKey::Ip);
        }
        if let Some(rest) = s.strip_prefix("header:") {
            return Ok(HashKey::Header(rest.trim().to_string()));
        }
        if let Some(rest) = s.strip_prefix("cookie:") {
            return Ok(HashKey::Cookie(rest.trim().to_string()));
        }
        Err(format!(
            "invalid hash_key: '{}', expected 'ip', 'header:NAME', or 'cookie:NAME'",
            s
        ))
    }
}

impl<'de> serde::Deserialize<'de> for HashKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        HashKey::parse(&s).map_err(serde::de::Error::custom)
    }
}

/// ロードバランシングアルゴリズム
#[derive(Clone, Debug, PartialEq, Default)]
pub enum LoadBalanceAlgorithm {
    /// ラウンドロビン（順番に振り分け）
    #[default]
    RoundRobin,
    /// Least Connections（接続数が最も少ないサーバー）
    LeastConnections,
    /// IP Hash（クライアントIPに基づく一貫したルーティング）
    IpHash,
    /// Weighted Round Robin（重み付きラウンドロビン）
    Weighted,
    /// Consistent Hash（仮想ノードリングによる一貫したルーティング）
    ConsistentHash {
        /// ハッシュキーの種類
        hash_key: HashKey,
    },
}

impl<'de> serde::Deserialize<'de> for LoadBalanceAlgorithm {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "round_robin" | "roundrobin" => Ok(LoadBalanceAlgorithm::RoundRobin),
            "least_conn" | "least_connections" | "leastconn" => Ok(LoadBalanceAlgorithm::LeastConnections),
            "ip_hash" | "iphash" => Ok(LoadBalanceAlgorithm::IpHash),
            "weighted" | "weighted_round_robin" | "wrr" => Ok(LoadBalanceAlgorithm::Weighted),
            // hash_key は UpstreamConfig の別フィールドで上書きされる（デフォルト Ip）
            "consistent_hash" | "consistenthash" => Ok(LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Ip,
            }),
            other => Err(serde::de::Error::custom(format!(
                "unknown load balance algorithm: '{}', expected 'round_robin', 'least_conn', 'ip_hash', 'weighted', or 'consistent_hash'",
                other
            ))),
        }
    }
}

/// algorithm と hash_key 設定から実際に使うアルゴリズムを決定する。
///
/// algorithm が ConsistentHash の場合、UpstreamConfig 側の hash_key
/// フィールドがあればそれで上書きする。
pub fn resolve_algorithm(
    algorithm: &LoadBalanceAlgorithm,
    hash_key: &Option<HashKey>,
) -> LoadBalanceAlgorithm {
    match algorithm {
        LoadBalanceAlgorithm::ConsistentHash { hash_key: inner } => {
            let key = hash_key.clone().unwrap_or_else(|| inner.clone());
            LoadBalanceAlgorithm::ConsistentHash { hash_key: key }
        }
        other => other.clone(),
    }
}

/// Upstream サーバーの状態
#[derive(Clone)]
pub struct UpstreamServer {
    /// バックエンドターゲット
    pub target: ProxyTarget,
    /// 現在のアクティブ接続数（Least Connections用）
    pub active_connections: Arc<AtomicUsize>,
    /// サーバーが利用可能かどうか（ヘルスチェック用）
    pub healthy: Arc<AtomicBool>,
    /// 連続成功回数（健康チェック用）
    pub consecutive_successes: Arc<AtomicUsize>,
    /// 連続失敗回数（健康チェック用）
    pub consecutive_failures: Arc<AtomicUsize>,
    /// サーキットブレーカー（F-06、有効時のみ Some）
    pub circuit_breaker: Option<crate::resilience::CircuitBreaker>,
    /// 異常検知用エラー率ウィンドウ（F-06）
    pub error_rate_window: Arc<std::sync::Mutex<crate::resilience::SlidingWindow>>,
    /// EWMA レイテンシ（ミリ秒、F-06）
    pub avg_latency_ms: Arc<AtomicU64>,
    /// 排除期限（F-06、Some の間は select 対象外）
    pub ejected_until: Arc<std::sync::Mutex<Option<std::time::Instant>>>,
}

impl UpstreamServer {
    pub fn new(target: ProxyTarget) -> Self {
        Self {
            target,
            active_connections: Arc::new(AtomicUsize::new(0)),
            healthy: Arc::new(AtomicBool::new(true)),
            consecutive_successes: Arc::new(AtomicUsize::new(0)),
            consecutive_failures: Arc::new(AtomicUsize::new(0)),
            circuit_breaker: None,
            error_rate_window: Arc::new(std::sync::Mutex::new(
                crate::resilience::SlidingWindow::new(std::time::Duration::from_secs(10)),
            )),
            avg_latency_ms: Arc::new(AtomicU64::new(0)),
            ejected_until: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// サーキットブレーカーを設定したコピーを返す
    pub fn with_circuit_breaker(mut self, cb: Option<crate::resilience::CircuitBreaker>) -> Self {
        self.circuit_breaker = cb;
        self
    }

    /// このサーバーが排除中かどうか（排除期限を過ぎていれば自動復帰）
    pub fn is_ejected(&self) -> bool {
        let mut guard = match self.ejected_until.lock() {
            Ok(g) => g,
            Err(p) => p.into_inner(),
        };
        match *guard {
            Some(until) => {
                if std::time::Instant::now() >= until {
                    *guard = None;
                    false
                } else {
                    true
                }
            }
            None => false,
        }
    }

    /// 指定時間だけサーバーを排除する
    pub fn eject_for(&self, dur: std::time::Duration) {
        if let Ok(mut guard) = self.ejected_until.lock() {
            *guard = Some(std::time::Instant::now() + dur);
        }
    }

    /// リクエスト結果を記録し、必要なら排除判定を行う（パッシブ Outlier Detection）
    ///
    /// `outlier` が None または無効の場合は EWMA レイテンシのみ更新する。
    pub fn record_outcome(&self, success: bool, latency_ms: u64, outlier: Option<&OutlierConfig>) {
        // EWMA レイテンシ更新（係数 0.2）
        let prev = self.avg_latency_ms.load(Ordering::Relaxed);
        let next = if prev == 0 {
            latency_ms
        } else {
            ((prev as f64) * 0.8 + (latency_ms as f64) * 0.2) as u64
        };
        self.avg_latency_ms.store(next, Ordering::Relaxed);

        // サーキットブレーカーへ反映
        if let Some(cb) = &self.circuit_breaker {
            if success {
                cb.record_success();
            } else {
                cb.record_failure();
            }
        }

        // Outlier Detection
        if let Some(cfg) = outlier {
            if !cfg.enabled {
                return;
            }
            let (total, errors) = {
                let mut w = match self.error_rate_window.lock() {
                    Ok(g) => g,
                    Err(p) => p.into_inner(),
                };
                w.record(success);
                (w.total(), w.failures())
            };
            // 一定サンプル数を超えたらエラー率を評価
            if total >= 5 {
                let rate = errors as f64 / total as f64;
                if rate >= cfg.error_rate_threshold {
                    self.eject_for(std::time::Duration::from_secs(cfg.base_ejection_time_secs));
                }
            }
        }
    }

    /// 接続カウンターを増加
    pub fn acquire(&self) {
        self.active_connections.fetch_add(1, Ordering::Relaxed);
    }

    /// 接続カウンターを減少
    pub fn release(&self) {
        self.active_connections.fetch_sub(1, Ordering::Relaxed);
    }

    /// 現在の接続数を取得
    pub fn connections(&self) -> usize {
        self.active_connections.load(Ordering::Relaxed)
    }

    /// サーバーが健全かどうか
    pub fn is_healthy(&self) -> bool {
        self.healthy.load(Ordering::Relaxed)
    }

    /// 健康チェック成功を記録
    pub fn record_success(&self, healthy_threshold: u32) {
        self.consecutive_failures.store(0, Ordering::Relaxed);
        let successes = self.consecutive_successes.fetch_add(1, Ordering::Relaxed) + 1;

        // 閾値に達したら healthy に設定
        if successes >= healthy_threshold as usize && !self.is_healthy() {
            self.healthy.store(true, Ordering::SeqCst);
            info!(
                "Upstream {}:{} is now healthy",
                self.target.host, self.target.port
            );
        }
    }

    /// 健康チェック失敗を記録
    pub fn record_failure(&self, unhealthy_threshold: u32) {
        self.consecutive_successes.store(0, Ordering::Relaxed);
        let failures = self.consecutive_failures.fetch_add(1, Ordering::Relaxed) + 1;

        // 閾値に達したら unhealthy に設定
        if failures >= unhealthy_threshold as usize && self.is_healthy() {
            self.healthy.store(false, Ordering::SeqCst);
            warn!(
                "Upstream {}:{} is now unhealthy",
                self.target.host, self.target.port
            );
        }
    }

    /// Get the host address
    pub fn host(&self) -> &str {
        &self.target.host
    }

    /// Get the port number
    pub fn port(&self) -> u16 {
        self.target.port
    }

    /// Check if TLS is enabled
    pub fn use_tls(&self) -> bool {
        self.target.use_tls
    }
}

/// Consistent Hash の仮想ノード数（サーバーあたり）
pub(super) const CONSISTENT_HASH_VNODES: usize = 150;
/// Consistent Hash 用のシード（固定）
pub(super) const CONSISTENT_HASH_SEED: u64 = 0x9E3779B97F4A7C15;

/// Upstream グループ（複数バックエンドのロードバランシング）
#[derive(Clone)]
pub struct UpstreamGroup {
    /// グループ名（ログ出力用）
    pub name: String,
    /// バックエンドサーバーリスト
    pub servers: Vec<UpstreamServer>,
    /// ロードバランシングアルゴリズム
    pub algorithm: LoadBalanceAlgorithm,
    /// ラウンドロビン用カウンター
    pub rr_counter: Arc<AtomicUsize>,
    /// 健康チェック設定（オプション）
    pub health_check: Option<HealthCheckConfig>,
    /// 上流の TLS の種類（検証する・しない・上流の mTLS の設定。F-217）
    pub tls_profile: crate::upstream_tls::TlsProfile,
    /// H2C (HTTP/2 over cleartext) を強制するかどうか
    pub use_h2c: bool,
    /// Weighted Round Robin 用の累積重みオフセット（構築時計算、O(1) 選択用）
    /// servers と同じ順序。`weighted_offsets[i]` は `servers[0..=i]` の重みの累積和。
    pub weighted_offsets: Vec<u32>,
    /// 全サーバーの重み合計（0 の場合は Weighted を使わない）
    pub total_weight: u32,
    /// Consistent Hash の仮想ノードリング（(hash, server_idx) を hash 昇順でソート）
    pub consistent_ring: Vec<(u64, usize)>,
    /// 異常検知設定（select 時に排除中サーバーを除外するために保持）
    pub outlier_detection: OutlierConfig,
}

impl UpstreamGroup {
    /// 新しい Upstream グループを作成
    pub fn new(
        name: String,
        entries: Vec<UpstreamServerEntry>,
        algorithm: LoadBalanceAlgorithm,
        health_check: Option<HealthCheckConfig>,
        tls_insecure: bool,
    ) -> Option<Self> {
        // 有効なエントリのみを (entry, server) として残す
        let pairs: Vec<(UpstreamServerEntry, UpstreamServer)> = entries
            .iter()
            .filter_map(|entry| {
                ProxyTarget::parse(&entry.url)
                    .map(|target| target.with_sni_name(entry.sni_name.clone()))
                    .map(|target| target.with_h2c(entry.use_h2c))
                    .map(|target| target.with_http2(entry.http2.unwrap_or_default()))
                    .map(|target| (entry.clone(), UpstreamServer::new(target)))
            })
            .collect();

        if pairs.is_empty() {
            return None;
        }

        let servers: Vec<UpstreamServer> = pairs.iter().map(|(_, s)| s.clone()).collect();

        // Weighted Round Robin 用の累積オフセットを構築
        let mut weighted_offsets = Vec::with_capacity(pairs.len());
        let mut acc: u32 = 0;
        for (entry, _) in &pairs {
            acc = acc.saturating_add(entry.weight.max(1));
            weighted_offsets.push(acc);
        }
        let total_weight = acc;

        // Consistent Hash 用の仮想ノードリングを構築
        let consistent_ring = Self::build_ring(&pairs);

        Some(Self {
            name,
            servers,
            algorithm,
            rr_counter: Arc::new(AtomicUsize::new(0)),
            health_check,
            tls_profile: crate::upstream_tls::TlsProfile::from_insecure(tls_insecure),
            use_h2c: false, // デフォルトでは各サーバーの設定に従う
            weighted_offsets,
            total_weight,
            consistent_ring,
            outlier_detection: OutlierConfig::default(),
        })
    }

    /// 仮想ノードリングを構築する（サーバーごとに CONSISTENT_HASH_VNODES 個の vnode）
    fn build_ring(pairs: &[(UpstreamServerEntry, UpstreamServer)]) -> Vec<(u64, usize)> {
        use xxhash_rust::xxh3::xxh3_64_with_seed;
        let mut ring: Vec<(u64, usize)> = Vec::with_capacity(pairs.len() * CONSISTENT_HASH_VNODES);
        for (idx, (_, server)) in pairs.iter().enumerate() {
            // サーバー識別子（接続先表記。TCP は host:port、UDS は unix:<path>、F-170）を
            // 基に vnode を生成。TCP では conn_addr() は host:port と 1 バイトも変わらない。
            let id = server.target.conn_addr().as_str().to_string();
            for vnode in 0..CONSISTENT_HASH_VNODES {
                let key = format!("{}#{}", id, vnode);
                let h = xxh3_64_with_seed(key.as_bytes(), CONSISTENT_HASH_SEED);
                ring.push((h, idx));
            }
        }
        ring.sort_by_key(|(h, _)| *h);
        ring
    }

    /// サーキットブレーカー・異常検知を適用したグループを返す（設定読み込み時に使用）
    pub fn with_resilience(
        mut self,
        cb_config: &CircuitBreakerConfig,
        outlier: &OutlierConfig,
    ) -> Self {
        if cb_config.enabled {
            for server in &mut self.servers {
                server.circuit_breaker =
                    Some(crate::resilience::CircuitBreaker::new(cb_config.clone()));
            }
        }
        // error_rate_window の長さを interval に合わせて再構築
        if outlier.enabled {
            for server in &mut self.servers {
                server.error_rate_window = Arc::new(std::sync::Mutex::new(
                    crate::resilience::SlidingWindow::new(std::time::Duration::from_secs(
                        outlier.interval_secs,
                    )),
                ));
            }
        }
        self.outlier_detection = outlier.clone();
        self
    }

    /// 単一サーバーからグループを作成
    pub fn single(target: ProxyTarget) -> Self {
        let server = UpstreamServer::new(target);
        Self {
            name: String::new(),
            servers: vec![server],
            algorithm: LoadBalanceAlgorithm::RoundRobin,
            rr_counter: Arc::new(AtomicUsize::new(0)),
            health_check: None, // 単一サーバーでは健康チェックなし
            // 単一サーバーではデフォルトで証明書検証を有効
            tls_profile: crate::upstream_tls::TlsProfile::Verify,
            use_h2c: false,
            weighted_offsets: vec![1],
            total_weight: 1,
            consistent_ring: Vec::new(),
            outlier_detection: OutlierConfig::default(),
        }
    }

    /// H2C設定を変更したコピーを作成
    pub fn with_h2c(&self, use_h2c: bool) -> Self {
        let mut new_group = self.clone();
        new_group.use_h2c = use_h2c;
        new_group
    }

    /// 選択候補となるサーバーを抽出（healthy かつ排除されていないもの）
    ///
    /// サーキットブレーカーが Open のサーバーも除外する。
    fn candidates(&self) -> Vec<(usize, &UpstreamServer)> {
        let avail: Vec<(usize, &UpstreamServer)> = self
            .servers
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_healthy() && !s.is_ejected())
            .filter(|(_, s)| match &s.circuit_breaker {
                Some(cb) => cb.allow_request(),
                None => true,
            })
            .collect();
        if !avail.is_empty() {
            return avail;
        }
        // 全て利用不可なら healthy なものへフォールバック（全滅回避）
        self.servers
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_healthy())
            .collect()
    }

    /// 次のバックエンドサーバーを選択
    ///
    /// # Arguments
    /// * `client_ip` - クライアントIPアドレス（IpHash / ConsistentHash 用）
    ///
    /// # Returns
    /// 選択されたサーバーへの参照（健全なサーバーがない場合は None）
    pub fn select(&self, client_ip: &str) -> Option<&UpstreamServer> {
        self.select_with_key(client_ip, None, None)
    }

    /// リクエストヘッダから Consistent Hash キーを解決してサーバーを選択する。
    ///
    /// - `ConsistentHash { Header }` / `Cookie` のときだけ `get_header` を呼ぶ
    ///   （それ以外のアルゴリズムではヘッダ走査ゼロ — ホットパス最適化）。
    /// - 値が取れない場合は `client_ip` にフォールバックする。
    ///
    /// `get_header` はヘッダ名（小文字比較用バイト列）を受け取り、値のバイト列を返す。
    /// Cookie 名は `HashKey::Cookie` の名前で `cookie` ヘッダをパースする。
    pub fn select_with_header_fn<'a, F>(
        &'a self,
        client_ip: &str,
        mut get_header: F,
    ) -> Option<&'a UpstreamServer>
    where
        F: FnMut(&[u8]) -> Option<&'a [u8]>,
    {
        match &self.algorithm {
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Header(name),
            } => {
                let val = get_header(name.as_bytes()).and_then(|v| std::str::from_utf8(v).ok());
                self.select_with_key(client_ip, val, None)
            }
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Cookie(name),
            } => {
                let cookie_hdr = get_header(b"cookie").and_then(|v| std::str::from_utf8(v).ok());
                let val = cookie_hdr.and_then(|c| extract_cookie_value(c, name));
                self.select_with_key(client_ip, val, None)
            }
            _ => self.select(client_ip),
        }
    }

    /// ハッシュキーの値を指定してサーバーを選択する
    ///
    /// ConsistentHash で `header:` / `cookie:` を使う場合は、呼び出し側で
    /// 該当ヘッダー/Cookie の値を解決して `hash_value` に渡す。値が解決
    /// できない場合は client_ip にフォールバックする。
    pub fn select_with_key(
        &self,
        client_ip: &str,
        hash_value: Option<&str>,
        _unused: Option<()>,
    ) -> Option<&UpstreamServer> {
        let candidates = self.candidates();
        if candidates.is_empty() {
            return None;
        }

        let selected_idx = match &self.algorithm {
            LoadBalanceAlgorithm::RoundRobin => {
                let counter = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                counter % candidates.len()
            }
            LoadBalanceAlgorithm::LeastConnections => candidates
                .iter()
                .enumerate()
                .min_by_key(|(_, (_, s))| s.connections())
                .map(|(i, _)| i)
                .unwrap_or(0),
            LoadBalanceAlgorithm::IpHash => {
                let hash = Self::fnv1a(client_ip.as_bytes());
                (hash as usize) % candidates.len()
            }
            LoadBalanceAlgorithm::Weighted => {
                // 健全なサーバー集合に対する重み合計を計算し、
                // rr_counter を total で割った余りで二分探索する。
                return self.select_weighted(&candidates);
            }
            LoadBalanceAlgorithm::ConsistentHash { hash_key } => {
                // ハッシュ対象の値を決定
                let key: &str = match hash_key {
                    HashKey::Ip => client_ip,
                    HashKey::Header(_) | HashKey::Cookie(_) => hash_value.unwrap_or(client_ip),
                };
                return self.select_consistent(key, &candidates);
            }
        };

        candidates.get(selected_idx).map(|(_, s)| *s)
    }

    /// FNV-1a ハッシュ（IpHash 用）
    fn fnv1a(bytes: &[u8]) -> u64 {
        let mut hash: u64 = 14695981039346656037;
        for &byte in bytes {
            hash ^= byte as u64;
            hash = hash.wrapping_mul(1099511628211);
        }
        hash
    }

    /// Weighted Round Robin 選択（候補内の重みで按分）
    fn select_weighted<'a>(
        &'a self,
        candidates: &[(usize, &'a UpstreamServer)],
    ) -> Option<&'a UpstreamServer> {
        // 候補（healthy）の重みを weighted_offsets から逆算して累積を作る
        let mut cum: Vec<(u32, usize)> = Vec::with_capacity(candidates.len());
        let mut acc: u32 = 0;
        for (ci, (orig_idx, _)) in candidates.iter().enumerate() {
            let w = self.weight_of(*orig_idx);
            acc = acc.saturating_add(w);
            cum.push((acc, ci));
        }
        let total = acc;
        if total == 0 {
            return candidates.first().map(|(_, s)| *s);
        }
        let pos = (self.rr_counter.fetch_add(1, Ordering::Relaxed) as u32) % total;
        // pos < offset となる最初の要素を二分探索
        let idx = match cum.binary_search_by(|(off, _)| {
            if *off <= pos {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            }
        }) {
            Ok(i) => i,
            Err(i) => i,
        };
        let ci = cum.get(idx).map(|(_, ci)| *ci).unwrap_or(0);
        candidates.get(ci).map(|(_, s)| *s)
    }

    /// 元インデックスのサーバーの重みを取得
    fn weight_of(&self, orig_idx: usize) -> u32 {
        let prev = if orig_idx == 0 {
            0
        } else {
            self.weighted_offsets
                .get(orig_idx - 1)
                .copied()
                .unwrap_or(0)
        };
        let cur = self
            .weighted_offsets
            .get(orig_idx)
            .copied()
            .unwrap_or(prev + 1);
        cur.saturating_sub(prev).max(1)
    }

    /// Consistent Hash 選択（リング上の二分探索）
    fn select_consistent<'a>(
        &'a self,
        key: &str,
        candidates: &[(usize, &'a UpstreamServer)],
    ) -> Option<&'a UpstreamServer> {
        use xxhash_rust::xxh3::xxh3_64_with_seed;
        if self.consistent_ring.is_empty() {
            // リング未構築（単一サーバー等）の場合はハッシュで按分
            let h = xxh3_64_with_seed(key.as_bytes(), CONSISTENT_HASH_SEED);
            let idx = (h as usize) % candidates.len();
            return candidates.get(idx).map(|(_, s)| *s);
        }
        let h = xxh3_64_with_seed(key.as_bytes(), CONSISTENT_HASH_SEED);
        // h 以上の最初の vnode を探す（なければ先頭へラップ）
        let start = self.consistent_ring.partition_point(|(vh, _)| *vh < h);
        let ring_len = self.consistent_ring.len();
        // リングを start から一周し、候補に含まれる最初のサーバーを選ぶ
        for offset in 0..ring_len {
            let (_, server_idx) = self.consistent_ring[(start + offset) % ring_len];
            if let Some((_, s)) = candidates.iter().find(|(oi, _)| *oi == server_idx) {
                return Some(*s);
            }
        }
        candidates.first().map(|(_, s)| *s)
    }

    /// 指定インデックスのサーバーのリクエスト結果を記録（F-06）
    pub fn record_outcome(&self, server_idx: usize, success: bool, latency_ms: u64) {
        if let Some(server) = self.servers.get(server_idx) {
            server.record_outcome(success, latency_ms, Some(&self.outlier_detection));
        }
    }

    /// サーバー数を取得
    pub fn len(&self) -> usize {
        self.servers.len()
    }

    /// サーバーが空かどうか
    pub fn is_empty(&self) -> bool {
        self.servers.is_empty()
    }

    /// 上流の TLS の種類（接続の作り方とプールの鍵。F-217）
    pub fn tls_profile(&self) -> crate::upstream_tls::TlsProfile {
        self.tls_profile
    }

    /// 上流の mTLS の設定を使う（F-217。`tls_insecure` より優先）
    pub fn with_tls_client(mut self, c: &'static crate::upstream_tls::UpstreamTlsClient) -> Self {
        self.tls_profile = crate::upstream_tls::TlsProfile::Custom(c);
        self
    }

    /// H2C (HTTP/2 over cleartext) を強制するかどうかを取得
    pub fn use_h2c(&self) -> bool {
        self.use_h2c
    }
}
