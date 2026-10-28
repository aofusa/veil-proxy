//! Prometheus のメトリクスの設定
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// Prometheusメトリクス設定セクション
// ====================

/// Prometheusメトリクス設定
///
/// メトリクスエンドポイントの有効化、パス変更、アクセス制限を設定します。
///
/// 例:
/// ```toml
/// [prometheus]
/// enabled = true
/// path = "/metrics"
/// allowed_ips = ["127.0.0.1", "10.0.0.0/8"]
/// ```
#[derive(Deserialize, Clone, Debug)]
pub struct PrometheusConfig {
    /// メトリクスエンドポイントを有効化するかどうか
    /// デフォルト: true
    #[serde(default = "default_prometheus_enabled")]
    pub enabled: bool,

    /// メトリクスエンドポイントのパス
    /// デフォルト: "/__metrics"
    #[serde(default = "default_prometheus_path")]
    pub path: String,

    /// メトリクスエンドポイントへのアクセスを許可するIPアドレス/CIDR
    /// 空の場合は loopback（と UDS のリスナー）だけ（B-231。全許可は `["0.0.0.0/0", "::/0"]`）
    /// 例: ["127.0.0.1", "10.0.0.0/8", "192.168.0.0/16"]
    #[serde(default)]
    pub allowed_ips: Vec<String>,
}

pub(super) fn default_prometheus_enabled() -> bool {
    false
}
pub(super) fn default_prometheus_path() -> String {
    "/__metrics".to_string()
}

/// 管理 API 設定（F-20）
///
/// キャッシュ Purge などの管理操作を受け付けるエンドポイント。
/// `secret` は Authorization ヘッダー（`Bearer <secret>` または生の値）で検証する。
///
/// 例:
/// ```toml
/// [admin]
/// enabled = true
/// path_prefix = "/__admin"
/// secret = "changeme"
/// ```
#[cfg(feature = "admin")]
#[derive(Deserialize, Clone, Debug)]
pub struct AdminConfig {
    /// 管理 API を有効化するかどうか
    #[serde(default)]
    pub enabled: bool,
    /// 管理エンドポイントのパスプレフィックス
    #[serde(default = "default_admin_path_prefix")]
    pub path_prefix: String,
    /// 認証用シークレット（空の場合は全リクエストを拒否）
    #[serde(default)]
    pub secret: String,
    /// アクセスを許可するIPアドレス/CIDR（空の場合は loopback だけ。B-231）
    /// 例: ["127.0.0.1", "10.0.0.0/8", "192.168.0.0/16"]
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    /// キャッシュパージプレフィックス（事前計算、リクエスト毎の format! を回避）
    /// デシリアライズ時に自動計算される: "{path_prefix}/cache/purge"
    #[serde(skip)]
    pub cache_purge_prefix: String,
}

/// B-231: 管理 API の認証の失敗を数えるレート表のゾーン（ルートのゾーン＝`SecurityConfig` のアドレスと重ならない値）。
#[cfg(feature = "admin")]
const ADMIN_AUTH_ZONE: usize = usize::MAX;
/// B-231: IP ごとの管理 API の認証の失敗の上限（1 分あたり）。
#[cfg(feature = "admin")]
pub const ADMIN_AUTH_FAILURES_PER_MINUTE: u64 = 10;

/// 長さが違っても途中で抜けずに比べる（時間は `expected` の長さだけで決まる）。
#[cfg(feature = "admin")]
fn constant_time_eq(given: &[u8], expected: &[u8]) -> bool {
    let mut diff = (given.len() != expected.len()) as u8;
    for (i, e) in expected.iter().enumerate() {
        diff |= given.get(i).copied().unwrap_or(0) ^ e;
    }
    std::hint::black_box(diff) == 0
}

/// loopback のアドレスか（IPv4 射影の IPv6 も含む。UDS のリスナーの接続は 127.0.0.1 として届く）。
pub(super) fn is_loopback_ip(client_ip: &str) -> bool {
    match client_ip.parse::<std::net::IpAddr>() {
        Ok(std::net::IpAddr::V4(v4)) => v4.is_loopback(),
        Ok(std::net::IpAddr::V6(v6)) => {
            v6.is_loopback() || v6.to_ipv4_mapped().is_some_and(|v4| v4.is_loopback())
        }
        Err(_) => false,
    }
}

#[cfg(feature = "admin")]
pub(super) fn default_admin_path_prefix() -> String {
    "/__admin".to_string()
}

/// OpenTelemetry (OTLP/HTTP) 設定（F-10）
///
/// 例:
/// ```toml
/// [opentelemetry]
/// enabled = false
/// endpoint = "http://localhost:4318"
/// service_name = "veil-proxy"
/// batch_interval_secs = 30
/// ```
///
/// `opentelemetry` feature が無効でもパースは可能（無視される）。
#[derive(Deserialize, Clone, Debug)]
pub struct OpenTelemetryConfig {
    /// 有効化フラグ
    #[serde(default)]
    pub enabled: bool,
    /// OTLP/HTTP エンドポイント
    #[serde(default = "default_otel_endpoint")]
    pub endpoint: String,
    /// サービス名
    #[serde(default = "default_otel_service_name")]
    pub service_name: String,
    /// バッチ送信間隔（秒）
    #[serde(default = "default_otel_batch_interval")]
    pub batch_interval_secs: u64,
}

pub(super) fn default_otel_endpoint() -> String {
    "http://localhost:4318".to_string()
}
pub(super) fn default_otel_service_name() -> String {
    "veil-proxy".to_string()
}
pub(super) fn default_otel_batch_interval() -> u64 {
    30
}

impl Default for OpenTelemetryConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            endpoint: default_otel_endpoint(),
            service_name: default_otel_service_name(),
            batch_interval_secs: default_otel_batch_interval(),
        }
    }
}

#[cfg(feature = "admin")]
impl Default for AdminConfig {
    fn default() -> Self {
        let path_prefix = default_admin_path_prefix();
        let cache_purge_prefix = format!("{}/cache/purge", path_prefix);
        Self {
            enabled: false,
            path_prefix,
            secret: String::new(),
            allowed_ips: Vec::new(),
            cache_purge_prefix,
        }
    }
}

#[cfg(feature = "admin")]
impl AdminConfig {
    /// デシリアライズ後に事前計算フィールドを補完する
    pub fn compute_derived(&mut self) {
        self.cache_purge_prefix = format!("{}/cache/purge", self.path_prefix);
    }
}

#[cfg(feature = "admin")]
impl AdminConfig {
    /// Authorization ヘッダー値がシークレットと一致するか検証する。
    ///
    /// `Bearer <secret>` 形式と生の `<secret>` 形式の両方を許容する。
    /// secret が空の場合は常に false（無効化）。
    pub fn check_auth(&self, auth_header: Option<&str>) -> bool {
        if !self.enabled || self.secret.is_empty() {
            return false;
        }
        match auth_header {
            Some(h) => {
                let h = h.trim();
                let token = h.strip_prefix("Bearer ").unwrap_or(h);
                // B-231: 定数時間で比べる（以前の `==` は一致した先頭の長さでかかる時間が変わり、1 バイトずつ当てられた）
                constant_time_eq(token.as_bytes(), self.secret.as_bytes())
            }
            None => false,
        }
    }

    /// `check_auth` に、IP ごとの失敗の回数の上限（1 分あたり `ADMIN_AUTH_FAILURES_PER_MINUTE`）を加えたもの。
    /// 上限を超えた IP は正しい秘密でも拒否する（総当たりを遅くする。B-231）。
    pub fn check_auth_from(&self, client_ip: &str, auth_header: Option<&str>) -> bool {
        if super::runtime_state::rate_peek(ADMIN_AUTH_ZONE, client_ip)
            >= ADMIN_AUTH_FAILURES_PER_MINUTE
        {
            return false;
        }
        if self.check_auth(auth_header) {
            return true;
        }
        super::runtime_state::check_rate_limit(
            ADMIN_AUTH_ZONE,
            client_ip,
            ADMIN_AUTH_FAILURES_PER_MINUTE,
        );
        false
    }

    /// クライアント IP が管理 API エンドポイントへのアクセスを許可されているか確認する。
    ///
    /// `allowed_ips` が空の場合は loopback（UDS のリスナーの接続を含む）だけを許可する（B-231。以前は全 IP）。
    pub fn is_ip_allowed(&self, client_ip: &str) -> bool {
        if self.allowed_ips.is_empty() {
            return is_loopback_ip(client_ip);
        }
        let client_addr: std::net::IpAddr = match client_ip.parse() {
            Ok(addr) => addr,
            Err(_) => return false,
        };
        for allowed in &self.allowed_ips {
            if allowed.contains('/') {
                if let Some((network, prefix_len)) = allowed.split_once('/') {
                    if let (Ok(network_addr), Ok(prefix)) = (
                        network.parse::<std::net::IpAddr>(),
                        prefix_len.parse::<u8>(),
                    ) {
                        if PrometheusConfig::ip_in_cidr(&client_addr, &network_addr, prefix) {
                            return true;
                        }
                    }
                }
            } else if let Ok(allowed_addr) = allowed.parse::<std::net::IpAddr>() {
                if client_addr == allowed_addr {
                    return true;
                }
            }
        }
        false
    }
}

impl Default for PrometheusConfig {
    fn default() -> Self {
        Self {
            enabled: default_prometheus_enabled(),
            path: default_prometheus_path(),
            allowed_ips: Vec::new(),
        }
    }
}

impl PrometheusConfig {
    /// IPアドレスがメトリクスエンドポイントへのアクセスを許可されているか確認
    pub fn is_ip_allowed(&self, client_ip: &str) -> bool {
        // B-231: allowed_ips が空なら loopback（UDS のリスナーの接続を含む）だけ。以前は全 IP を許していた
        if self.allowed_ips.is_empty() {
            return is_loopback_ip(client_ip);
        }

        // クライアントIPをパース
        let client_addr: std::net::IpAddr = match client_ip.parse() {
            Ok(addr) => addr,
            Err(_) => return false,
        };

        for allowed in &self.allowed_ips {
            // CIDR表記かチェック
            if allowed.contains('/') {
                // CIDR表記の場合
                if let Some((network, prefix_len)) = allowed.split_once('/') {
                    if let (Ok(network_addr), Ok(prefix)) = (
                        network.parse::<std::net::IpAddr>(),
                        prefix_len.parse::<u8>(),
                    ) {
                        if Self::ip_in_cidr(&client_addr, &network_addr, prefix) {
                            return true;
                        }
                    }
                }
            } else {
                // 単一IPアドレスの場合
                if let Ok(allowed_addr) = allowed.parse::<std::net::IpAddr>() {
                    if client_addr == allowed_addr {
                        return true;
                    }
                }
            }
        }

        false
    }

    /// IPアドレスがCIDRブロック内にあるかチェック
    fn ip_in_cidr(ip: &std::net::IpAddr, network: &std::net::IpAddr, prefix_len: u8) -> bool {
        match (ip, network) {
            (std::net::IpAddr::V4(ip), std::net::IpAddr::V4(net)) => {
                if prefix_len > 32 {
                    return false;
                }
                let mask = if prefix_len == 0 {
                    0
                } else {
                    !0u32 << (32 - prefix_len)
                };
                let ip_bits = u32::from_be_bytes(ip.octets());
                let net_bits = u32::from_be_bytes(net.octets());
                (ip_bits & mask) == (net_bits & mask)
            }
            (std::net::IpAddr::V6(ip), std::net::IpAddr::V6(net)) => {
                if prefix_len > 128 {
                    return false;
                }
                let ip_bits = u128::from_be_bytes(ip.octets());
                let net_bits = u128::from_be_bytes(net.octets());
                let mask = if prefix_len == 0 {
                    0
                } else {
                    !0u128 << (128 - prefix_len)
                };
                (ip_bits & mask) == (net_bits & mask)
            }
            _ => false, // IPv4とIPv6の混在は不一致
        }
    }
}

// 権限降格（get_uid_by_name, get_gid_by_name, drop_privileges）と
// build_sandbox_config は crate::system モジュールに移動しました。
