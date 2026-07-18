//! 設定モジュール
//!
//! データ型、構造体、列挙型、静的変数、設定読み込み関数を提供します。

use crate::constants::*;
use crate::logging::*;
use crate::pool::*;
use crate::runtime::io::AsyncWriteRentExt;
use crate::runtime::tcp::TcpStream;
use crate::runtime::time::timeout;
use arc_swap::ArcSwap;
use clap::Parser;
use ftlog::{info, warn};
use httparse::{Request, Status};
use once_cell::sync::Lazy;
use rustls::ServerConfig;
use rustls_pki_types::{pem::PemObject, CertificateDer, PrivateKeyDer};
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::fs::File;
use std::io;
use std::io::BufReader;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[cfg(veil_ktls)]
use crate::ktls_rustls::{KtlsClientStream, KtlsServerStream, RustlsConnector};

#[cfg(not(veil_ktls))]
use crate::simple_tls;

#[cfg(feature = "http2")]
use crate::protocol;

use crate::buffering;
use crate::cache;
#[cfg(feature = "http2")]
use crate::http2;
#[cfg(feature = "http3")]
use crate::http3_server;
use crate::routing;
// ====================
// kTLS設定情報
// ====================
//
// kTLSはrustls + ktls2経由でサポートされています。
// `cargo build --features ktls` でビルドしてください。
//

/// kTLS設定情報
#[derive(Clone, Debug, Default)]
pub struct KtlsConfig {
    /// kTLSを有効化するかどうか
    pub enabled: bool,
    /// TLS TX（送信）のkTLSを有効化
    pub enable_tx: bool,
    /// TLS RX（受信）のkTLSを有効化
    pub enable_rx: bool,
    /// kTLS有効化失敗時にrustlsへフォールバックするかどうか
    /// false: kTLS必須（失敗時は接続拒否）
    /// true: kTLS失敗時はrustlsで継続（デフォルト）
    pub fallback_enabled: bool,
    /// TCP_CORKを使用するかどうか
    /// kTLS設定中のパケット結合最適化を有効化
    pub tcp_cork_enabled: bool,
}
/// セキュリティ設定のデフォルト値関数
fn default_max_body_size() -> usize {
    MAX_BODY_SIZE
}
fn default_max_header_size() -> usize {
    MAX_HEADER_SIZE
}
fn default_client_header_timeout() -> u64 {
    30
}
fn default_client_body_timeout() -> u64 {
    30
}
fn default_backend_connect_timeout() -> u64 {
    10
}
fn default_max_idle_connections() -> usize {
    BACKEND_POOL_MAX_IDLE_PER_HOST
}
fn default_idle_connection_timeout() -> u64 {
    BACKEND_POOL_IDLE_TIMEOUT_SECS
}

// WebSocket ポーリング設定のデフォルト値
fn default_websocket_poll_timeout_ms() -> u64 {
    1
}
fn default_websocket_poll_max_timeout_ms() -> u64 {
    100
}
fn default_websocket_backoff_multiplier() -> f64 {
    2.0
}
// ====================
// IP制限機能（CIDR対応）
// ====================
//
// allowed_ips と denied_ips でルートごとのIP制限を設定できます。
// CIDR記法（例: "192.168.1.0/24"）と単一IP（例: "10.0.0.1"）の両方をサポート。
//
// 評価順序: deny → allow（denyが優先）
// - denied_ips にマッチ → 拒否
// - allowed_ips が空 → 許可
// - allowed_ips にマッチ → 許可
// - それ以外 → 拒否
// ====================

/// CIDR範囲を表す構造体
#[derive(Clone, Debug)]
pub struct CidrRange {
    /// ネットワークアドレス（IPv4は32ビット、IPv6は128ビット）
    pub network: u128,
    /// プレフィックス長
    pub prefix_len: u8,
    /// IPv6かどうか
    pub is_ipv6: bool,
}

impl CidrRange {
    /// CIDR文字列をパース（例: "192.168.1.0/24" または "10.0.0.1"）
    pub fn parse(s: &str) -> Option<Self> {
        let (ip_str, prefix_len) = if let Some(idx) = s.find('/') {
            let prefix: u8 = s[idx + 1..].parse().ok()?;
            (&s[..idx], prefix)
        } else {
            // プレフィックスなし = 単一IP
            (s, 255) // 255は後で適切な値に変換
        };

        // IPv4をパース
        if let Some(ipv4) = Self::parse_ipv4(ip_str) {
            let prefix = if prefix_len == 255 { 32 } else { prefix_len };
            if prefix > 32 {
                return None;
            }
            // IPv4を128ビットの上位に配置（IPv6-mapped形式ではなく単純に格納）
            let network = (ipv4 as u128) & Self::mask_v4(prefix);
            return Some(CidrRange {
                network,
                prefix_len: prefix,
                is_ipv6: false,
            });
        }

        // IPv6をパース
        if let Some(ipv6) = Self::parse_ipv6(ip_str) {
            let prefix = if prefix_len == 255 { 128 } else { prefix_len };
            if prefix > 128 {
                return None;
            }
            let network = ipv6 & Self::mask_v6(prefix);
            return Some(CidrRange {
                network,
                prefix_len: prefix,
                is_ipv6: true,
            });
        }

        None
    }

    /// IPv4アドレス文字列をパース
    fn parse_ipv4(s: &str) -> Option<u32> {
        let parts: Vec<&str> = s.split('.').collect();
        if parts.len() != 4 {
            return None;
        }

        let mut result: u32 = 0;
        for (i, part) in parts.iter().enumerate() {
            let octet: u8 = part.parse().ok()?;
            result |= (octet as u32) << (24 - i * 8);
        }
        Some(result)
    }

    /// IPv6アドレス文字列をパース（簡易実装）
    fn parse_ipv6(s: &str) -> Option<u128> {
        // :: の展開を処理
        let parts: Vec<&str> = s.split(':').collect();

        // :: がある場合の処理
        let has_double_colon = s.contains("::");
        if has_double_colon {
            let sides: Vec<&str> = s.split("::").collect();
            if sides.len() > 2 {
                return None;
            }

            let left_parts: Vec<&str> = if sides[0].is_empty() {
                vec![]
            } else {
                sides[0].split(':').collect()
            };

            let right_parts: Vec<&str> = if sides.len() < 2 || sides[1].is_empty() {
                vec![]
            } else {
                sides[1].split(':').collect()
            };

            let missing = 8 - left_parts.len() - right_parts.len();
            let mut all_parts: Vec<u16> = Vec::with_capacity(8);

            for part in &left_parts {
                all_parts.push(u16::from_str_radix(part, 16).ok()?);
            }
            all_parts.resize(all_parts.len() + missing, 0);
            for part in &right_parts {
                all_parts.push(u16::from_str_radix(part, 16).ok()?);
            }

            if all_parts.len() != 8 {
                return None;
            }

            let mut result: u128 = 0;
            for (i, &part) in all_parts.iter().enumerate() {
                result |= (part as u128) << (112 - i * 16);
            }
            return Some(result);
        }

        // :: がない場合
        if parts.len() != 8 {
            return None;
        }

        let mut result: u128 = 0;
        for (i, part) in parts.iter().enumerate() {
            let segment: u16 = u16::from_str_radix(part, 16).ok()?;
            result |= (segment as u128) << (112 - i * 16);
        }
        Some(result)
    }

    /// IPv4用のネットマスクを生成
    #[inline]
    fn mask_v4(prefix: u8) -> u128 {
        if prefix == 0 {
            0
        } else if prefix >= 32 {
            0xFFFF_FFFF
        } else {
            ((1u128 << prefix) - 1) << (32 - prefix)
        }
    }

    /// IPv6用のネットマスクを生成
    #[inline]
    fn mask_v6(prefix: u8) -> u128 {
        if prefix == 0 {
            0
        } else if prefix >= 128 {
            u128::MAX
        } else {
            ((1u128 << prefix) - 1) << (128 - prefix)
        }
    }

    /// IPアドレスがこのCIDR範囲に含まれるかチェック
    pub fn contains(&self, ip: &str) -> bool {
        if self.is_ipv6 {
            // IPv6
            if let Some(ipv6) = Self::parse_ipv6(ip) {
                let masked = ipv6 & Self::mask_v6(self.prefix_len);
                return masked == self.network;
            }
        } else {
            // IPv4
            if let Some(ipv4) = Self::parse_ipv4(ip) {
                let masked = (ipv4 as u128) & Self::mask_v4(self.prefix_len);
                return masked == self.network;
            }
        }
        false
    }

    /// `IpAddr` がこの CIDR 範囲に含まれるか（文字列を経由しないゼロアロケーション版）。
    ///
    /// accept ホットパスでの IP ブロックリスト判定に用いる（F-35）。`to_string()` を
    /// 介さないため接続ごとのヒープ確保が発生しない。
    #[inline]
    pub fn contains_addr(&self, ip: std::net::IpAddr) -> bool {
        match ip {
            std::net::IpAddr::V4(v4) => {
                if self.is_ipv6 {
                    return false;
                }
                let addr = u32::from(v4) as u128;
                (addr & Self::mask_v4(self.prefix_len)) == self.network
            }
            std::net::IpAddr::V6(v6) => {
                if !self.is_ipv6 {
                    return false;
                }
                let addr = u128::from(v6);
                (addr & Self::mask_v6(self.prefix_len)) == self.network
            }
        }
    }
}

/// IPフィルター（許可/拒否リスト）
#[derive(Clone, Debug, Default)]
pub struct IpFilter {
    /// 許可するIP/CIDR範囲（空 = すべて許可）
    pub allowed: Vec<CidrRange>,
    /// 拒否するIP/CIDR範囲
    pub denied: Vec<CidrRange>,
}

impl IpFilter {
    /// 文字列リストからIpFilterを構築
    pub fn from_lists(allowed_ips: &[String], denied_ips: &[String]) -> Self {
        let allowed: Vec<CidrRange> = allowed_ips
            .iter()
            .filter_map(|s| CidrRange::parse(s))
            .collect();

        let denied: Vec<CidrRange> = denied_ips
            .iter()
            .filter_map(|s| CidrRange::parse(s))
            .collect();

        Self { allowed, denied }
    }

    /// IPアドレスが許可されているかチェック
    /// 評価順序: deny → allow（denyが優先）
    pub fn is_allowed(&self, ip: &str) -> bool {
        // denyリストにマッチしたら拒否
        for cidr in &self.denied {
            if cidr.contains(ip) {
                return false;
            }
        }

        // allowリストが空なら許可
        if self.allowed.is_empty() {
            return true;
        }

        // allowリストにマッチしたら許可
        for cidr in &self.allowed {
            if cidr.contains(ip) {
                return true;
            }
        }

        // どちらにもマッチしない場合は拒否
        false
    }

    /// フィルターが設定されているか（空でないか）
    pub fn is_configured(&self) -> bool {
        !self.allowed.is_empty() || !self.denied.is_empty()
    }
}

// ====================
// WebSocket ポーリング設定
// ====================

/// WebSocketポーリングモード
///
/// - `Fixed`: 固定タイムアウト（低レイテンシ優先）
/// - `Adaptive`: バックオフ方式による動的調整（CPU効率優先）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum WebSocketPollMode {
    /// 固定タイムアウト - 常に同じタイムアウト値を使用
    /// 低レイテンシが最優先の場合（リアルタイムゲームなど）に推奨
    Fixed,
    /// バックオフ方式 - アクティブ時は短く、アイドル時は長くなる
    /// CPU効率とレイテンシのバランスを取る場合（チャットなど）に推奨
    #[default]
    Adaptive,
}

impl<'de> serde::Deserialize<'de> for WebSocketPollMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.to_lowercase().as_str() {
            "fixed" => Ok(WebSocketPollMode::Fixed),
            "adaptive" => Ok(WebSocketPollMode::Adaptive),
            other => Err(serde::de::Error::custom(format!(
                "unknown websocket_poll_mode: '{}', expected 'fixed' or 'adaptive'",
                other
            ))),
        }
    }
}

/// WebSocketポーリング設定
///
/// この設定は、WebSocket双方向転送時のポーリング動作を制御します。
///
/// ## モード
///
/// - **Fixed**: 常に `initial_timeout_ms` でポーリング
/// - **Adaptive**: データ転送時は `initial_timeout_ms` を使用し、
///   アイドル時は `max_timeout_ms` まで徐々に延長
///
/// ## 設定例
///
/// ```toml
/// # リアルタイムゲーム（低レイテンシ最優先）
/// websocket_poll_mode = "fixed"
/// websocket_poll_timeout_ms = 1
///
/// # チャットアプリ（バランス重視）
/// websocket_poll_mode = "adaptive"
/// websocket_poll_timeout_ms = 1
/// websocket_poll_max_timeout_ms = 50
/// ```
#[derive(Clone, Debug)]
pub struct WebSocketPollConfig {
    /// ポーリングモード
    pub mode: WebSocketPollMode,
    /// 初期タイムアウト（ミリ秒）
    /// Fixedモード: この値を固定で使用
    /// Adaptiveモード: この値から開始
    pub initial_timeout_ms: u64,
    /// 最大タイムアウト（ミリ秒）- Adaptiveモードでのみ使用
    /// タイムアウトはこの値を超えて延長されない
    pub max_timeout_ms: u64,
    /// バックオフ倍率 - Adaptiveモードでのみ使用
    /// タイムアウト発生時に現在値に掛ける倍率
    pub backoff_multiplier: f64,
}

impl Default for WebSocketPollConfig {
    fn default() -> Self {
        Self {
            mode: WebSocketPollMode::Adaptive,
            initial_timeout_ms: default_websocket_poll_timeout_ms(),
            max_timeout_ms: default_websocket_poll_max_timeout_ms(),
            backoff_multiplier: default_websocket_backoff_multiplier(),
        }
    }
}

/// ルートごとのセキュリティ設定
/// ルートごとのセキュリティ設定
#[derive(Deserialize, Clone, Debug)]
pub struct SecurityConfig {
    /// リクエストボディ最大サイズ（バイト）
    #[serde(default = "default_max_body_size")]
    pub max_request_body_size: usize,

    /// Chunked転送時の累積最大サイズ（バイト）
    #[serde(default = "default_max_body_size")]
    pub max_chunked_body_size: usize,

    /// クライアントヘッダー受信タイムアウト（秒）
    #[serde(default = "default_client_header_timeout")]
    pub client_header_timeout_secs: u64,

    /// クライアントボディ受信タイムアウト（秒）
    #[serde(default = "default_client_body_timeout")]
    pub client_body_timeout_secs: u64,

    /// 許可するHTTPメソッド（空 = すべて許可）
    #[serde(default)]
    pub allowed_methods: Vec<String>,

    /// 分間リクエスト数上限（0 = 無制限）
    #[serde(default)]
    pub rate_limit_requests_per_min: u64,

    /// バックエンド接続タイムアウト（秒）
    #[serde(default = "default_backend_connect_timeout")]
    pub backend_connect_timeout_secs: u64,

    /// ホストごとの最大アイドル接続数
    #[serde(default = "default_max_idle_connections")]
    pub max_idle_connections_per_host: usize,

    /// アイドル接続の維持時間（秒）
    #[serde(default = "default_idle_connection_timeout")]
    pub idle_connection_timeout_secs: u64,

    /// リクエストヘッダー最大サイズ（バイト）
    #[serde(default = "default_max_header_size")]
    pub max_request_header_size: usize,

    /// 許可するIPアドレス/CIDR（空 = すべて許可）
    /// 例: ["192.168.1.0/24", "10.0.0.1"]
    #[serde(default)]
    pub allowed_ips: Vec<String>,

    /// 拒否するIPアドレス/CIDR（denyが優先）
    /// 例: ["192.168.1.100", "10.0.0.0/8"]
    #[serde(default)]
    pub denied_ips: Vec<String>,

    // ====================
    // ヘッダー操作設定
    // ====================
    /// リクエストに追加するヘッダー（バックエンドへ転送前）
    /// 例: { "X-Real-IP" = "$client_ip", "X-Forwarded-Proto" = "https" }
    ///
    /// 特殊変数:
    /// - $client_ip: クライアントのIPアドレス
    /// - $host: リクエストのHostヘッダー
    /// - $request_uri: リクエストURI
    #[serde(default)]
    pub add_request_headers: HashMap<String, String>,

    /// リクエストから削除するヘッダー（バックエンドへ転送前）
    /// 例: ["X-Debug", "X-Internal-Token"]
    #[serde(default)]
    pub remove_request_headers: Vec<String>,

    /// レスポンスに追加するヘッダー（クライアントへ返送前）
    /// 例: { "X-Frame-Options" = "DENY", "Strict-Transport-Security" = "max-age=31536000" }
    #[serde(default)]
    pub add_response_headers: HashMap<String, String>,

    /// レスポンスから削除するヘッダー（クライアントへ返送前）
    /// 例: ["Server", "X-Powered-By"]
    #[serde(default)]
    pub remove_response_headers: Vec<String>,

    // ====================
    // WebSocket設定
    // ====================
    /// WebSocketポーリングモード
    ///
    /// - `"fixed"`: 固定タイムアウト（低レイテンシ優先）
    /// - `"adaptive"`: バックオフ方式による動的調整（CPU効率優先）
    ///
    /// デフォルト: `"adaptive"`
    #[serde(default)]
    pub websocket_poll_mode: WebSocketPollMode,

    /// WebSocketポーリング初期タイムアウト（ミリ秒）
    ///
    /// - fixedモード: この値を固定で使用
    /// - adaptiveモード: この値から開始し、アイドル時に徐々に延長
    ///
    /// デフォルト: `1`
    #[serde(default = "default_websocket_poll_timeout_ms")]
    pub websocket_poll_timeout_ms: u64,

    /// WebSocketポーリング最大タイムアウト（ミリ秒）
    ///
    /// adaptiveモードでのみ使用。
    /// タイムアウトはこの値を超えて延長されない。
    ///
    /// デフォルト: `100`
    #[serde(default = "default_websocket_poll_max_timeout_ms")]
    pub websocket_poll_max_timeout_ms: u64,

    /// WebSocketバックオフ倍率
    ///
    /// adaptiveモードでタイムアウト発生時に現在値に掛ける倍率。
    ///
    /// 例: `2.0` → 1ms → 2ms → 4ms → 8ms → ... → 100ms（最大値）
    ///
    /// デフォルト: `2.0`
    #[serde(default = "default_websocket_backoff_multiplier")]
    pub websocket_poll_backoff_multiplier: f64,
}

impl SecurityConfig {
    /// IP制限フィルターを構築
    pub fn ip_filter(&self) -> IpFilter {
        IpFilter::from_lists(&self.allowed_ips, &self.denied_ips)
    }

    /// ヘッダー操作が設定されているかどうか
    pub fn has_header_operations(&self) -> bool {
        !self.add_request_headers.is_empty()
            || !self.remove_request_headers.is_empty()
            || !self.add_response_headers.is_empty()
            || !self.remove_response_headers.is_empty()
    }

    /// セキュリティチェックが設定されているかどうか
    ///
    /// ## パフォーマンス最適化
    ///
    /// セキュリティ設定が全てデフォルト値の場合、ホットパスでの
    /// 複数のチェックを完全にスキップできます。
    /// これにより、設定がないルートでは5-10%の高速化が期待できます。
    ///
    /// チェック対象:
    /// - IP制限（allowed_ips, denied_ips）
    /// - HTTPメソッド制限（allowed_methods）
    /// - レートリミット（rate_limit_requests_per_min）
    #[inline]
    pub fn has_security_checks(&self) -> bool {
        !self.allowed_ips.is_empty()
            || !self.denied_ips.is_empty()
            || !self.allowed_methods.is_empty()
            || self.rate_limit_requests_per_min > 0
    }

    /// WebSocketポーリング設定を構築
    ///
    /// SecurityConfigのWebSocket関連フィールドから
    /// WebSocketPollConfig構造体を生成します。
    #[inline]
    pub fn websocket_poll_config(&self) -> WebSocketPollConfig {
        WebSocketPollConfig {
            mode: self.websocket_poll_mode,
            initial_timeout_ms: self.websocket_poll_timeout_ms,
            max_timeout_ms: self.websocket_poll_max_timeout_ms,
            backoff_multiplier: self.websocket_poll_backoff_multiplier,
        }
    }
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            max_request_body_size: default_max_body_size(),
            max_chunked_body_size: default_max_body_size(),
            client_header_timeout_secs: default_client_header_timeout(),
            client_body_timeout_secs: default_client_body_timeout(),
            allowed_methods: Vec::new(),
            rate_limit_requests_per_min: 0,
            backend_connect_timeout_secs: default_backend_connect_timeout(),
            max_idle_connections_per_host: default_max_idle_connections(),
            idle_connection_timeout_secs: default_idle_connection_timeout(),
            max_request_header_size: default_max_header_size(),
            allowed_ips: Vec::new(),
            denied_ips: Vec::new(),
            add_request_headers: HashMap::new(),
            remove_request_headers: Vec::new(),
            add_response_headers: HashMap::new(),
            remove_response_headers: Vec::new(),
            // WebSocket設定
            websocket_poll_mode: WebSocketPollMode::default(),
            websocket_poll_timeout_ms: default_websocket_poll_timeout_ms(),
            websocket_poll_max_timeout_ms: default_websocket_poll_max_timeout_ms(),
            websocket_poll_backoff_multiplier: default_websocket_backoff_multiplier(),
        }
    }
}

// ====================
// 圧縮設定（プロキシバックエンド用）
// ====================
//
// ルートごとにレスポンス圧縮を設定できます。
// デフォルトは無効で、kTLS最適化を維持します。
//
// 有効にすると、バックエンドからのレスポンスを動的に圧縮し、
// クライアントへ転送します。この場合、kTLSのゼロコピー最適化は
// 迂回されます。
// ====================

/// クライアントがサポートする圧縮方式
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AcceptedEncoding {
    /// Zstandard (zstd) - 最高効率
    Zstd,
    /// Brotli (br) - 高圧縮率
    Brotli,
    /// Gzip - 標準的な圧縮
    Gzip,
    /// Deflate（互換性のため）
    Deflate,
    /// 圧縮なし
    Identity,
}

impl AcceptedEncoding {
    /// Accept-Encodingヘッダーから最適な圧縮方式を選択
    ///
    /// 優先順位: zstd > br > gzip > deflate > identity
    /// q値（品質値）も考慮します。
    pub fn parse(value: &[u8]) -> Self {
        let value_str = match std::str::from_utf8(value) {
            Ok(s) => s.to_ascii_lowercase(),
            Err(_) => return Self::Identity,
        };

        // q値を考慮した解析
        let mut best = (Self::Identity, 0.0f32);

        for part in value_str.split(',') {
            let part = part.trim();
            let (encoding, q) = if let Some((enc, q_part)) = part.split_once(";q=") {
                (enc.trim(), q_part.trim().parse().unwrap_or(1.0))
            } else {
                (part, 1.0)
            };

            let candidate = match encoding {
                "zstd" => (Self::Zstd, q),
                "br" => (Self::Brotli, q),
                "gzip" => (Self::Gzip, q),
                "deflate" => (Self::Deflate, q),
                "*" => (Self::Gzip, q * 0.9), // * は gzip として扱う
                _ => continue,
            };

            // q値が高いもの、または同じq値ならZstd > Brotliを優先
            if candidate.1 > best.1
                || (candidate.1 == best.1 && matches!(candidate.0, Self::Zstd))
                || (candidate.1 == best.1
                    && matches!(candidate.0, Self::Brotli)
                    && !matches!(best.0, Self::Zstd))
            {
                best = candidate;
            }
        }

        best.0
    }

    /// Content-Encodingヘッダー値を返す
    pub fn as_header_value(&self) -> &'static [u8] {
        match self {
            Self::Zstd => b"zstd",
            Self::Brotli => b"br",
            Self::Gzip => b"gzip",
            Self::Deflate => b"deflate",
            Self::Identity => b"identity",
        }
    }
}

/// ルートごとの圧縮設定
#[derive(Deserialize, Clone, Debug)]
#[serde(default)]
pub struct CompressionConfig {
    /// 圧縮を有効にするかどうか
    /// デフォルト: false（kTLS最適化を維持）
    pub enabled: bool,

    /// 圧縮方式の優先順位
    /// サポート: "zstd", "br" (Brotli), "gzip", "deflate"
    /// デフォルト: ["zstd", "br", "gzip"]
    pub preferred_encodings: Vec<String>,

    /// Gzip圧縮レベル (1-9)
    /// 1: 最速（圧縮率低）、9: 最遅（圧縮率高）
    /// デフォルト: 4（バランス重視）
    #[serde(default = "default_gzip_level")]
    pub gzip_level: u32,

    /// Brotli圧縮レベル (0-11)
    /// 0: 最速、11: 最遅（圧縮率最高）
    /// デフォルト: 4（バランス重視）
    #[serde(default = "default_brotli_level")]
    pub brotli_level: u32,

    /// Zstd圧縮レベル (1-22)
    /// 1: 最速、22: 最遅（圧縮率最高）
    /// デフォルト: 3（高速重視）
    #[serde(default = "default_zstd_level")]
    pub zstd_level: i32,

    /// 最小圧縮サイズ（バイト）
    /// これより小さいレスポンスは圧縮オーバーヘッドの方が大きいためスキップ
    /// デフォルト: 1024 (1KB)
    #[serde(default = "default_compression_min_size")]
    pub min_size: usize,

    /// 圧縮対象のMIMEタイプ（プレフィックスマッチ）
    /// デフォルト: ["text/", "application/json", "application/javascript", ...]
    #[serde(default = "default_compressible_types")]
    pub compressible_types: Vec<String>,

    /// 圧縮をスキップするMIMEタイプ（プレフィックスマッチ）
    /// これらにマッチするレスポンスは圧縮対象から除外
    /// デフォルト: ["image/", "video/", "audio/", ...]
    #[serde(default = "default_skip_types")]
    pub skip_types: Vec<String>,
}

// 圧縮設定のデフォルト値
fn default_gzip_level() -> u32 {
    4
}
fn default_brotli_level() -> u32 {
    4
}
fn default_zstd_level() -> i32 {
    3
} // zstdは1-22、3が高速でバランス良好
fn default_compression_min_size() -> usize {
    1024
}

fn default_compressible_types() -> Vec<String> {
    vec![
        "text/".into(),
        "application/json".into(),
        "application/javascript".into(),
        "application/xml".into(),
        "application/xhtml+xml".into(),
        "application/rss+xml".into(),
        "application/atom+xml".into(),
        "image/svg+xml".into(),
        "application/wasm".into(),
    ]
}

fn default_skip_types() -> Vec<String> {
    vec![
        "image/".into(),
        "video/".into(),
        "audio/".into(),
        "application/octet-stream".into(),
        "application/zip".into(),
        "application/gzip".into(),
        "application/x-gzip".into(),
        "application/x-brotli".into(),
    ]
}

fn default_preferred_encodings() -> Vec<String> {
    vec!["zstd".into(), "br".into(), "gzip".into()]
}

impl Default for CompressionConfig {
    fn default() -> Self {
        Self {
            enabled: false, // デフォルト無効（kTLS最適化維持）
            preferred_encodings: default_preferred_encodings(),
            gzip_level: default_gzip_level(),
            brotli_level: default_brotli_level(),
            zstd_level: default_zstd_level(),
            min_size: default_compression_min_size(),
            compressible_types: default_compressible_types(),
            skip_types: default_skip_types(),
        }
    }
}

impl CompressionConfig {
    /// 設定の妥当性を検証
    pub fn validate(&self) -> Result<(), String> {
        if self.gzip_level < 1 || self.gzip_level > 9 {
            return Err(format!(
                "invalid gzip_level: {} (must be 1-9)",
                self.gzip_level
            ));
        }
        if self.brotli_level > 11 {
            return Err(format!(
                "invalid brotli_level: {} (must be 0-11)",
                self.brotli_level
            ));
        }
        if self.zstd_level < 1 || self.zstd_level > 22 {
            return Err(format!(
                "invalid zstd_level: {} (must be 1-22)",
                self.zstd_level
            ));
        }
        for enc in &self.preferred_encodings {
            match enc.as_str() {
                "zstd" | "gzip" | "br" | "deflate" => {}
                _ => return Err(format!("unknown encoding: {}", enc)),
            }
        }
        Ok(())
    }

    /// レスポンスを圧縮すべきか判定
    ///
    /// # Arguments
    /// * `client_encoding` - クライアントがサポートする圧縮方式
    /// * `content_type` - レスポンスのContent-Type
    /// * `content_length` - レスポンスのContent-Length（既知の場合）
    /// * `existing_encoding` - バックエンドからのContent-Encoding
    ///
    /// # Returns
    /// 圧縮すべき場合は使用する圧縮方式、それ以外はNone
    pub fn should_compress(
        &self,
        client_encoding: AcceptedEncoding,
        content_type: Option<&[u8]>,
        content_length: Option<usize>,
        existing_encoding: Option<&[u8]>,
    ) -> Option<AcceptedEncoding> {
        // 1. 圧縮が無効
        if !self.enabled {
            return None;
        }

        // 2. クライアントが圧縮非対応
        if client_encoding == AcceptedEncoding::Identity {
            return None;
        }

        // 3. バックエンドが既に圧縮済み
        if let Some(enc) = existing_encoding {
            if !enc.is_empty() && !enc.eq_ignore_ascii_case(b"identity") {
                return None;
            }
        }

        // 4. Content-Type確認
        if let Some(ct) = content_type {
            let ct_str = std::str::from_utf8(ct).unwrap_or("");
            info!("[Compression] Checking Content-Type: '{}'", ct_str);

            // スキップ対象をチェック
            for skip in &self.skip_types {
                if ct_str.starts_with(skip) {
                    return None;
                }
            }

            // 圧縮対象をチェック
            let is_compressible = self
                .compressible_types
                .iter()
                .any(|t| ct_str.starts_with(t));

            if !is_compressible {
                return None;
            }
        } else {
            // Content-Typeがない場合は圧縮しない
            return None;
        }

        // 5. サイズ確認
        if let Some(len) = content_length {
            info!(
                "[Compression] Checking Content-Length: {} (min_size: {})",
                len, self.min_size
            );
            if len < self.min_size {
                info!("[Compression] Content-Length is too small, skipping");
                return None;
            }
        } else {
            info!("[Compression] Content-Length is missing, proceeding anyway");
        }

        // 6. クライアントがサポートし、かつ設定で許可されている圧縮方式を選択
        let client_supports = |enc: &str| -> bool {
            matches!(
                (enc, client_encoding),
                ("zstd", AcceptedEncoding::Zstd)
                    | ("br", AcceptedEncoding::Brotli | AcceptedEncoding::Zstd)
                    | (
                        "gzip",
                        AcceptedEncoding::Gzip | AcceptedEncoding::Brotli | AcceptedEncoding::Zstd,
                    )
                    | (
                        "deflate",
                        AcceptedEncoding::Deflate
                            | AcceptedEncoding::Gzip
                            | AcceptedEncoding::Brotli
                            | AcceptedEncoding::Zstd,
                    )
            )
        };

        for enc in &self.preferred_encodings {
            if client_supports(enc) {
                return match enc.as_str() {
                    "zstd" if client_encoding == AcceptedEncoding::Zstd => {
                        Some(AcceptedEncoding::Zstd)
                    }
                    "br" if matches!(
                        client_encoding,
                        AcceptedEncoding::Brotli | AcceptedEncoding::Zstd
                    ) =>
                    {
                        Some(AcceptedEncoding::Brotli)
                    }
                    "gzip"
                        if matches!(
                            client_encoding,
                            AcceptedEncoding::Gzip
                                | AcceptedEncoding::Brotli
                                | AcceptedEncoding::Zstd
                        ) =>
                    {
                        Some(AcceptedEncoding::Gzip)
                    }
                    "deflate" => Some(AcceptedEncoding::Deflate),
                    _ => continue,
                };
            }
        }

        // クライアントの圧縮方式を使用
        Some(client_encoding)
    }
}

/// HTTP/3用の圧縮設定を解決
///
/// 優先順位:
/// 1. パスごとの設定 (compression.enabled = false なら圧縮しない)
/// 2. HTTP/3専用設定 (http3.compression_enabled + http3.compression.*)
/// 3. パスごとのデフォルト値
///
/// # 引数
/// * `path_compression` - パスごとの圧縮設定
/// * `http3_config` - HTTP/3セクションの設定
///
/// # 戻り値
/// 解決された圧縮設定
pub fn resolve_http3_compression_config(
    path_compression: &CompressionConfig,
    http3_config: &Http3ConfigSection,
) -> CompressionConfig {
    // パスごとの設定で明示的に有効化されている場合はそれを優先
    // （パス設定が既に有効なら、HTTP/3設定で上書きするだけ）
    if path_compression.enabled {
        // パス設定が有効な場合、HTTP/3専用パラメータで上書き
        let h3_comp = &http3_config.compression;
        return CompressionConfig {
            enabled: true,
            preferred_encodings: h3_comp
                .preferred_encodings
                .clone()
                .unwrap_or_else(|| path_compression.preferred_encodings.clone()),
            gzip_level: h3_comp.gzip_level.unwrap_or(path_compression.gzip_level),
            brotli_level: h3_comp
                .brotli_level
                .unwrap_or(path_compression.brotli_level),
            zstd_level: h3_comp.zstd_level.unwrap_or(path_compression.zstd_level),
            min_size: h3_comp.min_size.unwrap_or(path_compression.min_size),
            compressible_types: h3_comp
                .compressible_types
                .clone()
                .unwrap_or_else(|| path_compression.compressible_types.clone()),
            skip_types: h3_comp
                .skip_types
                .clone()
                .unwrap_or_else(|| path_compression.skip_types.clone()),
        };
    }

    // パスごとの設定で圧縮が無効の場合
    // HTTP/3の compression_enabled をチェック
    if http3_config.compression_enabled {
        // HTTP/3で圧縮が有効化されている場合、HTTP/3専用設定を適用
        let h3_comp = &http3_config.compression;
        return CompressionConfig {
            enabled: true, // HTTP/3では有効
            preferred_encodings: h3_comp
                .preferred_encodings
                .clone()
                .unwrap_or_else(|| path_compression.preferred_encodings.clone()),
            gzip_level: h3_comp.gzip_level.unwrap_or(path_compression.gzip_level),
            brotli_level: h3_comp
                .brotli_level
                .unwrap_or(path_compression.brotli_level),
            zstd_level: h3_comp.zstd_level.unwrap_or(path_compression.zstd_level),
            min_size: h3_comp.min_size.unwrap_or(path_compression.min_size),
            compressible_types: h3_comp
                .compressible_types
                .clone()
                .unwrap_or_else(|| path_compression.compressible_types.clone()),
            skip_types: h3_comp
                .skip_types
                .clone()
                .unwrap_or_else(|| path_compression.skip_types.clone()),
        };
    }

    // HTTP/3圧縮も無効の場合はパス設定をそのまま使用（圧縮無効）
    path_compression.clone()
}

/// グローバルセキュリティ設定
#[derive(Deserialize, Clone, Debug, Default)]
pub struct GlobalSecurityConfig {
    /// 起動後に降格するユーザー名（非root推奨）
    #[serde(default)]
    pub drop_privileges_user: Option<String>,

    /// 起動後に降格するグループ名
    #[serde(default)]
    pub drop_privileges_group: Option<String>,

    /// グローバル同時接続上限（0 = 無制限）
    #[serde(default)]
    pub max_concurrent_connections: usize,

    /// 最前線 DDoS 防御: ブロックする IP/CIDR のリスト（F-35）。
    /// accept 直後（TLS ハンドシェイク前・ハンドラ spawn 前）に評価し、マッチした接続を
    /// 即座に切断する。既知の不正 IP に対する高コスト処理（TLS ハンドシェイク等）を回避する。
    /// 例: ["203.0.113.0/24", "198.51.100.5"]
    #[serde(default)]
    pub blocked_ips: Vec<String>,

    // ====================
    // io_uring / seccomp セキュリティ設定
    // ====================
    /// seccompフィルタを有効化（Linux専用）
    /// システムコールを制限してio_uringの悪用を防止
    #[serde(default)]
    pub enable_seccomp: bool,

    /// seccompモード
    /// - "disabled": 無効
    /// - "log": 違反をログに記録（ブロックしない）
    /// - "filter": 違反をEPERMで拒否
    /// - "strict": 違反したプロセスをSIGKILL
    #[serde(default = "default_seccomp_mode")]
    pub seccomp_mode: String,

    /// Landlockファイルシステム制限を有効化（Linux 5.13+）
    #[serde(default)]
    pub enable_landlock: bool,

    /// Landlock読み取り専用パス
    #[serde(default = "default_landlock_read_paths")]
    pub landlock_read_paths: Vec<String>,

    /// Landlock読み書きパス
    #[serde(default = "default_landlock_write_paths")]
    pub landlock_write_paths: Vec<String>,

    // ====================
    // サンドボックス設定（bubblewrap相当）
    // ====================
    //
    // Linuxのnamespace分離、bind mounts、capabilities制限を
    // プログラム起動時に適用することで、bubblewrapと同等の
    // セキュリティ分離を実現します。
    //
    // 適用順序:
    // 1. サンドボックス（namespace分離、bind mounts、capabilities）
    // 2. 権限降格（setuid/setgid）
    // 3. Landlock（ファイルシステム制限）
    // 4. seccomp（システムコール制限）
    //
    /// サンドボックスを有効化
    /// bubblewrap相当のnamespace分離、bind mounts、capabilities制限を適用
    #[serde(default)]
    pub enable_sandbox: bool,

    /// PID namespace分離
    /// サンドボックス内のプロセスは外部のプロセスを見ることができなくなります
    #[serde(default)]
    pub sandbox_unshare_pid: bool,

    /// Mount namespace分離
    /// サンドボックス内で独自のマウントポイントを持ちます
    #[serde(default = "default_sandbox_unshare_mount")]
    pub sandbox_unshare_mount: bool,

    /// UTS namespace分離
    /// サンドボックス内で独自のホスト名を持ちます
    #[serde(default = "default_sandbox_unshare_uts")]
    pub sandbox_unshare_uts: bool,

    /// IPC namespace分離
    /// サンドボックス内で独自のIPC（共有メモリ、セマフォ等）を持ちます
    #[serde(default = "default_sandbox_unshare_ipc")]
    pub sandbox_unshare_ipc: bool,

    /// User namespace分離
    /// 注: 複雑なケースがあるためデフォルトは無効
    #[serde(default)]
    pub sandbox_unshare_user: bool,

    /// Network namespace分離
    /// 警告: trueにするとネットワーク通信ができなくなります
    /// サーバーでは通常false（--share-net相当）
    #[serde(default)]
    pub sandbox_unshare_net: bool,

    /// 読み取り専用バインドマウント
    /// source:dest 形式で指定（例: "/usr:/usr"）
    #[serde(default = "default_sandbox_ro_binds")]
    pub sandbox_ro_bind_mounts: Vec<String>,

    /// 読み書きバインドマウント
    /// source:dest 形式で指定（例: "/var/log:/var/log"）
    #[serde(default)]
    pub sandbox_rw_bind_mounts: Vec<String>,

    /// tmpfsマウント先
    /// 指定されたパスにtmpfs（メモリファイルシステム）をマウント
    #[serde(default = "default_sandbox_tmpfs")]
    pub sandbox_tmpfs_mounts: Vec<String>,

    /// /proc をマウントするかどうか
    #[serde(default = "default_true")]
    pub sandbox_mount_proc: bool,

    /// /dev に最小限のデバイスノードを作成するかどうか
    #[serde(default = "default_true")]
    pub sandbox_mount_dev: bool,

    /// ドロップするケイパビリティのリスト
    /// 例: ["CAP_SYS_ADMIN", "CAP_NET_RAW"]
    #[serde(default)]
    pub sandbox_drop_capabilities: Vec<String>,

    /// 保持するケイパビリティのリスト（他は全てドロップ）
    /// drop_capabilitiesより優先されます
    /// 例: ["CAP_NET_BIND_SERVICE"]
    #[serde(default)]
    pub sandbox_keep_capabilities: Vec<String>,

    /// サンドボックス内のホスト名
    #[serde(default = "default_sandbox_hostname")]
    pub sandbox_hostname: Option<String>,

    /// PR_SET_NO_NEW_PRIVSを設定するかどうか
    #[serde(default = "default_true")]
    pub sandbox_no_new_privs: bool,

    /// セキュリティ機能の有効化に失敗した場合の動作
    /// false: 失敗時に起動を中止（デフォルト、推奨）
    /// true: 失敗時も警告を出して起動を続行（開発・デバッグ用）
    #[serde(default = "default_allow_security_failures")]
    pub allow_security_failures: bool,

    // ====================
    // FreeBSD: capsicum（F-120 Phase 4）
    // ====================
    //
    // 非対象 OS（Linux/OpenBSD）でもキー自体は受理し、未知キー拒否にはしない
    // （設計ドキュメント 4.4 節）。適用時に警告ログを出して無視する。
    /// capsicum を有効化（FreeBSD 専用）。
    ///
    /// リスナー/接続/静的ファイル fd への `cap_rights_limit(2)` を適用する。
    #[serde(default)]
    pub enable_capsicum: bool,

    /// capability mode（`cap_enter(2)`）へ降格する（FreeBSD 専用・オプトイン）。
    ///
    /// capability mode ではグローバル名前空間操作（`bind(2)`/`connect(2)`/パス指定の
    /// `open(2)` 等）が全面禁止されるため、次の制約を **すべて** 満たす構成でのみ
    /// 適用できる（満たさない場合は警告を出して適用しない）:
    /// - プロキシ/upstream・L4 リスナーが無い（`connect(2)` 不要）
    /// - h2c / HTTP/3 / metrics の追加リスナーが無い
    ///
    /// 適用は全ワーカーの listener bind 完了後（bind は capability mode で不可のため）。
    /// 注意: 現状、リクエスト時のパス指定 `open(2)` を伴う静的ファイル配信は
    /// capability mode では失敗する（キャッシュ済み応答のみ返せる）。ディレクトリ fd +
    /// `openat(2)` 相対化による完全対応はフォローアップ（backlog 参照）。
    #[serde(default)]
    pub capsicum_capability_mode: bool,

    /// 起動時に attach する jail 名（FreeBSD 専用、root 前提）。
    /// 未設定なら jail_attach は行わない。
    #[serde(default)]
    pub jail_name: Option<String>,

    // ====================
    // OpenBSD: pledge / unveil（F-120 Phase 5）
    // ====================
    //
    // 非対象 OS（Linux/FreeBSD）でもキー自体は受理し、未知キー拒否にはしない
    // （設計ドキュメント 4.4 節）。適用時に警告ログを出して無視する。
    /// unveil(2) を有効化（OpenBSD 専用）。
    ///
    /// 設定ファイル・TLS 証明書/鍵・静的ファイルルート・アクセスログ/アプリログの
    /// ディレクトリ・ディスクキャッシュディレクトリ・WASM モジュールパスを
    /// 必要権限（"r" / "rwc"）で unveil し、最後に `unveil(NULL, NULL)` でロックする。
    #[serde(default)]
    pub enable_unveil: bool,

    /// pledge(2) を有効化（OpenBSD 専用）。
    ///
    /// 全 TLS ワーカーの listener bind 完了後に最小 promise 集合へ降格する。
    #[serde(default)]
    pub enable_pledge: bool,
}

fn default_allow_security_failures() -> bool {
    false // デフォルトはfalse（失敗時に起動失敗）
}

fn default_sandbox_unshare_mount() -> bool {
    true
}
fn default_sandbox_unshare_uts() -> bool {
    true
}
fn default_sandbox_unshare_ipc() -> bool {
    true
}
fn default_true() -> bool {
    true
}

fn default_sandbox_ro_binds() -> Vec<String> {
    vec![
        "/usr:/usr".to_string(),
        "/lib:/lib".to_string(),
        "/lib64:/lib64".to_string(),
        "/etc/ssl:/etc/ssl".to_string(),
        // DNS解決に必要なファイル
        "/etc/resolv.conf:/etc/resolv.conf".to_string(),
        "/etc/hosts:/etc/hosts".to_string(),
        "/etc/nsswitch.conf:/etc/nsswitch.conf".to_string(),
        "/etc/gai.conf:/etc/gai.conf".to_string(),
        // systemd-resolved使用時に必要（存在しない場合は無視される）
        "/run/systemd/resolve:/run/systemd/resolve".to_string(),
        // ユーザー/グループ情報
        "/etc/passwd:/etc/passwd".to_string(),
        "/etc/group:/etc/group".to_string(),
    ]
}

fn default_sandbox_tmpfs() -> Vec<String> {
    vec![
        "/tmp".to_string(),
        // 注: /run はsystemd-resolvedのDNS解決に必要なため除外
        // 必要な場合は明示的に追加してください
    ]
}

fn default_sandbox_hostname() -> Option<String> {
    Some("veil-sandbox".to_string())
}

fn default_seccomp_mode() -> String {
    "disabled".to_string()
}

fn default_landlock_read_paths() -> Vec<String> {
    vec![
        "/etc".to_string(),
        "/usr".to_string(),
        "/lib".to_string(),
        "/lib64".to_string(),
    ]
}

fn default_landlock_write_paths() -> Vec<String> {
    vec!["/var/log".to_string(), "/tmp".to_string()]
}

// ====================
// OpenBSD: unveil 対象パス収集（F-120 Phase 5）
// ====================

/// OpenBSD unveil(2) に渡すパス集合。
#[cfg(target_os = "openbsd")]
pub struct UnveilPaths {
    /// 読み取り専用で unveil するパス（"r"）: 設定ファイル・TLS証明書/鍵・静的ファイル
    /// ルート・WASM モジュール。
    pub read_only: Vec<PathBuf>,
    /// 読み書き・作成を伴う unveil するパス（"rwc"）: アクセスログ/アプリログの
    /// ディレクトリ・ディスクキャッシュディレクトリ。
    pub read_write_create: Vec<PathBuf>,
}

/// 設定ファイルを軽量に再パースし、unveil(2) に必要なパス集合を導出する（OpenBSD 専用）。
///
/// `load_config` が返す `LoadedConfig` は WASM エンジン構築・TLS 証明書読み込み等で
/// 個々のパス文字列を消費済みで再取得できないフィールドがあるため、unveil 専用に
/// 設定ファイルをもう一度パースする（起動時コールドパスで 1 回のみ・ホットパス無関係）。
#[cfg(target_os = "openbsd")]
pub fn collect_unveil_paths(config_path: &Path) -> io::Result<UnveilPaths> {
    let config_str = fs::read_to_string(config_path)?;
    let config: Config = toml::from_str(&config_str).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("TOML parse error: {}", e),
        )
    })?;

    let mut read_only = vec![config_path.to_path_buf()];
    let mut read_write_create = Vec::new();

    read_only.push(PathBuf::from(&config.tls.cert_path));
    read_only.push(PathBuf::from(&config.tls.key_path));

    // プロキシ経路の名前解決（getaddrinfo）と upstream TLS 検証に必要なシステムファイル。
    // 静的配信のみの構成では未使用だが、存在すれば読み取り許可しておく（unveil_path は
    // 不存在パスをスキップするため、無害）。Landlock のデフォルト読み取りパスと同方針。
    for sys_path in [
        "/etc/resolv.conf",
        "/etc/hosts",
        "/etc/ssl/cert.pem", // OpenBSD のシステム信頼ストア
        "/etc/ssl",
    ] {
        read_only.push(PathBuf::from(sys_path));
    }

    if let Some(routes) = &config.route {
        for route in routes {
            if let BackendConfig::File { path, .. } = &route.action {
                read_only.push(PathBuf::from(path));
            }
            if let Some(cache_cfg) = &route.cache {
                if let Some(disk_path) = &cache_cfg.disk_path {
                    read_write_create.push(disk_path.clone());
                }
            }
        }
    }

    #[cfg(feature = "wasm")]
    if let Some(wasm_cfg) = &config.wasm {
        for module in &wasm_cfg.modules {
            read_only.push(PathBuf::from(&module.path));
        }
    }

    #[cfg(feature = "access-log")]
    if let Some(file_path) = &config.access_log.file_path {
        push_unveil_parent_dir(&mut read_write_create, file_path);
    }

    if let Some(file_path) = &config.logging.app_file_path {
        push_unveil_parent_dir(&mut read_write_create, file_path);
    }
    if let Some(file_path) = &config.logging.error_file_path {
        push_unveil_parent_dir(&mut read_write_create, file_path);
    }

    Ok(UnveilPaths {
        read_only,
        read_write_create,
    })
}

/// ログ/アクセスログファイルパスの親ディレクトリ（ローテーション・新規作成に必要）を
/// rwc 対象へ追加する。親ディレクトリが取得できない場合はファイルパス自体を追加する。
#[cfg(target_os = "openbsd")]
fn push_unveil_parent_dir(paths: &mut Vec<PathBuf>, file_path: &str) {
    let p = Path::new(file_path);
    match p.parent() {
        Some(parent) if !parent.as_os_str().is_empty() => paths.push(parent.to_path_buf()),
        _ => paths.push(p.to_path_buf()),
    }
}

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
    /// 空の場合はすべてのIPからアクセス可能
    /// 例: ["127.0.0.1", "10.0.0.0/8", "192.168.0.0/16"]
    #[serde(default)]
    pub allowed_ips: Vec<String>,
}

fn default_prometheus_enabled() -> bool {
    false
}
fn default_prometheus_path() -> String {
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
    /// アクセスを許可するIPアドレス/CIDR（空の場合は全IPを許可）
    /// 例: ["127.0.0.1", "10.0.0.0/8", "192.168.0.0/16"]
    #[serde(default)]
    pub allowed_ips: Vec<String>,
    /// キャッシュパージプレフィックス（事前計算、リクエスト毎の format! を回避）
    /// デシリアライズ時に自動計算される: "{path_prefix}/cache/purge"
    #[serde(skip)]
    pub cache_purge_prefix: String,
}

#[cfg(feature = "admin")]
fn default_admin_path_prefix() -> String {
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

fn default_otel_endpoint() -> String {
    "http://localhost:4318".to_string()
}
fn default_otel_service_name() -> String {
    "veil-proxy".to_string()
}
fn default_otel_batch_interval() -> u64 {
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
                // 定数時間比較ではないが、管理 API は信頼ネットワーク前提
                token == self.secret
            }
            None => false,
        }
    }

    /// クライアント IP が管理 API エンドポイントへのアクセスを許可されているか確認する。
    ///
    /// `allowed_ips` が空の場合はすべての IP を許可する。
    pub fn is_ip_allowed(&self, client_ip: &str) -> bool {
        if self.allowed_ips.is_empty() {
            return true;
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
        // allowed_ipsが空の場合はすべてのIPを許可
        if self.allowed_ips.is_empty() {
            return true;
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

// ====================
// Graceful Shutdown / Hot Reload フラグ
// ====================

/// シャットダウンフラグ（Ctrl+C等でtrueに設定）
/// HTTP/3モジュールからも参照できるようにpub
pub static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

/// 設定リロード要求フラグ（SIGHUP でトリガー）
/// Arc<AtomicBool> として初期化（signal-hook の要件）
pub static RELOAD_FLAG: Lazy<Arc<AtomicBool>> = Lazy::new(|| Arc::new(AtomicBool::new(false)));

/// TLS 証明書リロード要求フラグ（SIGHUP でトリガー、F-03）
/// 設定リロードとは独立して証明書のみを再読み込みするために使用する。
pub static TLS_RELOAD_FLAG: Lazy<Arc<AtomicBool>> = Lazy::new(|| Arc::new(AtomicBool::new(false)));

/// グローバル IP ブロックリスト（最前線 DDoS 防御、F-35）。
///
/// `accept` 直後（TLS ハンドシェイク前・ハンドラ spawn 前）に評価し、ブロック対象 IP の
/// 接続を即座に切断することで、既知の不正 IP に対する TLS ハンドシェイク等の高コスト処理を
/// 行わずに弾く。CIDR は起動時にパース済み（`CidrRange`）で保持し、accept ホットパスでは
/// 文字列パースを行わない。`ArcSwap` により SIGHUP ホットリロードに対応する。
///
/// 注: NIC ドライバ段でドロップする XDP/eBPF（F-35 の本来の設計）は、ビルドに nightly +
/// bpf-linker、実行に CAP_BPF/対応 NIC を要し本リポジトリのサンドボックスでは検証不能なため、
/// 検証可能な「ユーザースペース最前線（accept 段の事前ドロップ）」をまず実装する。
pub static GLOBAL_BLOCKED_IPS: Lazy<ArcSwap<Vec<CidrRange>>> =
    Lazy::new(|| ArcSwap::from_pointee(Vec::new()));

/// グローバル IP ブロックリストを設定する（起動時・SIGHUP リロード時に呼ぶ）。
/// パース不能なエントリは黙って無視する（設定検証は別途 `validate` で行う）。
pub fn set_global_blocked_ips(cidrs: &[String]) {
    let parsed: Vec<CidrRange> = cidrs.iter().filter_map(|s| CidrRange::parse(s)).collect();
    GLOBAL_BLOCKED_IPS.store(Arc::new(parsed));
}

/// 指定 IP がグローバルブロックリストに含まれるか（accept ホットパス、ゼロアロケーション）。
///
/// ブロックリスト未設定（空）時は即 `false` を返し、ホットパスのオーバーヘッドを発生させない。
#[inline]
pub fn is_ip_blocked(ip: std::net::IpAddr) -> bool {
    let list = GLOBAL_BLOCKED_IPS.load();
    if list.is_empty() {
        return false;
    }
    list.iter().any(|cidr| cidr.contains_addr(ip))
}

/// グレースフルシャットダウンタイムアウト（秒）
/// 起動時に設定ファイルから読み込まれ、ワーカースレッドがドレイン待機時に参照
pub static GRACEFUL_SHUTDOWN_TIMEOUT_SECS: AtomicU64 = AtomicU64::new(30);

// secure_zero, secure_clear_arc_vec は crate::system モジュールに移動しました。

// ====================
// 同時接続数カウンター
// ====================
//
// グローバルなアトミックカウンターで現在の接続数を追跡します。
// max_concurrent_connections が設定されている場合、上限を超える接続は拒否されます。
// ====================

pub static CURRENT_CONNECTIONS: AtomicUsize = AtomicUsize::new(0);

/// グレースフルシャットダウン時に既存接続の完了を待機
///
/// CURRENT_CONNECTIONSが0になるか、タイムアウトに達するまで待機します。
/// 設定されたタイムアウト（GRACEFUL_SHUTDOWN_TIMEOUT_SECS）を使用。
///
/// # Arguments
/// * `worker_type` - ワーカーの種類（ログ表示用）
/// * `thread_id` - スレッドID（ログ表示用）
pub async fn drain_connections(worker_type: &str, thread_id: usize) {
    let timeout_secs = GRACEFUL_SHUTDOWN_TIMEOUT_SECS.load(Ordering::Relaxed);

    if timeout_secs == 0 {
        // タイムアウト0の場合は即座に終了（既存の動作）
        return;
    }

    let start = std::time::Instant::now();
    let drain_timeout = Duration::from_secs(timeout_secs);

    // 初期接続数を確認
    let initial_connections = CURRENT_CONNECTIONS.load(Ordering::Relaxed);
    if initial_connections == 0 {
        info!(
            "[{} {}] No active connections, proceeding with shutdown",
            worker_type, thread_id
        );
        return;
    }

    info!(
        "[{} {}] Draining {} active connections (timeout: {}s)...",
        worker_type, thread_id, initial_connections, timeout_secs
    );

    // 接続数が0になるかタイムアウトするまで待機
    while CURRENT_CONNECTIONS.load(Ordering::Relaxed) > 0 {
        if start.elapsed() > drain_timeout {
            let remaining = CURRENT_CONNECTIONS.load(Ordering::Relaxed);
            warn!(
                "[{} {}] Drain timeout after {}s, {} connections still active (forcing shutdown)",
                worker_type, thread_id, timeout_secs, remaining
            );
            break;
        }
        crate::runtime::time::sleep(Duration::from_millis(100)).await;
    }

    let elapsed = start.elapsed();
    let remaining = CURRENT_CONNECTIONS.load(Ordering::Relaxed);
    if remaining == 0 {
        info!(
            "[{} {}] All connections drained in {:.1}s",
            worker_type,
            thread_id,
            elapsed.as_secs_f64()
        );
    }
}

// ====================
// レートリミッター（スライディングウィンドウ方式）
// ====================
//
// クライアントIPごとに分間リクエスト数を追跡します。
// スレッドローカルで管理し、ロックフリーで高パフォーマンスを実現。
// ====================

/// レートリミットのエントリ
pub struct RateLimitEntry {
    /// 現在のウィンドウ（分）のリクエスト数
    pub current_count: u32,
    /// 前のウィンドウ（分）のリクエスト数
    pub previous_count: u32,
    /// 現在のウィンドウの開始時刻（分単位のタイムスタンプ）
    pub current_minute: u64,
}

impl RateLimitEntry {
    pub fn new(current_minute: u64) -> Self {
        Self {
            current_count: 1,
            previous_count: 0,
            current_minute,
        }
    }

    /// リクエストを記録し、現在のレートを返す（スライディングウィンドウ方式）
    /// 返り値: 推定される分間リクエスト数
    pub fn record_request(&mut self, now_minute: u64, now_second_in_minute: u32) -> u32 {
        if now_minute > self.current_minute {
            if now_minute == self.current_minute + 1 {
                // 次の分に移行
                self.previous_count = self.current_count;
                self.current_count = 1;
            } else {
                // 2分以上経過 - リセット
                self.previous_count = 0;
                self.current_count = 1;
            }
            self.current_minute = now_minute;
        } else {
            self.current_count += 1;
        }

        // スライディングウィンドウによる推定レート計算
        // 現在の分の経過割合に基づいて重み付け
        let weight = (60 - now_second_in_minute) as f32 / 60.0;
        let estimated = (self.previous_count as f32 * weight) + self.current_count as f32;
        estimated.ceil() as u32
    }
}

/// スレッドローカルなレートリミットマップ
/// キー: クライアントIPアドレス（文字列）
/// 値: RateLimitEntry
pub struct RateLimiter {
    pub entries: HashMap<String, RateLimitEntry>,
    pub last_cleanup: std::time::Instant,
}

impl RateLimiter {
    fn new() -> Self {
        Self {
            entries: HashMap::new(),
            last_cleanup: std::time::Instant::now(),
        }
    }

    /// リクエストをチェックし、レート制限を超えていないか確認
    /// 戻り値: (許可されたか, 現在のレート)
    ///
    /// ## パフォーマンス最適化
    ///
    /// SystemTime::now()の代わりにCoarse Timerを使用してシステムコールを削減。
    /// 100ms程度の精度低下は、レートリミットの用途では許容範囲。
    fn check_and_record(&mut self, client_ip: &str, limit: u64) -> (bool, u32) {
        // 定期的なクリーンアップ（5分ごと）
        if self.last_cleanup.elapsed().as_secs() > 300 {
            self.cleanup();
            self.last_cleanup = std::time::Instant::now();
        }

        // Coarse Timerから現在時刻を取得（システムコール削減）
        // OffsetDateTime から Unix タイムスタンプを計算
        let now_time = coarse_now();
        let now_secs = now_time.unix_timestamp() as u64;
        let now_minute = now_secs / 60;
        let now_second_in_minute = (now_secs % 60) as u32;

        let rate = if let Some(entry) = self.entries.get_mut(client_ip) {
            entry.record_request(now_minute, now_second_in_minute)
        } else {
            self.entries
                .insert(client_ip.to_string(), RateLimitEntry::new(now_minute));
            1
        };

        (rate as u64 <= limit, rate)
    }

    /// 古いエントリをクリーンアップ
    ///
    /// Coarse Timerを使用してシステムコールを削減。
    fn cleanup(&mut self) {
        // Coarse Timerから現在時刻を取得
        let now_time = coarse_now();
        let now_minute = now_time.unix_timestamp() as u64 / 60;

        // 2分以上古いエントリを削除
        self.entries
            .retain(|_, entry| now_minute.saturating_sub(entry.current_minute) < 2);
    }
}

thread_local! {
    static RATE_LIMITER: RefCell<RateLimiter> = RefCell::new(RateLimiter::new());
}

/// レートリミットをチェック
/// 戻り値: レート制限内であればtrue
pub fn check_rate_limit(client_ip: &str, limit: u64) -> bool {
    if limit == 0 {
        return true; // 0 = 無制限
    }

    RATE_LIMITER.with(|limiter| {
        let (allowed, _rate) = limiter.borrow_mut().check_and_record(client_ip, limit);
        allowed
    })
}

// ====================
// TLSコネクタ（スレッドローカル）
// ====================

// rustls 用の TLS コネクター（kTLS 有効時は ktls_rustls を使用）
// kTLSフィーチャー有効時はシークレット抽出を有効化し、kTLS利用可能な状態にする
#[cfg(all(veil_ktls, feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR: RustlsConnector = {
        // 設定ファイルから kTLS 有効化、ktls_fallback_enabled, tcp_cork_enabled を読み込み
        let config_guard = CURRENT_CONFIG.load();
        let ktls_enabled = config_guard.ktls_config.enabled;
        let fallback_enabled = config_guard.ktls_config.fallback_enabled;
        let tcp_cork_enabled = config_guard.ktls_config.tcp_cork_enabled;

        // kTLS が有効な場合のみシークレット抽出を有効化した設定を使用
        let config = (*crate::ktls_rustls::client_config(ktls_enabled)).clone();
        let config = crate::protocol::configure_alpn_h2_client(config, false);

        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls_enabled)        // 設定に基づいて kTLS を有効化
            .with_fallback(fallback_enabled)    // kTLS 失敗時のフォールバック設定
            .with_tcp_cork(tcp_cork_enabled)    // TCP_CORK 設定
    };
}

// kTLS あり・HTTP/2 なしの場合のコネクター
#[cfg(all(veil_ktls, not(feature = "http2")))]
thread_local! {
    static TLS_CONNECTOR: RustlsConnector = {
        let config_guard = CURRENT_CONFIG.load();
        let ktls_enabled = config_guard.ktls_config.enabled;
        let fallback_enabled = config_guard.ktls_config.fallback_enabled;
        let tcp_cork_enabled = config_guard.ktls_config.tcp_cork_enabled;

        let config = (*crate::ktls_rustls::client_config(ktls_enabled)).clone();

        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls_enabled)
            .with_fallback(fallback_enabled)
            .with_tcp_cork(tcp_cork_enabled)
    };
}

// 証明書検証をスキップする TLS コネクター（kTLS 有効時・自己署名証明書用）
#[cfg(all(veil_ktls, feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR_INSECURE: RustlsConnector = {
        // 証明書検証をスキップするクライアント設定
        // 設定ファイルから kTLS 有効化、ktls_fallback_enabled, tcp_cork_enabled を読み込み
        let config_guard = CURRENT_CONFIG.load();
        let ktls_enabled = config_guard.ktls_config.enabled;
        let fallback_enabled = config_guard.ktls_config.fallback_enabled;
        let tcp_cork_enabled = config_guard.ktls_config.tcp_cork_enabled;

        let config = (*crate::ktls_rustls::insecure_client_config()).clone();
        let config = crate::protocol::configure_alpn_h2_client(config, false);

        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls_enabled)
            .with_fallback(fallback_enabled)
            .with_tcp_cork(tcp_cork_enabled)
    };
}

// kTLS あり・HTTP/2 なしの場合の insecure コネクター
#[cfg(all(veil_ktls, not(feature = "http2")))]
thread_local! {
    static TLS_CONNECTOR_INSECURE: RustlsConnector = {
        let config_guard = CURRENT_CONFIG.load();
        let ktls_enabled = config_guard.ktls_config.enabled;
        let fallback_enabled = config_guard.ktls_config.fallback_enabled;
        let tcp_cork_enabled = config_guard.ktls_config.tcp_cork_enabled;

        let config = (*crate::ktls_rustls::insecure_client_config()).clone();

        RustlsConnector::new(Arc::new(config))
            .with_ktls(ktls_enabled)
            .with_fallback(fallback_enabled)
            .with_tcp_cork(tcp_cork_enabled)
    };
}

// rustls 用の TLS コネクター（kTLS 無効時は simple_tls を使用）
#[cfg(all(not(veil_ktls), feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::default_client_config()).clone();
        let config = protocol::configure_alpn_h2_client(config, false);
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// HTTP/2 なし・kTLS なしの場合のコネクター
#[cfg(all(not(veil_ktls), not(feature = "http2")))]
thread_local! {
    static TLS_CONNECTOR: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::default_client_config()).clone();
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// 証明書検証をスキップする TLS コネクター（自己署名証明書用）
#[cfg(all(not(veil_ktls), feature = "http2"))]
thread_local! {
    static TLS_CONNECTOR_INSECURE: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::insecure_client_config()).clone();
        let config = protocol::configure_alpn_h2_client(config, false);
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// HTTP/2 なし・kTLS なしの場合の insecure コネクター
#[cfg(all(not(veil_ktls), not(feature = "http2")))]
thread_local! {
    static TLS_CONNECTOR_INSECURE: simple_tls::SimpleTlsConnector = {
        let config = (*simple_tls::insecure_client_config()).clone();
        simple_tls::SimpleTlsConnector::new(Arc::new(config))
    };
}

// ====================
// TLS コネクタアクセサ関数
// ====================

/// TLS コネクタを取得（通常接続用）
#[cfg(veil_ktls)]
pub fn get_tls_connector() -> RustlsConnector {
    TLS_CONNECTOR.with(|c| c.clone())
}

/// TLS コネクタを取得（証明書検証スキップ用）
#[cfg(veil_ktls)]
pub fn get_tls_connector_insecure() -> RustlsConnector {
    TLS_CONNECTOR_INSECURE.with(|c| c.clone())
}

/// TLS コネクタを取得（通常接続用）
#[cfg(not(veil_ktls))]
pub fn get_tls_connector() -> crate::simple_tls::SimpleTlsConnector {
    TLS_CONNECTOR.with(|c| c.clone())
}

/// TLS コネクタを取得（証明書検証スキップ用）
#[cfg(not(veil_ktls))]
pub fn get_tls_connector_insecure() -> crate::simple_tls::SimpleTlsConnector {
    TLS_CONNECTOR_INSECURE.with(|c| c.clone())
}

// ====================
// WASM Response Filter Context
// ====================
// 設定構造体
// ====================

/// Upstream サーバーエントリ（文字列または構造体）
///
/// 以下の2つの形式をサポート:
/// - 文字列形式: "http://localhost:8080"
/// - 構造体形式: { url = "https://192.168.1.100:443", sni_name = "api.example.com" }
#[derive(Clone, Debug)]
pub struct UpstreamServerEntry {
    pub url: String,
    pub sni_name: Option<String>,
    pub use_h2c: bool,
    /// 重み（Weighted Round Robin 用、デフォルト 1）
    pub weight: u32,
}

impl<'de> serde::Deserialize<'de> for UpstreamServerEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};

        struct UpstreamServerEntryVisitor;

        impl<'de> Visitor<'de> for UpstreamServerEntryVisitor {
            type Value = UpstreamServerEntry;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a string URL or an object with 'url' and optional 'sni_name'")
            }

            // 文字列形式: "http://localhost:8080"
            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                Ok(UpstreamServerEntry {
                    url: v.to_string(),
                    sni_name: None,
                    use_h2c: false,
                    weight: 1,
                })
            }

            // 構造体形式: { url = "...", sni_name = "...", weight = 2 }
            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut url: Option<String> = None;
                let mut sni_name: Option<String> = None;
                let mut use_h2c: Option<bool> = None;
                let mut weight: Option<u32> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "url" => url = Some(map.next_value()?),
                        "sni_name" => sni_name = Some(map.next_value()?),
                        "use_h2c" | "h2c" => use_h2c = Some(map.next_value()?),
                        "weight" => weight = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let url = url.ok_or_else(|| serde::de::Error::missing_field("url"))?;
                let use_h2c = use_h2c.unwrap_or(false);
                // weight=0 は無効なので最低1に補正
                let weight = weight.unwrap_or(1).max(1);
                Ok(UpstreamServerEntry {
                    url,
                    sni_name,
                    use_h2c,
                    weight,
                })
            }
        }

        deserializer.deserialize_any(UpstreamServerEntryVisitor)
    }
}

/// Upstream 設定（ロードバランシング用）
#[derive(Deserialize, Clone, Debug)]
pub struct UpstreamConfig {
    /// ロードバランシングアルゴリズム
    /// - "round_robin": ラウンドロビン（デフォルト）
    /// - "least_conn": Least Connections
    /// - "ip_hash": クライアントIPハッシュ
    #[serde(default)]
    pub algorithm: LoadBalanceAlgorithm,
    /// Consistent Hash 用のハッシュキー（algorithm = "consistent_hash" 時のみ有効）
    /// 省略時は IP ベース
    #[serde(default)]
    pub hash_key: Option<HashKey>,
    /// バックエンドサーバーエントリ一覧
    /// 文字列形式と構造体形式の両方をサポート
    pub servers: Vec<UpstreamServerEntry>,
    /// 健康チェック設定（オプション）
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    /// TLS証明書検証を無効化（自己署名証明書を許可）
    /// デフォルト: false（証明書検証を有効）
    /// 注意: 本番環境では false を推奨
    #[serde(default)]
    pub tls_insecure: bool,
    /// サーキットブレーカー設定（F-06）
    #[serde(default)]
    pub circuit_breaker: CircuitBreakerConfig,
    /// 異常検知（Outlier Detection）設定（F-06）
    #[serde(default)]
    pub outlier_detection: OutlierConfig,
}

/// サーキットブレーカー設定（F-06）
///
/// アップストリームサーバー単位で適用される。連続失敗が閾値を超えると
/// Open 状態へ遷移し、一定時間経過後 HalfOpen でプローブを行う。
#[derive(Deserialize, Clone, Debug)]
pub struct CircuitBreakerConfig {
    /// 有効化フラグ
    #[serde(default)]
    pub enabled: bool,
    /// Open へ遷移する失敗回数の閾値
    #[serde(default = "default_cb_failure_threshold")]
    pub failure_threshold: u32,
    /// 失敗をカウントするスライディングウィンドウ（秒）
    #[serde(default = "default_cb_failure_window")]
    pub failure_window_secs: u64,
    /// Open 状態を維持する時間（秒）
    #[serde(default = "default_cb_open_duration")]
    pub open_duration_secs: u64,
    /// HalfOpen 時に許可するプローブ数
    #[serde(default = "default_cb_half_open_probes")]
    pub half_open_probes: u32,
    /// HalfOpen から Closed へ戻る成功回数の閾値
    #[serde(default = "default_cb_success_threshold")]
    pub success_threshold: u32,
    /// タイムアウトを失敗としてカウントするか
    #[serde(default = "default_true")]
    pub trip_on_timeout: bool,
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            failure_threshold: default_cb_failure_threshold(),
            failure_window_secs: default_cb_failure_window(),
            open_duration_secs: default_cb_open_duration(),
            half_open_probes: default_cb_half_open_probes(),
            success_threshold: default_cb_success_threshold(),
            trip_on_timeout: true,
        }
    }
}

fn default_cb_failure_threshold() -> u32 {
    5
}
fn default_cb_failure_window() -> u64 {
    60
}
fn default_cb_open_duration() -> u64 {
    30
}
fn default_cb_half_open_probes() -> u32 {
    3
}
fn default_cb_success_threshold() -> u32 {
    2
}

/// 異常検知（パッシブ Outlier Detection）設定（F-06）
#[derive(Deserialize, Clone, Debug)]
pub struct OutlierConfig {
    /// 有効化フラグ
    #[serde(default)]
    pub enabled: bool,
    /// エラー率の閾値（0.0-1.0、デフォルト 0.5）
    #[serde(default = "default_outlier_error_rate")]
    pub error_rate_threshold: f64,
    /// 評価間隔（秒）
    #[serde(default = "default_outlier_interval")]
    pub interval_secs: u64,
    /// 基本排除時間（秒）
    #[serde(default = "default_outlier_base_ejection")]
    pub base_ejection_time_secs: u64,
    /// 同時に排除可能なサーバーの最大割合（%）
    #[serde(default = "default_outlier_max_eject_percent")]
    pub max_ejection_percent: u32,
}

impl Default for OutlierConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            error_rate_threshold: default_outlier_error_rate(),
            interval_secs: default_outlier_interval(),
            base_ejection_time_secs: default_outlier_base_ejection(),
            max_ejection_percent: default_outlier_max_eject_percent(),
        }
    }
}

fn default_outlier_error_rate() -> f64 {
    0.5
}
fn default_outlier_interval() -> u64 {
    10
}
fn default_outlier_base_ejection() -> u64 {
    30
}
fn default_outlier_max_eject_percent() -> u32 {
    50
}

// 注: リトライポリシー（RetryPolicy）は F-06 で構造体のみ定義されたが、リトライ機構
// 本体が未実装でどこからも参照されない dead code だったため F-51 で削除した。
// リトライを実装する際は resilience.rs と併せて再設計すること。

/// ヘルスチェックの種別（F-22）
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum HealthCheckType {
    /// HTTP/HTTPS リクエストを送信してステータスコードを確認（デフォルト）
    #[default]
    Http,
    /// TCP 接続の確立可否のみ確認
    Tcp,
    /// gRPC Health Checking Protocol (grpc.health.v1.Health/Check)
    Grpc,
}

/// 健康チェック設定
#[derive(Deserialize, Clone, Debug)]
pub struct HealthCheckConfig {
    /// チェック種別（http / tcp / grpc）。省略時は http。
    #[serde(default)]
    pub check_type: HealthCheckType,
    /// チェック間隔（秒）
    #[serde(default = "default_health_check_interval")]
    pub interval_secs: u64,
    /// チェック対象パス（HTTP チェック時のリクエストパス、gRPC チェック時のサービス名）
    #[serde(default = "default_health_check_path")]
    pub path: String,
    /// タイムアウト（秒）
    #[serde(default = "default_health_check_timeout")]
    pub timeout_secs: u64,
    /// 成功と判断するHTTPステータスコード（HTTP チェック時のみ有効、デフォルト: 200-399）
    #[serde(default = "default_healthy_statuses")]
    pub healthy_statuses: Vec<u16>,
    /// 何回連続で失敗したら unhealthy とするか
    #[serde(default = "default_unhealthy_threshold")]
    pub unhealthy_threshold: u32,
    /// 何回連続で成功したら healthy に戻すか
    #[serde(default = "default_healthy_threshold")]
    pub healthy_threshold: u32,
    /// TLS接続を使用するかどうか
    /// デフォルト: false（既存のHTTP健康チェックを使用）
    #[serde(default)]
    pub use_tls: bool,
    /// 証明書検証を有効化するかどうか（use_tls=true時のみ有効）
    /// デフォルト: true
    #[serde(default = "default_true")]
    pub verify_cert: bool,
}

fn default_health_check_interval() -> u64 {
    10
}
fn default_health_check_path() -> String {
    "/".to_string()
}
fn default_health_check_timeout() -> u64 {
    5
}
fn default_healthy_statuses() -> Vec<u16> {
    vec![200, 201, 202, 204, 301, 302, 304]
}
fn default_unhealthy_threshold() -> u32 {
    3
}
fn default_healthy_threshold() -> u32 {
    2
}

impl Default for HealthCheckConfig {
    fn default() -> Self {
        Self {
            check_type: HealthCheckType::Http,
            interval_secs: default_health_check_interval(),
            path: default_health_check_path(),
            timeout_secs: default_health_check_timeout(),
            healthy_statuses: default_healthy_statuses(),
            unhealthy_threshold: default_unhealthy_threshold(),
            healthy_threshold: default_healthy_threshold(),
            use_tls: false,
            verify_cert: default_true(),
        }
    }
}

// ====================
// 統合ルーティング（AWS ALB準拠）
// ====================

/// ルーティング条件（AWS ALB準拠）
///
/// すべての条件はANDで結合されます。
/// 条件が指定されていない場合は、すべてのリクエストにマッチします（デフォルトルート）。
#[derive(Clone, Debug, Deserialize, Default)]
pub struct RouteConditions {
    /// host-header: ホスト名マッチ（ワイルドカード対応）
    /// 例: "api.example.com", "*.example.com"
    #[serde(default)]
    pub host: Option<String>,

    /// path-pattern: パスマッチ（ワイルドカード対応）
    /// 例: "/api/*", "/static/*", "/api/v2/*"
    #[serde(default)]
    pub path: Option<String>,

    /// http-header: HTTPヘッダーマッチ（複数指定可能）
    /// 例: { "X-Version" = "v2" }
    #[serde(default)]
    pub header: Option<HashMap<String, String>>,

    /// http-request-method: HTTPメソッドマッチ（配列対応）
    /// 例: ["GET", "POST"]
    #[serde(default)]
    pub method: Option<Vec<String>>,

    /// query-string: クエリパラメータマッチ（複数指定可能）
    /// 例: { "key" = "value" }
    #[serde(default)]
    pub query: Option<HashMap<String, String>>,

    /// source-ip: ソースIPマッチ（CIDR表記）
    /// 例: ["192.168.0.0/16", "10.0.0.0/8"]
    #[serde(default)]
    pub source_ip: Option<Vec<String>>,
}

/// ルーティングルール
///
/// 条件に一致するリクエストに対して、指定されたバックエンドアクションを実行します。
#[derive(Clone, Debug, Deserialize)]
pub struct Route {
    /// ルーティング条件（空の場合はデフォルトルート）
    #[serde(default)]
    pub conditions: RouteConditions,

    /// バックエンドアクション
    pub action: BackendConfig,

    /// ルートレベルのセキュリティ設定（actionの設定をオーバーライド）
    #[serde(default)]
    pub security: Option<SecurityConfig>,

    /// ルートレベルの圧縮設定（actionの設定をオーバーライド）
    #[serde(default)]
    pub compression: Option<CompressionConfig>,

    /// ルートレベルのバッファリング設定（actionの設定をオーバーライド）
    #[serde(default)]
    pub buffering: Option<buffering::BufferingConfig>,

    /// ルートレベルのキャッシュ設定（actionの設定をオーバーライド）
    #[serde(default)]
    pub cache: Option<cache::CacheConfig>,

    /// ルートレベルのOpenFileCache設定（actionの設定をオーバーライド、Fileバックエンドのみ）
    #[serde(default)]
    pub open_file_cache: Option<cache::OpenFileCacheConfig>,

    /// ルートレベルのWASMモジュール名のリスト（このルートに適用するWASMモジュール）
    /// 注意: modules は route 直下で設定（action配下の設定は削除）
    #[serde(default)]
    pub modules: Option<Vec<String>>,
}

#[derive(Deserialize)]
struct Config {
    server: ServerConfigSection,
    tls: TlsConfigSection,
    #[serde(default)]
    performance: PerformanceConfigSection,
    /// グローバルセキュリティ設定（権限降格など）
    #[serde(default)]
    security: GlobalSecurityConfig,
    /// ログ設定（非同期ログの最適化）
    #[serde(default)]
    logging: LoggingConfigSection,
    /// Prometheusメトリクス設定
    #[serde(default)]
    prometheus: PrometheusConfig,
    /// 管理 API 設定（F-20: キャッシュ Purge 等）
    #[cfg(feature = "admin")]
    #[serde(default)]
    admin: AdminConfig,
    /// 構造化アクセスログ設定（F-21）
    #[cfg(feature = "access-log")]
    #[serde(default)]
    access_log: crate::access_log::AccessLogConfig,
    /// OpenTelemetry 設定（F-10）
    #[serde(default)]
    opentelemetry: OpenTelemetryConfig,
    /// バッファプール設定（メモリ最適化）
    #[serde(default)]
    buffer_pool: BufferPoolConfig,
    /// HTTP/2 設定セクション
    #[serde(default)]
    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    http2: Http2ConfigSection,
    /// HTTP/3 設定セクション
    #[serde(default)]
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    http3: Http3ConfigSection,
    /// Upstream グループ定義（ロードバランシング用）
    #[serde(default)]
    upstreams: Option<HashMap<String, UpstreamConfig>>,
    /// 統合ルーティング（唯一のルーティング方式）
    /// 配列の順序で評価（first-match方式）
    #[serde(default)]
    route: Option<Vec<Route>>,
    /// WASM拡張設定（feature flagで条件付きコンパイル）
    #[cfg(feature = "wasm")]
    #[serde(default)]
    wasm: Option<crate::wasm::WasmConfig>,
    /// L4 (TCP/UDP) ストリームプロキシ設定（F-18）
    #[cfg(feature = "l4-proxy")]
    #[serde(default)]
    l4: Option<Vec<L4ListenerConfig>>,
}

// ====================
// L4 (TCP/UDP) ストリームプロキシ設定（F-18）
// ====================

/// L4 TLS モード
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum L4TlsMode {
    /// TLS なし（プレーンな TCP）
    #[default]
    None,
    /// TLS パススルー（TLS を復号せず upstream にそのまま転送）
    Passthrough,
    /// TLS ターミネーション（veil で TLS を終端し、upstream にはプレーン TCP で接続）
    Terminate,
}

/// L4 ロードバランシングアルゴリズム
#[derive(Deserialize, Clone, Debug, PartialEq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum L4LbAlgorithm {
    /// ラウンドロビン（デフォルト）
    #[default]
    RoundRobin,
    /// 最小接続数
    LeastConn,
}

/// L4 upstream バックエンド
#[derive(Deserialize, Clone, Debug)]
pub struct L4UpstreamEntry {
    /// バックエンドアドレス（"host:port" 形式）
    pub addr: String,
    /// 重み（weighted round-robin 用、デフォルト: 1）
    #[serde(default = "default_l4_weight")]
    pub weight: u32,
}

fn default_l4_weight() -> u32 {
    1
}

/// L4 リスナー設定（TOML: [[l4]]）
#[derive(Deserialize, Clone, Debug)]
pub struct L4ListenerConfig {
    /// リスナー名（ログ・メトリクス識別用）
    pub name: String,
    /// リッスンアドレス（例: "0.0.0.0:3306"）
    pub listen: String,
    /// upstream バックエンド一覧
    pub upstreams: Vec<L4UpstreamEntry>,
    /// ロードバランシングアルゴリズム
    #[serde(default)]
    pub lb: L4LbAlgorithm,
    /// TLS モード（none / passthrough / terminate）
    #[serde(default)]
    pub tls: L4TlsMode,
    /// 最大同時接続数（0 = 無制限）
    #[serde(default)]
    pub max_connections: usize,
    /// ヘルスチェック設定（省略可）
    #[serde(default)]
    pub health_check: Option<HealthCheckConfig>,
    /// バックエンド接続タイムアウト（秒）
    #[serde(default = "default_l4_connect_timeout")]
    pub connect_timeout_secs: u64,
    /// アイドルタイムアウト（秒）: この時間データ転送がなければ接続を切断（デフォルト: 600）
    #[serde(default = "default_l4_idle_timeout")]
    pub idle_timeout_secs: u64,
}

fn default_l4_connect_timeout() -> u64 {
    10
}

fn default_l4_idle_timeout() -> u64 {
    600
}

// ====================
// HTTP/2 設定セクション (RFC 7540)
// ====================

/// HTTP/2 詳細設定
///
/// HTTP/2 プロトコルのパラメータを設定します。
/// 有効化は `server.http2_enabled` で行います。
#[derive(Deserialize, Clone)]
pub struct Http2ConfigSection {
    /// SETTINGS_HEADER_TABLE_SIZE (HPACK動的テーブルサイズ)
    /// デフォルト: 4096 (4KB)
    /// 高パフォーマンス: 65536 (64KB)
    #[serde(default = "default_h2_header_table_size")]
    pub header_table_size: u32,

    /// SETTINGS_MAX_CONCURRENT_STREAMS (同時ストリーム数)
    /// デフォルト: 100
    #[serde(default = "default_h2_max_concurrent_streams")]
    pub max_concurrent_streams: u32,

    /// SETTINGS_INITIAL_WINDOW_SIZE (ストリームウィンドウサイズ)
    /// デフォルト: 65535 (64KB - 1)
    #[serde(default = "default_h2_initial_window_size")]
    pub initial_window_size: u32,

    /// SETTINGS_MAX_FRAME_SIZE (最大フレームサイズ)
    /// デフォルト: 65536 (64KB)。これは **受信側の上限として相手へ広告する値** であり、
    /// 送信フレームは相手の SETTINGS_MAX_FRAME_SIZE（既定 16384）に従って分割される。
    #[serde(default = "default_h2_max_frame_size")]
    pub max_frame_size: u32,

    /// SETTINGS_MAX_HEADER_LIST_SIZE (最大ヘッダーリストサイズ)
    /// デフォルト: 16384 (16KB)
    #[serde(default = "default_h2_max_header_list_size")]
    pub max_header_list_size: u32,

    /// コネクションウィンドウサイズ（コネクション全体のフロー制御）
    /// デフォルト: 65535 (64KB - 1)
    #[serde(default = "default_h2_connection_window_size")]
    pub connection_window_size: u32,

    // ====================
    // DoS 対策設定
    // ====================
    /// RST_STREAM レート制限 (1秒あたりの最大数)
    /// Rapid Reset 対策 (CVE-2023-44487)
    /// デフォルト: 100
    #[serde(default = "default_h2_max_rst_stream_per_second")]
    pub max_rst_stream_per_second: u32,

    /// 制御フレームレート制限 (1秒あたりの最大数)
    /// PING/SETTINGS フラッド対策
    /// デフォルト: 500
    #[serde(default = "default_h2_max_control_frames_per_second")]
    pub max_control_frames_per_second: u32,

    /// CONTINUATION フレーム制限 (ヘッダーブロックあたりの最大数)
    /// CONTINUATION Flood 対策 (CVE-2024-24786)
    /// デフォルト: 10
    #[serde(default = "default_h2_max_continuation_frames")]
    pub max_continuation_frames: u32,

    /// 最大ヘッダーブロックサイズ (bytes)
    /// HPACK Bomb 対策
    /// デフォルト: 65536 (64KB)
    #[serde(default = "default_h2_max_header_block_size")]
    pub max_header_block_size: usize,

    /// ストリームアイドルタイムアウト (秒)
    /// Slow Loris 対策
    /// デフォルト: 60
    #[serde(default = "default_h2_stream_idle_timeout_secs")]
    pub stream_idle_timeout_secs: u64,
}

// HTTP/2 設定のデフォルト値（high_performance と同等）
// HPACK動的テーブルサイズを大きくすることで、ヘッダー圧縮効率が向上
fn default_h2_header_table_size() -> u32 {
    65536
} // 64KB (より多くのヘッダーをキャッシュ)
fn default_h2_max_concurrent_streams() -> u32 {
    256
} // より多くの同時ストリーム
fn default_h2_initial_window_size() -> u32 {
    1048576
} // 1MB (より大きなウィンドウ)
fn default_h2_max_frame_size() -> u32 {
    65536
} // 64KB (より大きなフレーム)
fn default_h2_max_header_list_size() -> u32 {
    65536
} // 64KB
fn default_h2_connection_window_size() -> u32 {
    1048576
} // 1MB

// DoS 対策のデフォルト値
fn default_h2_max_rst_stream_per_second() -> u32 {
    100
}
fn default_h2_max_control_frames_per_second() -> u32 {
    500
}
fn default_h2_max_continuation_frames() -> u32 {
    10
}
fn default_h2_max_header_block_size() -> usize {
    65536
}
fn default_h2_stream_idle_timeout_secs() -> u64 {
    60
}

impl Default for Http2ConfigSection {
    fn default() -> Self {
        Self {
            header_table_size: default_h2_header_table_size(),
            max_concurrent_streams: default_h2_max_concurrent_streams(),
            initial_window_size: default_h2_initial_window_size(),
            max_frame_size: default_h2_max_frame_size(),
            max_header_list_size: default_h2_max_header_list_size(),
            connection_window_size: default_h2_connection_window_size(),
            // DoS 対策
            max_rst_stream_per_second: default_h2_max_rst_stream_per_second(),
            max_control_frames_per_second: default_h2_max_control_frames_per_second(),
            max_continuation_frames: default_h2_max_continuation_frames(),
            max_header_block_size: default_h2_max_header_block_size(),
            stream_idle_timeout_secs: default_h2_stream_idle_timeout_secs(),
        }
    }
}

impl Http2ConfigSection {
    /// HTTP/2 設定を Http2Settings に変換
    #[cfg(feature = "http2")]
    pub fn to_http2_settings(&self) -> http2::Http2Settings {
        http2::Http2Settings {
            header_table_size: self.header_table_size,
            max_concurrent_streams: self.max_concurrent_streams,
            initial_window_size: self.initial_window_size,
            max_frame_size: self.max_frame_size,
            max_header_list_size: self.max_header_list_size,
            enable_push: false, // サーバーではpush無効
            connection_window_size: self.connection_window_size,
            // DoS 対策
            max_rst_stream_per_second: self.max_rst_stream_per_second,
            max_control_frames_per_second: self.max_control_frames_per_second,
            max_continuation_frames: self.max_continuation_frames,
            max_header_block_size: self.max_header_block_size,
            stream_idle_timeout_secs: self.stream_idle_timeout_secs,
        }
    }
}

// ====================
// HTTP/3 設定セクション (RFC 9114, QUIC RFC 9000)
// ====================

/// HTTP/3 専用圧縮設定
///
/// HTTP/3接続時に使用する圧縮パラメータを設定します。
/// 未設定のフィールドはパスごとの設定または全体設定を継承します。
#[derive(Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Http3CompressionConfig {
    /// 圧縮方式の優先順位
    /// サポート: "zstd", "br" (Brotli), "gzip", "deflate"
    /// 未設定時はパスごとの設定を使用
    pub preferred_encodings: Option<Vec<String>>,

    /// Gzip圧縮レベル (1-9)
    /// 未設定時はパスごとの設定を使用
    pub gzip_level: Option<u32>,

    /// Brotli圧縮レベル (0-11)
    /// 未設定時はパスごとの設定を使用
    pub brotli_level: Option<u32>,

    /// Zstd圧縮レベル (1-22)
    /// 未設定時はパスごとの設定を使用
    pub zstd_level: Option<i32>,

    /// 最小圧縮サイズ（バイト）
    /// 未設定時はパスごとの設定を使用
    pub min_size: Option<usize>,

    /// 圧縮対象のMIMEタイプ（プレフィックスマッチ）
    /// 未設定時はパスごとの設定を使用
    pub compressible_types: Option<Vec<String>>,

    /// 圧縮をスキップするMIMEタイプ（プレフィックスマッチ）
    /// 未設定時はパスごとの設定を使用
    pub skip_types: Option<Vec<String>>,
}

impl Http3CompressionConfig {
    /// 設定の妥当性を検証
    pub fn validate(&self) -> Result<(), String> {
        if let Some(level) = self.gzip_level {
            if !(1..=9).contains(&level) {
                return Err(format!(
                    "http3.compression.gzip_level: {} (must be 1-9)",
                    level
                ));
            }
        }
        if let Some(level) = self.brotli_level {
            if level > 11 {
                return Err(format!(
                    "http3.compression.brotli_level: {} (must be 0-11)",
                    level
                ));
            }
        }
        if let Some(level) = self.zstd_level {
            if !(1..=22).contains(&level) {
                return Err(format!(
                    "http3.compression.zstd_level: {} (must be 1-22)",
                    level
                ));
            }
        }
        if let Some(ref encodings) = self.preferred_encodings {
            for enc in encodings {
                if !["zstd", "br", "gzip", "deflate"].contains(&enc.as_str()) {
                    return Err(format!(
                        "http3.compression.preferred_encodings: unknown encoding '{}'",
                        enc
                    ));
                }
            }
        }
        Ok(())
    }
}

/// HTTP/3 詳細設定
///
/// HTTP/3 (QUIC) プロトコルのパラメータを設定します。
/// 有効化は `server.http3_enabled` で行います。
#[derive(Deserialize, Clone)]
pub struct Http3ConfigSection {
    /// HTTP/3リッスンアドレス（UDP）
    /// 未指定の場合は server.listen と同じアドレスを使用
    #[serde(default)]
    pub listen: Option<String>,

    /// 最大アイドルタイムアウト（ミリ秒）
    /// デフォルト: 30000 (30秒)
    #[serde(default = "default_h3_max_idle_timeout")]
    pub max_idle_timeout: u64,

    /// 最大UDPペイロードサイズ
    /// デフォルト: 1350 (MTU考慮)
    #[serde(default = "default_h3_max_udp_payload_size")]
    pub max_udp_payload_size: u64,

    /// 初期最大データサイズ（コネクション全体）
    /// デフォルト: 10000000 (10MB)
    #[serde(default = "default_h3_initial_max_data")]
    pub initial_max_data: u64,

    /// 初期最大ストリームデータサイズ（双方向ローカル）
    #[serde(default = "default_h3_initial_max_stream_data")]
    pub initial_max_stream_data_bidi_local: u64,

    /// 初期最大ストリームデータサイズ（双方向リモート）
    #[serde(default = "default_h3_initial_max_stream_data")]
    pub initial_max_stream_data_bidi_remote: u64,

    /// 初期最大ストリームデータサイズ（単方向）
    #[serde(default = "default_h3_initial_max_stream_data")]
    pub initial_max_stream_data_uni: u64,

    /// 初期最大双方向ストリーム数
    #[serde(default = "default_h3_max_streams")]
    pub initial_max_streams_bidi: u64,

    /// 初期最大単方向ストリーム数
    #[serde(default = "default_h3_max_streams")]
    pub initial_max_streams_uni: u64,

    /// HTTP/3接続時の圧縮を常に有効化
    /// デフォルト: false
    ///
    /// true の場合、パスごとの設定で明示的に無効化されていない限り、
    /// すべてのHTTP/3レスポンスで圧縮を試みます。
    /// パスごとの compression.enabled = false の場合はそちらが優先されます。
    #[serde(default)]
    pub compression_enabled: bool,

    /// HTTP/3専用の圧縮パラメータ
    ///
    /// パスごとの圧縮設定より優先されます。
    /// 未設定のフィールドはパスごとの設定を継承します。
    #[serde(default)]
    pub compression: Http3CompressionConfig,

    /// GSO/GRO を有効化するかどうか
    ///
    /// GSO (Generic Segmentation Offload) / GRO (Generic Receive Offload) は
    /// カーネルレベルでUDPパケットの送受信を効率化する機能です。
    ///
    /// 効果:
    /// - 複数の小さなUDPパケットを一度に送受信
    /// - システムコール回数の削減
    /// - CPU使用率の低減
    ///
    /// 注意:
    /// - Linux 5.0+ でサポート
    /// - 一部の仮想環境やDockerでは期待通りに動作しない場合あり
    /// - 問題が発生した場合は false に設定してください
    ///
    /// デフォルト: false
    #[serde(default)]
    pub gso_gro_enabled: bool,

    // ====================
    // Alt-Svc（HTTP/3 広告、F-94）— すべて [http3] に集約
    // ====================
    /// HTTP/1.1・HTTP/2 応答へ `Alt-Svc` ヘッダーを付与するか
    ///
    /// デフォルト: `true`（`server.http3_enabled = true` のときのみ実際に広告される）
    /// `false` にすると広告を抑制する。
    #[serde(default = "default_true")]
    pub alt_svc_enabled: bool,

    /// Alt-Svc ヘッダー値の上書き
    ///
    /// 未指定時はリッスンポートから `h3=":PORT"; ma=86400` を自動生成する。
    /// 明示指定例: `h3=":443"; ma=86400, h3=":443"; ma=2592000`
    #[serde(default)]
    pub alt_svc: Option<String>,

    /// Alt-Svc の max-age（秒）。`alt_svc` 未指定時の自動生成に使用。デフォルト: 86400
    #[serde(default = "default_h3_alt_svc_ma")]
    pub alt_svc_ma_secs: u64,
}

fn default_h3_max_idle_timeout() -> u64 {
    30000
}
fn default_h3_max_udp_payload_size() -> u64 {
    1350
}
fn default_h3_initial_max_data() -> u64 {
    10_000_000
}
fn default_h3_initial_max_stream_data() -> u64 {
    1_000_000
}
fn default_h3_max_streams() -> u64 {
    100
}
fn default_h3_alt_svc_ma() -> u64 {
    86400
}

impl Default for Http3ConfigSection {
    fn default() -> Self {
        Self {
            listen: None,
            max_idle_timeout: default_h3_max_idle_timeout(),
            max_udp_payload_size: default_h3_max_udp_payload_size(),
            initial_max_data: default_h3_initial_max_data(),
            initial_max_stream_data_bidi_local: default_h3_initial_max_stream_data(),
            initial_max_stream_data_bidi_remote: default_h3_initial_max_stream_data(),
            initial_max_stream_data_uni: default_h3_initial_max_stream_data(),
            initial_max_streams_bidi: default_h3_max_streams(),
            initial_max_streams_uni: default_h3_max_streams(),
            compression_enabled: false,
            compression: Http3CompressionConfig::default(),
            gso_gro_enabled: false,
            alt_svc_enabled: true,
            alt_svc: None,
            alt_svc_ma_secs: default_h3_alt_svc_ma(),
        }
    }
}

#[cfg(feature = "http3")]
impl Http3ConfigSection {
    /// HTTP/3 設定を Http3ServerConfig に変換
    pub fn to_http3_config(
        &self,
        cert_path: &str,
        key_path: &str,
    ) -> http3_server::Http3ServerConfig {
        http3_server::Http3ServerConfig {
            cert_path: cert_path.to_string(),
            key_path: key_path.to_string(),
            cert_pem: None, // quicheはファイルパスからの読み込みのみサポート
            key_pem: None,
            max_idle_timeout: self.max_idle_timeout,
            max_udp_payload_size: self.max_udp_payload_size,
            initial_max_data: self.initial_max_data,
            initial_max_stream_data_bidi_local: self.initial_max_stream_data_bidi_local,
            initial_max_stream_data_bidi_remote: self.initial_max_stream_data_bidi_remote,
            initial_max_stream_data_uni: self.initial_max_stream_data_uni,
            initial_max_streams_bidi: self.initial_max_streams_bidi,
            initial_max_streams_uni: self.initial_max_streams_uni,
            gso_gro_enabled: self.gso_gro_enabled,
        }
    }
}

#[derive(Deserialize)]
pub struct ServerConfigSection {
    pub listen: String,
    /// HTTPリスナーアドレス（オプション）
    ///
    /// 指定した場合、HTTPアクセスをHTTPSにリダイレクトするリスナーを起動します。
    /// 例: "0.0.0.0:80"
    ///
    /// リダイレクトのみを行い、コンテンツは配信しません（セキュリティ考慮）。
    #[serde(default)]
    pub http: Option<String>,
    /// ワーカースレッド数
    /// 未指定または0の場合はCPUコア数と同じスレッド数を使用
    #[serde(default)]
    pub threads: Option<usize>,

    // ====================
    // Serverヘッダー設定
    // ====================
    /// Serverヘッダーを有効化（デフォルト: false）
    ///
    /// セキュリティ考慮事項:
    /// - Serverヘッダーはサーバーソフトウェア情報を公開します
    /// - 攻撃者がバージョン別の脆弱性を狙う手がかりになり得ます
    /// - 本番環境では無効化を推奨
    #[serde(default)]
    pub server_header_enabled: bool,

    /// Serverヘッダーの値（server_header_enabled = true時のみ有効）
    ///
    /// デフォルト: "veil"
    /// 空文字の場合: ヘッダー自体を送信しない
    #[serde(default = "default_server_header_value")]
    pub server_header_value: String,

    // ====================
    // HTTP/2・HTTP/3 設定
    // ====================
    /// HTTP/2 を有効化するかどうか
    ///
    /// TLS ALPN ネゴシエーションにより HTTP/2 (h2) をサポートします。
    /// HTTP/1.1 へのフォールバックも可能です。
    ///
    /// 効果:
    /// - ストリーム多重化によるレイテンシ削減
    /// - HPACK ヘッダー圧縮によるオーバーヘッド削減
    /// - サーバープッシュ（無効化推奨）
    ///
    /// 注意: `--features http2` でビルドする必要があります
    #[serde(default)]
    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    pub http2_enabled: bool,

    /// HTTP/3 を有効化するかどうか
    ///
    /// QUIC/UDP ベースの HTTP/3 プロトコルをサポートします。
    ///
    /// 効果:
    /// - 0-RTT 接続確立
    /// - 接続マイグレーション
    /// - Head-of-Line ブロッキング解消
    ///
    /// 注意:
    /// - `--features http3` でビルドする必要があります
    /// - HTTP/3 は UDP ベースのため kTLS は使用不可
    /// - GSO/GRO による高パフォーマンス UDP 処理を使用
    /// - リッスンアドレスは [http3].listen で設定
    #[serde(default)]
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    pub http3_enabled: bool,

    // ====================
    // H2C (HTTP/2 Cleartext) 設定
    // ====================
    /// H2C (HTTP/2 Cleartext) サーバーを有効化するかどうか
    ///
    /// TLSなしでHTTP/2を使用するプロトコルです（RFC 7540 Section 3.4）。
    /// Prior Knowledgeモードをサポートします。
    ///
    /// 効果:
    /// - ストリーム多重化によるレイテンシ削減
    /// - HPACK ヘッダー圧縮によるオーバーヘッド削減
    /// - gRPC バックエンドへの接続に適している
    ///
    /// セキュリティ考慮事項:
    /// - H2Cは平文通信のため、本番環境では内部ネットワークでのみ使用を推奨
    /// - 外部公開時はTLS必須
    /// - デフォルトで無効（明示的に有効化が必要）
    ///
    /// 注意: `--features http2` でビルドする必要があります
    #[serde(default)]
    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    pub h2c_enabled: bool,

    /// H2C リスニングアドレス（オプション）
    ///
    /// 指定した場合、H2C専用のリスナーを起動します。
    /// 未指定の場合は、TLSリスナーと同じアドレスでプロトコル検出を行います。
    ///
    /// 例: "0.0.0.0:8080"
    ///
    /// 注意: 同じポートでTLSとH2Cの両方を処理する場合は未指定にしてください。
    #[serde(default)]
    #[cfg_attr(not(feature = "http2"), allow(dead_code))]
    pub h2c_listen: Option<String>,

    // ====================
    // グレースフルシャットダウン設定
    // ====================
    /// グレースフルシャットダウンのドレインタイムアウト（秒）
    ///
    /// SIGTERMまたはSIGINT受信時に、既存の接続が完了するまで待機する最大時間。
    /// タイムアウト後は残りの接続を強制終了します。
    ///
    /// デフォルト: 30秒
    /// 0: 待機せずに即座に終了（既存の動作）
    #[serde(default = "default_graceful_shutdown_timeout")]
    pub graceful_shutdown_timeout_secs: u64,
}

/// グレースフルシャットダウンタイムアウトのデフォルト値（30秒）
fn default_graceful_shutdown_timeout() -> u64 {
    30
}

/// Serverヘッダーのデフォルト値
fn default_server_header_value() -> String {
    "veil".to_string()
}

#[derive(Deserialize)]
pub struct TlsConfigSection {
    pub cert_path: String,
    pub key_path: String,
    /// kTLSを有効化するかどうか（Linux 5.15+、modprobe tls 必須）
    ///
    /// kTLS有効化時の効果:
    /// - TLSデータ転送フェーズでカーネルオフロード
    /// - sendfileでゼロコピー送信（TLS暗号化済み）
    /// - 高負荷時にCPU 20-40%節約、スループット最大2倍
    ///
    /// 注意事項:
    /// - TLSハンドシェイクはrustlsで実行（セキュリティ維持）
    /// - AES-GCM暗号スイートのみサポート
    /// - カーネルバグの影響範囲に注意
    #[serde(default)]
    pub ktls_enabled: bool,
    /// kTLS有効化失敗時にrustlsへフォールバックするかどうか
    ///
    /// - false: kTLS必須モード（失敗時は接続拒否）
    /// - true: kTLS失敗時はrustlsで継続（デフォルト）
    ///
    /// フォールバック無効化のメリット:
    /// - パフォーマンス予測可能性（確実にkTLSを使用）
    /// - デバッグ容易性（kTLS/rustls混在なし）
    /// - 環境問題の早期発見
    #[serde(default = "default_ktls_fallback")]
    pub ktls_fallback_enabled: bool,
    /// kTLS有効時にTCP_CORKを使用するかどうか
    ///
    /// TCP_CORKはkTLS設定中に小さなTCPパケットの送信を遅延し、
    /// パケット結合により効率的なネットワーク転送を実現します。
    ///
    /// - true: TCP_CORK有効（デフォルト、推奨）
    /// - false: TCP_CORK無効（特定の低遅延要件がある場合）
    #[serde(default = "default_tcp_cork")]
    pub tcp_cork_enabled: bool,
    /// 利用を許可する TLS 暗号スイート（nginx の `ssl_ciphers` 相当、F-50）
    ///
    /// **記載順 = サーバ優先度順**（rustls はサーバ選好でネゴシエートする）。
    /// 未指定（空）の場合は rustls 既定（DEFAULT_CIPHER_SUITES）を使用する。
    /// 不正なスイート名は起動時（設定検証時）にエラーとなる。
    ///
    /// 指定可能な名前（aws-lc-rs プロバイダ）:
    /// - TLS 1.3: `TLS13_AES_256_GCM_SHA384`, `TLS13_AES_128_GCM_SHA256`,
    ///   `TLS13_CHACHA20_POLY1305_SHA256`
    /// - TLS 1.2: `TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384`,
    ///   `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256`,
    ///   `TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256`,
    ///   `TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384`,
    ///   `TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256`,
    ///   `TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256`
    ///
    /// 注意: kTLS は AES-GCM 系のみ対応。kTLS 有効時に CHACHA20 系を含めると、
    /// そのスイートでネゴシエートした接続は kTLS へオフロードされず rustls で
    /// 処理される（`ktls_fallback_enabled = false` の場合は接続拒否）。
    #[serde(default)]
    pub cipher_suites: Vec<String>,
    /// 証明書ファイルの変更を自動検知してリロードするか（F-03）
    /// デフォルト: false
    #[serde(default)]
    pub auto_reload: bool,
    /// 証明書変更チェックの間隔（秒、F-03）
    /// デフォルト: 60
    #[serde(default = "default_tls_reload_interval")]
    pub reload_interval_secs: u64,
}

/// 証明書リロードチェック間隔のデフォルト値（秒）
fn default_tls_reload_interval() -> u64 {
    60
}

/// kTLSフォールバックのデフォルト値（true = フォールバック有効）
fn default_ktls_fallback() -> bool {
    true
}

/// TCP_CORKのデフォルト値（true = 有効）
fn default_tcp_cork() -> bool {
    true
}

/// パフォーマンス設定
#[derive(Deserialize, Clone, Default)]
pub struct PerformanceConfigSection {
    /// SO_REUSEPORTの振り分け方式
    /// - "kernel": カーネルデフォルト（3元タプルハッシュ）
    /// - "cbpf": クライアントIPベースのCBPF振り分け（Linux 4.6+必須）
    #[serde(default)]
    pub reuseport_balancing: ReuseportBalancing,
    /// Huge Pages (Large OS Pages) の使用
    ///
    /// mimallocでHuge Pages（2MB）を優先使用し、TLBミスを削減します。
    ///
    /// 効果:
    /// - TLB（Translation Lookaside Buffer）ミス削減
    /// - 大容量メモリ使用時のページフォルト減少
    /// - kTLS/splice時のカーネル連携で5-10%パフォーマンス向上
    ///
    /// 要件（Linux）:
    /// - /proc/sys/vm/nr_hugepages に十分な値を設定
    /// - コンテナ環境では追加設定が必要な場合あり
    #[serde(default)]
    pub huge_pages_enabled: bool,

    // ====================
    // Viaヘッダー設定
    // ====================
    /// Viaヘッダーを追加するかどうか
    ///
    /// RFC 7230 Section 5.7.1 に従い、プロキシ経由のリクエスト/レスポンスに
    /// Via ヘッダーを追加します。
    ///
    /// 形式: Via: 1.1 <hostname>
    ///
    /// - true: Viaヘッダーを追加
    /// - false: Viaヘッダーを追加しない（デフォルト）
    #[serde(default)]
    pub via_header_enabled: bool,

    /// Viaヘッダーに使用するホスト名
    ///
    /// via_header_enabled = true 時に使用します。
    /// 未指定の場合はシステムホスト名または "veil" を使用します。
    #[serde(default)]
    pub via_header_hostname: Option<String>,

    // ====================
    // sendfile/splice チャンクサイズ設定
    // ====================
    /// チャンクサイズ調整モード
    ///
    /// - "dynamic": ファイルサイズに応じて動的にチャンクサイズを決定（デフォルト）
    ///   - 0-64KB: 64KB
    ///   - 64KB-1MB: 256KB
    ///   - 1MB超: 1MB
    /// - "manual": 固定チャンクサイズを使用
    #[serde(default = "default_chunk_size_mode")]
    #[cfg_attr(not(veil_ktls), allow(dead_code))]
    pub chunk_size_mode: ChunkSizeMode,

    /// 手動チャンクサイズ（バイト）
    ///
    /// chunk_size_mode = "manual" 時に使用します。
    /// デフォルト: 1048576 (1MB)
    #[serde(default = "default_manual_chunk_size")]
    #[cfg_attr(not(veil_ktls), allow(dead_code))]
    pub manual_chunk_size: usize,

    // ====================
    // パイプ割当設定
    // ====================
    /// ストリーム毎にパイプを割り当てるかどうか
    ///
    /// kTLS splice 使用時のパイプバッファ管理方式を設定します。
    ///
    /// - false: スレッドローカルパイプを再利用（デフォルト、メモリ効率重視）
    /// - true: ストリーム毎にパイプを割り当て（高並行性環境向け）
    ///
    /// 高並行性環境（同時接続数1000+）ではtrueを推奨します。
    #[serde(default)]
    #[cfg_attr(not(veil_ktls), allow(dead_code))]
    pub per_stream_pipe_enabled: bool,

    // ====================
    // OpenFileCache設定
    // ====================
    /// OpenFileCache（ファイルメタデータキャッシュ）のグローバルデフォルト設定
    ///
    /// 効果:
    ///   - canonicalize、metadata、mime_guessのシステムコールをキャッシュ
    ///   - 1リクエストあたり5〜6回のシステムコールを2回に削減（キャッシュヒット時）
    ///   - パフォーマンス向上: 60〜67%のシステムコール削減
    ///
    /// 注意事項:
    ///   - ファイル変更の検出が最大60秒（デフォルト）遅延する可能性
    ///   - シンボリックリンク変更の検出が遅延する可能性
    ///   - 静的ファイル配信に最適（動的に変更されるファイルには不向き）
    ///
    /// ルーティングごとの設定:
    ///   - 各ルーティング（[[route]]）で`open_file_cache`セクションを指定可能
    ///   - ルーティング設定がない場合は、このグローバル設定が使用される
    ///
    /// デフォルト: false（無効）
    #[serde(default)]
    pub open_file_cache_enabled: bool,

    /// OpenFileCacheの有効期間（秒、グローバルデフォルト）
    /// キャッシュされたファイル情報が有効とみなされる期間
    /// デフォルト: 60秒
    #[serde(default = "default_open_file_cache_valid_duration")]
    pub open_file_cache_valid_duration_secs: u64,

    /// OpenFileCacheの最大エントリ数（グローバルデフォルト）
    /// キャッシュに保持する最大ファイル情報数
    /// デフォルト: 10000
    #[serde(default = "default_open_file_cache_max_entries")]
    pub open_file_cache_max_entries: usize,
}

fn default_open_file_cache_valid_duration() -> u64 {
    60
}

fn default_open_file_cache_max_entries() -> usize {
    10000
}

// ====================
// ログ設定
// ====================
//
// ftlogは内部でバックグラウンドスレッドとチャネルを使用した
// 非同期ログライブラリです。以下の設定で最適化が可能です。
//
// ## grokの指摘に対する検証結果
//
// grokは「ftlogは同期ログ」と主張していましたが、これは不正確です。
// ftlogは以下の非同期アーキテクチャを使用しています：
// - ログマクロ → 内部チャネルにプッシュ（ノンブロッキング）
// - バックグラウンドスレッド → チャネルから読み取りファイルI/O
//
// したがって、tokio::sync::mpscを使った追加の非同期化層は不要であり、
// むしろオーバーヘッドを増やす可能性があります。
//
// ## 推奨される最適化
// - channel_size: 高負荷時のバックプレッシャーを軽減
// - flush_interval_ms: ディスクI/O頻度の調整
// - level: 本番環境ではinfo以上を推奨
// ====================

// LogFormat, LoggingConfigSection, parse_log_level は
// crate::logging モジュールに移動しました。

/// SO_REUSEPORTの振り分け方式
#[derive(Default, Clone, Copy, Debug, PartialEq)]
pub enum ReuseportBalancing {
    /// カーネルデフォルト（3元タプルハッシュ: protocol + source IP + source port）
    #[default]
    Kernel,
    /// クライアントIPベースのCBPF振り分け
    /// 同一クライアントIPからの接続を常に同じワーカースレッドに振り分け
    /// CPUキャッシュ効率とセッション再開効率を向上
    Cbpf,
}

impl<'de> serde::Deserialize<'de> for ReuseportBalancing {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "kernel" => Ok(ReuseportBalancing::Kernel),
            "cbpf" => Ok(ReuseportBalancing::Cbpf),
            other => Err(serde::de::Error::custom(format!(
                "unknown reuseport_balancing value: '{}', expected 'kernel' or 'cbpf'",
                other
            ))),
        }
    }
}

/// sendfile/splice チャンクサイズ調整モード
#[derive(Clone, Copy, Debug, PartialEq, Default)]
pub enum ChunkSizeMode {
    /// ファイルサイズに応じて動的にチャンクサイズを決定
    /// - 0-64KB: 64KB
    /// - 64KB-1MB: 256KB
    /// - 1MB超: 1MB
    #[default]
    Dynamic,
    /// 固定チャンクサイズを使用（manual_chunk_sizeで指定）
    Manual,
}

impl<'de> serde::Deserialize<'de> for ChunkSizeMode {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "dynamic" | "Dynamic" => Ok(ChunkSizeMode::Dynamic),
            "manual" | "Manual" => Ok(ChunkSizeMode::Manual),
            other => Err(serde::de::Error::custom(format!(
                "unknown chunk_size_mode value: '{}', expected 'dynamic' or 'manual'",
                other
            ))),
        }
    }
}

/// チャンクサイズモードのデフォルト値（動的調整）
fn default_chunk_size_mode() -> ChunkSizeMode {
    ChunkSizeMode::Dynamic
}

/// 手動チャンクサイズのデフォルト値（1MB）
fn default_manual_chunk_size() -> usize {
    1048576 // 1MB
}

/// ファイルサイズに応じた最適なチャンクサイズを計算
///
/// 動的調整モード時に使用されます。
pub fn calculate_optimal_chunk_size(file_size: u64) -> usize {
    match file_size {
        0..=65_536 => 65_536,          // 64KB以下: 64KB
        65_537..=1_048_576 => 262_144, // 1MB以下: 256KB
        _ => 1_048_576,                // 1MB超: 1MB
    }
}

#[derive(Clone, Debug)]
pub enum BackendConfig {
    /// 単一URLプロキシ（後方互換性のため維持）
    /// - sni_name: TLS接続時のSNI名（IP直打ち時にドメイン名を指定可能）
    /// - use_h2c: H2C (HTTP/2 over cleartext) を使用するかどうか
    ///
    /// 注意: security, compression, buffering, cache, modules は route 直下で設定
    Proxy {
        url: String,
        sni_name: Option<String>,
        use_h2c: bool,
    },
    /// Upstream グループ参照（ロードバランシング用）
    /// 注意: security, compression, buffering, cache, modules は route 直下で設定
    ProxyUpstream { upstream: String, use_h2c: bool },
    /// File バックエンド設定
    /// - path: ファイルまたはディレクトリのパス
    /// - mode: "sendfile" または "memory"
    /// - index: ディレクトリアクセス時に返すファイル名（デフォルト: "index.html"）
    ///
    /// 注意: security, cache, open_file_cache, modules は route 直下で設定
    File {
        path: String,
        mode: String,
        index: Option<String>,
    },
    /// Redirect バックエンド設定
    /// - redirect_url: リダイレクト先URL（$request_uri, $host, $path 変数使用可能）
    /// - redirect_status: ステータスコード（301, 302, 307, 308）
    /// - preserve_path: 元のパスをリダイレクト先に追加するか
    ///
    /// 注意: modules は route 直下で設定
    Redirect {
        redirect_url: String,
        redirect_status: u16,
        preserve_path: bool,
    },
}

impl<'de> serde::Deserialize<'de> for BackendConfig {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        use serde::de::{MapAccess, Visitor};

        struct BackendConfigVisitor;

        impl<'de> Visitor<'de> for BackendConfigVisitor {
            type Value = BackendConfig;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a backend configuration object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut backend_type: Option<String> = None;
                let mut url: Option<String> = None;
                let mut upstream: Option<String> = None;
                let mut path: Option<String> = None;
                let mut mode: Option<String> = None;
                let mut index: Option<String> = None;
                // Redirect 用フィールド
                let mut redirect_url: Option<String> = None;
                let mut redirect_status: Option<u16> = None;
                let mut preserve_path: Option<bool> = None;
                // SNI 用フィールド（Proxy用）
                let mut sni_name: Option<String> = None;
                // H2C 用フィールド（Proxy用）
                let mut use_h2c: Option<bool> = None;

                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => backend_type = Some(map.next_value()?),
                        "url" => url = Some(map.next_value()?),
                        "upstream" => upstream = Some(map.next_value()?),
                        "path" => path = Some(map.next_value()?),
                        "mode" => mode = Some(map.next_value()?),
                        "index" => index = Some(map.next_value()?),
                        // security, compression, buffering, cache, open_file_cache, modules は route 直下で設定されるため、ここでは無視
                        "security" | "compression" | "buffering" | "cache" | "open_file_cache"
                        | "modules" => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                        "redirect_url" => redirect_url = Some(map.next_value()?),
                        "redirect_status" => redirect_status = Some(map.next_value()?),
                        "preserve_path" => preserve_path = Some(map.next_value()?),
                        "sni_name" => sni_name = Some(map.next_value()?),
                        "use_h2c" | "h2c" => use_h2c = Some(map.next_value()?),
                        _ => {
                            let _: serde::de::IgnoredAny = map.next_value()?;
                        }
                    }
                }

                let backend_type = backend_type.unwrap_or_else(|| "File".to_string());

                match backend_type.as_str() {
                    "Proxy" => {
                        // upstream が指定されている場合はロードバランシング用
                        if let Some(upstream_name) = upstream {
                            let use_h2c = use_h2c.unwrap_or(false);
                            Ok(BackendConfig::ProxyUpstream {
                                upstream: upstream_name,
                                use_h2c,
                            })
                        } else {
                            let url = url.ok_or_else(|| {
                                serde::de::Error::missing_field("url or upstream")
                            })?;
                            let use_h2c = use_h2c.unwrap_or(false);
                            Ok(BackendConfig::Proxy {
                                url,
                                sni_name,
                                use_h2c,
                            })
                        }
                    }
                    "Redirect" => {
                        let redirect_url = redirect_url
                            .ok_or_else(|| serde::de::Error::missing_field("redirect_url"))?;
                        let redirect_status = redirect_status.unwrap_or(301);
                        // ステータスコードの検証（301, 302, 303, 307, 308のみ許可）
                        if !matches!(redirect_status, 301 | 302 | 303 | 307 | 308) {
                            return Err(serde::de::Error::custom(format!(
                                "invalid redirect_status: {}, expected 301, 302, 303, 307, or 308",
                                redirect_status
                            )));
                        }
                        let preserve_path = preserve_path.unwrap_or(false);
                        Ok(BackendConfig::Redirect {
                            redirect_url,
                            redirect_status,
                            preserve_path,
                        })
                    }
                    _ => {
                        let path = path.ok_or_else(|| serde::de::Error::missing_field("path"))?;
                        let mode = mode.unwrap_or_else(|| "sendfile".to_string());
                        Ok(BackendConfig::File { path, mode, index })
                    }
                }
            }
        }

        deserializer.deserialize_map(BackendConfigVisitor)
    }
}

// ====================
// ランタイムBackend
// ====================

#[derive(Clone)]
pub enum Backend {
    /// Proxy バックエンド（ロードバランシング対応）
    /// - Arc<UpstreamGroup>: アップストリームグループ（単一または複数バックエンド）
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    /// - Arc<CompressionConfig>: 圧縮設定
    /// - Arc<buffering::BufferingConfig>: バッファリング設定
    /// - Arc<cache::CacheConfig>: キャッシュ設定
    Proxy(
        Arc<UpstreamGroup>,
        Arc<SecurityConfig>,
        Arc<CompressionConfig>,
        Arc<buffering::BufferingConfig>,
        Arc<cache::CacheConfig>,
        /// WASMモジュール名のリスト（このバックエンドに適用するWASMモジュール）
        Option<Arc<Vec<String>>>,
    ),
    /// MemoryFile バックエンド
    /// - Arc<Vec<u8>>: ファイルコンテンツ
    /// - Arc<str>: MIMEタイプ
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    MemoryFile(
        Arc<Vec<u8>>,
        Arc<str>,
        Arc<SecurityConfig>,
        /// WASMモジュール名のリスト（このバックエンドに適用するWASMモジュール）
        Option<Arc<Vec<String>>>,
    ),
    /// SendFile バックエンド
    /// - Arc<PathBuf>: ベースパス
    /// - bool: ディレクトリかどうか
    /// - Option<Arc<str>>: インデックスファイル名（None = "index.html"）
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    /// - Arc<cache::CacheConfig>: キャッシュ設定
    /// - Option<Arc<cache::OpenFileCacheConfig>>: OpenFileCache設定（ルーティングごと）
    SendFile(
        Arc<PathBuf>,
        bool,
        Option<Arc<str>>,
        Arc<SecurityConfig>,
        Arc<cache::CacheConfig>,
        Option<Arc<cache::OpenFileCacheConfig>>,
        /// WASMモジュール名のリスト（このバックエンドに適用するWASMモジュール）
        Option<Arc<Vec<String>>>,
    ),
    /// Redirect バックエンド
    /// - Arc<str>: リダイレクト先URL
    /// - u16: ステータスコード（301, 302, 307, 308）
    /// - bool: 元のパスを保持するか
    Redirect(
        Arc<str>,
        u16,
        bool,
        /// WASMモジュール名のリスト（このバックエンドに適用するWASMモジュール）
        Option<Arc<Vec<String>>>,
    ),
}

impl Backend {
    /// このバックエンドのセキュリティ設定を取得
    #[inline]
    pub fn security(&self) -> &SecurityConfig {
        // デフォルトのセキュリティ設定（Redirect用）
        static DEFAULT_SECURITY: Lazy<SecurityConfig> = Lazy::new(SecurityConfig::default);

        match self {
            Backend::Proxy(_, security, _, _, _, _) => security,
            Backend::MemoryFile(_, _, security, _) => security,
            Backend::SendFile(_, _, _, security, _, _, _) => security,
            Backend::Redirect(_, _, _, _) => &DEFAULT_SECURITY,
        }
    }

    /// このバックエンドに適用するWASMモジュール名のリストを取得
    #[inline]
    /// F-43: WASM モジュールリストを Arc 共有で取得する（リクエストごとの deep copy 排除）。
    pub fn modules_arc(&self) -> Option<&Arc<Vec<String>>> {
        match self {
            Backend::Proxy(_, _, _, _, _, modules) => modules.as_ref(),
            Backend::MemoryFile(_, _, _, modules) => modules.as_ref(),
            Backend::SendFile(_, _, _, _, _, _, modules) => modules.as_ref(),
            Backend::Redirect(_, _, _, modules) => modules.as_ref(),
        }
    }

    pub fn modules(&self) -> Option<&[String]> {
        match self {
            Backend::Proxy(_, _, _, _, _, modules) => modules.as_deref().map(|v| v.as_slice()),
            Backend::MemoryFile(_, _, _, modules) => modules.as_deref().map(|v| v.as_slice()),
            Backend::SendFile(_, _, _, _, _, _, modules) => {
                modules.as_deref().map(|v| v.as_slice())
            }
            Backend::Redirect(_, _, _, modules) => modules.as_deref().map(|v| v.as_slice()),
        }
    }
}

#[derive(Clone)]
pub struct ProxyTarget {
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub path_prefix: String,
    /// SNI (Server Name Indication) に使用するホスト名
    /// Noneの場合はhostを使用。IP直打ちの場合にドメイン名を指定可能
    pub sni_name: Option<String>,
    /// H2C (HTTP/2 over cleartext) を使用するかどうか
    /// true の場合、非TLSバックエンドにHTTP/2で接続
    /// HTTP/2 Upgrade 経由ではなく、Prior Knowledge モードを使用
    pub use_h2c: bool,
}

impl ProxyTarget {
    pub fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else if let Some(rest) = url.strip_prefix("http://") {
            (false, rest)
        } else {
            return None;
        };

        let (host_port, path) = match rest.find('/') {
            Some(idx) => (&rest[..idx], &rest[idx..]),
            None => (rest, "/"),
        };

        let (host, port) = match host_port.find(':') {
            Some(idx) => {
                let h = &host_port[..idx];
                let p = host_port[idx + 1..].parse().ok()?;
                (h.to_string(), p)
            }
            None => (host_port.to_string(), if scheme { 443 } else { 80 }),
        };

        Some(ProxyTarget {
            host,
            port,
            use_tls: scheme,
            path_prefix: path.to_string(),
            sni_name: None,
            use_h2c: false, // デフォルトでは無効
        })
    }

    /// SNI名を設定したコピーを作成
    pub fn with_sni_name(mut self, sni_name: Option<String>) -> Self {
        self.sni_name = sni_name;
        self
    }

    /// H2C設定を変更したコピーを作成
    pub fn with_h2c(mut self, use_h2c: bool) -> Self {
        // H2Cは非TLSの場合のみ有効
        if !self.use_tls {
            self.use_h2c = use_h2c;
        }
        self
    }

    /// TLS接続時に使用するSNI名を取得
    #[inline]
    pub fn sni(&self) -> &str {
        self.sni_name.as_deref().unwrap_or(&self.host)
    }

    /// デフォルトポートかどうかを判定
    #[inline]
    pub fn is_default_port(&self) -> bool {
        if self.use_tls {
            self.port == 443
        } else {
            self.port == 80
        }
    }
}

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
fn extract_cookie_value<'a>(cookie_header: &'a str, name: &str) -> Option<&'a str> {
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
const CONSISTENT_HASH_VNODES: usize = 150;
/// Consistent Hash 用のシード（固定）
const CONSISTENT_HASH_SEED: u64 = 0x9E3779B97F4A7C15;

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
    /// TLS証明書検証を無効化（自己署名証明書を許可）
    pub tls_insecure: bool,
    /// H2C (HTTP/2 over cleartext) を強制するかどうか
    pub use_h2c: bool,
    /// Weighted Round Robin 用の累積重みオフセット（構築時計算、O(1) 選択用）
    /// servers と同じ順序。weighted_offsets[i] は servers[0..=i] の重みの累積和。
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
            tls_insecure,
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
            // サーバー識別子（host:port）を基に vnode を生成
            let id = format!("{}:{}", server.target.host, server.target.port);
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
            health_check: None,  // 単一サーバーでは健康チェックなし
            tls_insecure: false, // 単一サーバーではデフォルトで証明書検証を有効
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

    /// TLS証明書検証を無効化するかどうかを取得
    pub fn tls_insecure(&self) -> bool {
        self.tls_insecure
    }

    /// H2C (HTTP/2 over cleartext) を強制するかどうかを取得
    pub fn use_h2c(&self) -> bool {
        self.use_h2c
    }
}

// ====================
// 非同期I/Oトレイト（コード重複解消）
// ====================

/// 非同期読み込みトレイト（SafeReadBuffer対応）
///
/// 読み込み操作で `SafeReadBuffer` を受け取り、返却します。
/// monoio の `set_init()` コールバックにより、読み込み完了時に
/// 自動的に `valid_len` が設定されます。
#[allow(async_fn_in_trait)]
pub trait AsyncReader {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer);
}

/// 非同期書き込みトレイト
///
/// 書き込み操作では `Vec<u8>` を受け取ります。
/// 書き込みデータは既に有効なデータなので、SafeReadBuffer は不要です。
#[allow(async_fn_in_trait)]
pub trait AsyncWriter {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>);
}

// TcpStream用の実装
impl AsyncReader for TcpStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }
}

impl AsyncWriter for TcpStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// KtlsServerStream用の実装（rustls + ktls2）
#[cfg(veil_ktls)]
impl AsyncReader for KtlsServerStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }
}

#[cfg(veil_ktls)]
impl AsyncWriter for KtlsServerStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// KtlsClientStream用の実装（rustls + ktls2）
#[cfg(veil_ktls)]
impl AsyncReader for KtlsClientStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }
}

#[cfg(veil_ktls)]
impl AsyncWriter for KtlsClientStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// SimpleTlsServerStream用の実装（rustls のみ）
#[cfg(not(veil_ktls))]
impl AsyncReader for crate::simple_tls::SimpleTlsServerStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }
}

#[cfg(not(veil_ktls))]
impl AsyncWriter for crate::simple_tls::SimpleTlsServerStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// SimpleTlsClientStream用の実装（rustls のみ）
#[cfg(not(veil_ktls))]
impl AsyncReader for crate::simple_tls::SimpleTlsClientStream {
    async fn read_buf(&mut self, buf: SafeReadBuffer) -> (io::Result<usize>, SafeReadBuffer) {
        use crate::runtime::io::AsyncReadRent;
        self.read(buf).await
    }
}

#[cfg(not(veil_ktls))]
impl AsyncWriter for crate::simple_tls::SimpleTlsClientStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

/// HTTP/3 有効時に Alt-Svc 広告を初期化する（F-94、コールドパス）。
///
/// `server.http3_enabled && [http3].alt_svc_enabled` のとき、
/// `[http3].alt_svc` 明示値、または listen ポートから自動生成した値を登録する。
/// Alt-Svc 関連キーはすべて `[http3]` に集約する。
fn apply_alt_svc_from_config(config: &Config) {
    #[cfg(feature = "http3")]
    {
        let enabled = config.server.http3_enabled && config.http3.alt_svc_enabled;
        let value = if enabled {
            if let Some(ref explicit) = config.http3.alt_svc {
                explicit.clone()
            } else {
                let listen = config
                    .http3
                    .listen
                    .as_deref()
                    .unwrap_or(config.server.listen.as_str());
                crate::pool::build_alt_svc_value(listen, config.http3.alt_svc_ma_secs)
            }
        } else {
            String::new()
        };
        crate::pool::init_alt_svc(enabled, &value);
    }
    #[cfg(not(feature = "http3"))]
    {
        let _ = config;
        crate::pool::init_alt_svc(false, "");
    }
}

fn validate_config(config: &Config) -> io::Result<()> {
    // TLS証明書ファイルの存在チェック
    let cert_path = Path::new(&config.tls.cert_path);
    if !cert_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("TLS certificate file not found: {}", config.tls.cert_path),
        ));
    }

    let key_path = Path::new(&config.tls.key_path);
    if !key_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("TLS key file not found: {}", config.tls.key_path),
        ));
    }

    // バインドアドレスの妥当性チェック
    if config.server.listen.parse::<SocketAddr>().is_err() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("Invalid listen address: {}", config.server.listen),
        ));
    }

    // Upstream設定の妥当性チェック
    if let Some(ref upstreams) = config.upstreams {
        for (name, upstream) in upstreams {
            if upstream.servers.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Upstream '{}' has no servers configured", name),
                ));
            }

            for entry in &upstream.servers {
                if ProxyTarget::parse(&entry.url).is_none() {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("Invalid server URL in upstream '{}': {}", name, entry.url),
                    ));
                }
            }
        }
    }

    // 統合ルーティング（[[route]]）の妥当性チェック
    if let Some(ref routes) = config.route {
        for (i, route) in routes.iter().enumerate() {
            let route_name = format!("route[{}]", i);
            validate_route_config(
                route,
                &route_name,
                #[cfg(feature = "wasm")]
                config.wasm.as_ref(),
            )?;
        }
    }

    // WASM設定のバリデーション
    #[cfg(feature = "wasm")]
    if let Some(wasm_cfg) = &config.wasm {
        if wasm_cfg.enabled {
            wasm_cfg.validate().map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("WASM config validation failed: {}", e),
                )
            })?;
        }
    }

    Ok(())
}

/// ルート設定の妥当性チェック
fn validate_route_config(
    route: &Route,
    route_name: &str,
    #[cfg(feature = "wasm")] wasm_config: Option<&crate::wasm::WasmConfig>,
) -> io::Result<()> {
    match &route.action {
        BackendConfig::Proxy { url, .. } => {
            if ProxyTarget::parse(url).is_none() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Invalid proxy URL for route '{}': {}", route_name, url),
                ));
            }
        }
        BackendConfig::ProxyUpstream { upstream, .. } => {
            // upstream の存在は load_backend でチェックされる
            if upstream.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Empty upstream name for route '{}'", route_name),
                ));
            }
        }
        BackendConfig::File { path, mode, .. } => {
            let file_path = Path::new(path);
            if !file_path.exists() {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!(
                        "File/directory not found for route '{}': {}",
                        route_name, path
                    ),
                ));
            }

            if !["sendfile", "memory", ""].contains(&mode.as_str()) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "Invalid mode for route '{}': {} (expected 'sendfile' or 'memory')",
                        route_name, mode
                    ),
                ));
            }
        }
        BackendConfig::Redirect {
            redirect_url,
            redirect_status,
            ..
        } => {
            if redirect_url.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Empty redirect_url for route '{}'", route_name),
                ));
            }
            if !matches!(*redirect_status, 301 | 302 | 303 | 307 | 308) {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Invalid redirect_status for route '{}': {} (expected 301, 302, 303, 307, or 308)", route_name, redirect_status)
                ));
            }
        }
    }

    // WASMモジュールの参照チェック（route直下のmodulesを使用）
    #[cfg(feature = "wasm")]
    if let Some(wasm_cfg) = wasm_config {
        if let Some(ref modules) = route.modules {
            for module_name in modules {
                if !wasm_cfg.modules.iter().any(|m| &m.name == module_name) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!(
                            "Route '{}' references unknown WASM module: {}",
                            route_name, module_name
                        ),
                    ));
                }
            }
        }
    }

    Ok(())
}

// rustls 用の TLS 設定読み込み（統一）
/// 証明書・秘密鍵パスから ServerConfig を構築する（F-03 リローダー用の公開 API）
///
/// `load_tls_config` と同じ手順（ALPN / kTLS シークレット抽出）で再構築する。
pub fn build_server_config_from_paths(
    cert_path: &Path,
    key_path: &Path,
    ktls_enabled: bool,
    http2_enabled: bool,
    cipher_suites: &[String],
) -> anyhow::Result<Arc<ServerConfig>> {
    let section = TlsConfigSection {
        cert_path: cert_path.to_string_lossy().into_owned(),
        key_path: key_path.to_string_lossy().into_owned(),
        ktls_enabled,
        ktls_fallback_enabled: true,
        tcp_cork_enabled: true,
        cipher_suites: cipher_suites.to_vec(),
        auto_reload: false,
        reload_interval_secs: default_tls_reload_interval(),
    };
    load_tls_config(&section, ktls_enabled, http2_enabled)
        .map_err(|e| anyhow::anyhow!("TLS reload build failed: {}", e))
}

/// 設定名（例: `TLS13_AES_256_GCM_SHA384`）から rustls の暗号スイートを解決する（F-50）。
///
/// 配列の順序を保持する（記載順 = サーバ優先度順）。不明な名前・重複はエラー。
/// 設定読み込み時（非ホットパス）にのみ呼ばれるため、ここでのアロケーションは許容される。
pub fn resolve_cipher_suites(names: &[String]) -> io::Result<Vec<rustls::SupportedCipherSuite>> {
    use crate::tls_provider::provider::ALL_CIPHER_SUITES;

    let mut out: Vec<rustls::SupportedCipherSuite> = Vec::with_capacity(names.len());
    for name in names {
        let found = ALL_CIPHER_SUITES
            .iter()
            .find(|s| format!("{:?}", s.suite()).eq_ignore_ascii_case(name));
        match found {
            Some(s) => {
                if out.iter().any(|e| e.suite() == s.suite()) {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("[tls] cipher_suites: duplicate cipher suite '{}'", name),
                    ));
                }
                out.push(*s);
            }
            None => {
                let valid: Vec<String> = ALL_CIPHER_SUITES
                    .iter()
                    .map(|s| format!("{:?}", s.suite()))
                    .collect();
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "[tls] cipher_suites: unknown cipher suite '{}'. Valid names: {}",
                        name,
                        valid.join(", ")
                    ),
                ));
            }
        }
    }
    Ok(out)
}

fn load_tls_config(
    tls_config: &TlsConfigSection,
    ktls_enabled: bool,
    #[allow(unused_variables)] http2_enabled: bool,
) -> io::Result<Arc<ServerConfig>> {
    let cert_file = File::open(&tls_config.cert_path)?;
    let key_file = File::open(&tls_config.key_path)?;

    let cert_reader = BufReader::new(cert_file);
    let cert_chain: Vec<CertificateDer<'static>> = CertificateDer::pem_reader_iter(cert_reader)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("Certificate parse error: {}", e),
            )
        })?;

    let key_reader = BufReader::new(key_file);
    let keys: PrivateKeyDer<'static> = PrivateKeyDer::from_pem_reader(key_reader).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Private key parse error: {}", e),
        )
    })?;

    // F-50: [tls] cipher_suites による暗号スイートの取捨選択・優先度指定
    //
    // 指定がある場合は CryptoProvider の cipher_suites を設定順（= サーバ優先度順）で
    // 差し替える。不正な名前はここでエラーになる（起動時 / リロード時の検証）。
    let custom_suites = if !tls_config.cipher_suites.is_empty() {
        let suites = resolve_cipher_suites(&tls_config.cipher_suites)?;
        info!(
            "TLS cipher suites restricted by config (priority order): {:?}",
            tls_config.cipher_suites
        );
        Some(suites)
    } else {
        None
    };

    // kTLS 互換性チェック: kTLS は AES-GCM 系のみオフロード可能。
    // 非互換スイートが指定されている場合は警告する（該当接続は rustls フォールバック、
    // ktls_fallback_enabled = false ならハンドシェイク後に拒否される）。
    #[cfg(veil_ktls)]
    if ktls_enabled {
        if let Some(ref suites) = custom_suites {
            let compat = crate::ktls::ktls_compatible_cipher_suites();
            let incompatible: Vec<String> = suites
                .iter()
                .filter(|s| !compat.contains(s))
                .map(|s| format!("{:?}", s.suite()))
                .collect();
            if !incompatible.is_empty() {
                warn!(
                    "kTLS enabled but non-kTLS-compatible cipher suites configured: {}. \
                     Connections negotiating these will not be offloaded to kTLS",
                    incompatible.join(", ")
                );
            }
        } else {
            info!("kTLS enabled: kTLS offload is available for AES-GCM cipher suites");
        }
    }

    // kTLS 有効時のみ config を変更するため、条件付きで mut を使用
    #[allow(unused_mut)]
    let mut config = {
        #[cfg(not(veil_ktls))]
        let _ = ktls_enabled;

        let mut provider = crate::tls_provider::provider::default_provider();
        if let Some(suites) = custom_suites {
            provider.cipher_suites = suites;
        }

        ServerConfig::builder_with_provider(provider.into())
            .with_protocol_versions(rustls::DEFAULT_VERSIONS)
            .map_err(|e| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("TLS version error: {}", e),
                )
            })?
            .with_no_client_auth()
            .with_single_cert(cert_chain, keys)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
    };

    // kTLS が有効な場合のみシークレット抽出を有効化
    // これにより dangerous_extract_secrets() が使用可能になる
    #[cfg(veil_ktls)]
    if ktls_enabled {
        config.enable_secret_extraction = true;
        info!("TLS secret extraction enabled for kTLS support");
    }

    // HTTP/2 有効時は ALPN を設定
    #[cfg(feature = "http2")]
    if http2_enabled {
        config = protocol::configure_alpn_h2(config, false);
        info!("HTTP/2 enabled via ALPN negotiation (h2, http/1.1)");
    }

    Ok(Arc::new(config))
}

/// 設定読み込みの戻り値型（統一）
pub struct LoadedConfig {
    pub listen_addr: String,
    /// HTTPリスナーアドレス（HTTPSリダイレクト用、オプション）
    pub listen_http_addr: Option<SocketAddr>,
    pub tls_config: Arc<ServerConfig>,
    /// TLS証明書パス（ログ・表示用）
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    pub tls_cert_path: String,
    /// TLS秘密鍵パス（ログ・表示用）
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    pub tls_key_path: String,
    /// 証明書の自動リロードを有効にするか（F-03）
    pub tls_auto_reload: bool,
    /// 証明書リロードチェック間隔（秒、F-03）
    pub tls_reload_interval_secs: u64,
    /// 設定された TLS 暗号スイート（F-50、リロード時の再構築用）
    pub tls_cipher_suites: Vec<String>,
    /// TLS証明書（PEM形式、事前読み込み済み）
    ///
    /// Landlock適用前に読み込まれた証明書データ。
    /// HTTP/3ではmemfd経由でquicheに渡すことで、
    /// Landlockによるファイルシステム制限下でも動作可能。
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    pub tls_cert_pem: Arc<Vec<u8>>,
    /// TLS秘密鍵（PEM形式、事前読み込み済み）
    ///
    /// Landlock適用前に読み込まれた秘密鍵データ。
    /// HTTP/3ではmemfd経由でquicheに渡す。
    #[cfg_attr(not(feature = "http3"), allow(dead_code))]
    pub tls_key_pem: Arc<Vec<u8>>,
    /// 統合ルーティング（唯一のルーティング方式）
    pub route: Arc<Vec<Route>>,
    /// 最適化ルーター（Phase 1-4最適化適用）
    pub optimized_router: Arc<routing::OptimizedRouter>,
    pub ktls_config: KtlsConfig,
    pub reuseport_balancing: ReuseportBalancing,
    pub num_threads: usize,
    /// Huge Pages (Large OS Pages) を有効化するかどうか
    pub huge_pages_enabled: bool,
    /// グローバルセキュリティ設定
    pub global_security: GlobalSecurityConfig,
    /// ログ設定
    pub logging: LoggingConfigSection,
    /// Prometheusメトリクス設定
    pub prometheus_config: PrometheusConfig,
    /// 管理 API 設定（F-20）
    #[cfg(feature = "admin")]
    pub admin_config: AdminConfig,
    /// 構造化アクセスログ設定（F-21）
    #[cfg(feature = "access-log")]
    pub access_log_config: crate::access_log::AccessLogConfig,
    /// OpenTelemetry 設定（F-10）
    pub opentelemetry: OpenTelemetryConfig,
    /// Upstream グループ（健康チェック用）
    pub upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    /// HTTP/2 を有効化するかどうか
    #[cfg(feature = "http2")]
    pub http2_enabled: bool,
    /// HTTP/3 を有効化するかどうか
    #[cfg(feature = "http3")]
    pub http3_enabled: bool,
    /// HTTP/3 リスナーアドレス (UDP)
    #[cfg(feature = "http3")]
    pub http3_listen: Option<String>,
    /// HTTP/2 設定（詳細設定）
    #[cfg(feature = "http2")]
    pub http2_config: Http2ConfigSection,
    /// HTTP/3 設定（詳細設定）
    #[cfg(feature = "http3")]
    pub http3_config: Http3ConfigSection,
    /// H2C (HTTP/2 Cleartext) を有効化するかどうか
    #[cfg(feature = "http2")]
    pub h2c_enabled: bool,
    /// H2C リスニングアドレス（オプション）
    #[cfg(feature = "http2")]
    pub h2c_listen: Option<String>,
    /// WASM Filter Engine（WASM機能が有効な場合）
    #[cfg(feature = "wasm")]
    pub wasm_filter_engine: Option<Arc<crate::wasm::FilterEngine>>,
    /// パフォーマンス設定
    pub performance: PerformanceConfigSection,
    /// グレースフルシャットダウンタイムアウト（秒）
    pub graceful_shutdown_timeout_secs: u64,
    /// L4 プロキシリスナー設定（F-18）
    #[cfg(feature = "l4-proxy")]
    pub l4_listeners: Vec<L4ListenerConfig>,
}

// ====================
// ホットリロード対応のランタイム設定
// ====================
//
// ArcSwap を使用することで、設定変更時にロックフリーで
// 新しい設定に切り替えることができます。
//
// ## メリット
// - 読み込みはロックフリーで非常に高速（数ナノ秒）
// - 設定更新中もリクエスト処理を継続可能
// - 古い設定を参照中のリクエストは安全に完了
//
// ## 使用方法
// ```rust
// // 設定の読み込み（ロックフリー）
// let config = CURRENT_CONFIG.load();
//
// // 設定の更新（アトミック）
// CURRENT_CONFIG.store(Arc::new(new_config));
// ```

/// ランタイムで使用する設定（ホットリロード対応）
///
/// 一部のフィールドはホットリロード機能のために保持されているが、
/// 現在は読み取られていない（将来的にTLS再設定などで使用予定）
pub struct RuntimeConfig {
    /// 統合ルーティング（唯一のルーティング方式）
    pub route: Arc<Vec<Route>>,
    /// 最適化ルーター（Phase 1-4最適化適用）
    pub optimized_router: Arc<routing::OptimizedRouter>,
    /// TLS設定（ホットリロード時の参照用）
    pub tls_config: Option<Arc<ServerConfig>>,
    /// kTLS設定（ホットリロード時の参照用）
    pub ktls_config: Arc<KtlsConfig>,
    /// グローバルセキュリティ設定（ホットリロード時の参照用）
    pub global_security: Arc<GlobalSecurityConfig>,
    /// Prometheusメトリクス設定
    pub prometheus_config: Arc<PrometheusConfig>,
    /// 管理 API 設定（F-20）
    #[cfg(feature = "admin")]
    pub admin_config: Arc<AdminConfig>,
    /// 構造化アクセスログ設定（F-21）
    #[cfg(feature = "access-log")]
    pub access_log_config: Arc<crate::access_log::AccessLogConfig>,
    /// Upstream グループ（健康チェック用）
    pub upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    /// HTTP/2 有効化フラグ
    #[cfg(feature = "http2")]
    pub http2_enabled: bool,
    /// HTTP/2 設定（詳細設定）
    #[cfg(feature = "http2")]
    pub http2_config: Http2ConfigSection,
    /// HTTP/3 設定（圧縮設定の解決に使用）
    #[cfg(feature = "http3")]
    pub http3_config: Http3ConfigSection,
    /// H2C (HTTP/2 Cleartext) を有効化するかどうか
    #[cfg(feature = "http2")]
    pub h2c_enabled: bool,
    /// H2C リスニングアドレス（オプション）
    /// ホットリロード時の参照用（現在は起動時のみ使用）
    #[cfg(feature = "http2")]
    pub h2c_listen: Option<String>,
    /// WASM Filter Engine（WASM機能が有効な場合）
    #[cfg(feature = "wasm")]
    pub wasm_filter_engine: Option<Arc<crate::wasm::FilterEngine>>,
    /// パフォーマンス設定（Via header, chunk size等）
    pub performance: PerformanceConfigSection,
    /// L4 プロキシリスナー設定（F-18）
    #[cfg(feature = "l4-proxy")]
    pub l4_listeners: Arc<Vec<L4ListenerConfig>>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            route: Arc::new(Vec::new()),
            optimized_router: Arc::new(routing::OptimizedRouter::new()),
            tls_config: None,
            ktls_config: Arc::new(KtlsConfig::default()),
            global_security: Arc::new(GlobalSecurityConfig::default()),
            prometheus_config: Arc::new(PrometheusConfig::default()),
            #[cfg(feature = "admin")]
            admin_config: Arc::new(AdminConfig::default()),
            #[cfg(feature = "access-log")]
            access_log_config: Arc::new(crate::access_log::AccessLogConfig::default()),
            upstream_groups: Arc::new(HashMap::new()),
            #[cfg(feature = "http2")]
            http2_enabled: false,
            #[cfg(feature = "http2")]
            http2_config: Http2ConfigSection::default(),
            #[cfg(feature = "http3")]
            http3_config: Http3ConfigSection::default(),
            #[cfg(feature = "http2")]
            h2c_enabled: false,
            #[cfg(feature = "http2")]
            h2c_listen: None,
            #[cfg(feature = "wasm")]
            wasm_filter_engine: None,
            performance: PerformanceConfigSection::default(),
            #[cfg(feature = "l4-proxy")]
            l4_listeners: Arc::new(Vec::new()),
        }
    }
}

/// グローバルな設定保持用（ホットリロード対応）
/// 読み込みはロックフリーで非常に高速
pub static CURRENT_CONFIG: Lazy<ArcSwap<RuntimeConfig>> =
    Lazy::new(|| ArcSwap::from_pointee(RuntimeConfig::default()));

/// デフォルトの設定ファイルパス
const DEFAULT_CONFIG_PATH: &str = "/etc/veil/config.toml";

/// グローバルな設定ファイルパス（ホットリロード用）
/// コマンドライン引数で指定されたパス、またはデフォルトパスを保持
pub static CONFIG_PATH: Lazy<ArcSwap<PathBuf>> =
    Lazy::new(|| ArcSwap::from_pointee(PathBuf::from(DEFAULT_CONFIG_PATH)));

/// HTTPSリダイレクト先のポート（listen設定から抽出）
///
/// HTTP→HTTPSリダイレクト時に使用するポート番号。
/// デフォルトは443（HTTPSの標準ポート）。
/// main関数で`[server].listen`の値から初期化される。
pub static HTTPS_REDIRECT_PORT: std::sync::atomic::AtomicU16 =
    std::sync::atomic::AtomicU16::new(443);

/// 設定をホットリロードする
///
/// 実行中のリクエストは古い設定を参照し続け、
/// 新規リクエストは新しい設定を使用します。
///
/// ## セキュリティに関する注意
///
/// TLS証明書・秘密鍵はホットリロードの対象外です。
/// これはLandlockによるファイルシステム制限を適用後、
/// 証明書ファイルへのアクセスを禁止するためです。
///
/// 証明書を更新する場合は、サーバーを再起動してください。
pub fn reload_config(path: &Path) -> io::Result<()> {
    let loaded = load_config_without_tls(path)?;

    // 現在のTLS設定を維持（ホットリロード対象外）
    let current = CURRENT_CONFIG.load();

    // キャッシュをクリア（設定変更時）
    loaded.optimized_router.clear_cache();

    let runtime_config = RuntimeConfig {
        route: loaded.route,
        optimized_router: loaded.optimized_router,
        // TLS設定は起動時のものを維持（セキュリティ上の理由）
        tls_config: current.tls_config.clone(),
        ktls_config: current.ktls_config.clone(),
        global_security: Arc::new(loaded.global_security),
        prometheus_config: Arc::new(loaded.prometheus_config),
        #[cfg(feature = "admin")]
        admin_config: Arc::new(loaded.admin_config),
        #[cfg(feature = "access-log")]
        access_log_config: Arc::new(loaded.access_log_config),
        upstream_groups: loaded.upstream_groups,
        #[cfg(feature = "http2")]
        http2_enabled: loaded.http2_enabled,
        #[cfg(feature = "http2")]
        http2_config: loaded.http2_config,
        #[cfg(feature = "http3")]
        http3_config: loaded.http3_config,
        #[cfg(feature = "http2")]
        h2c_enabled: loaded.h2c_enabled,
        #[cfg(feature = "http2")]
        h2c_listen: loaded.h2c_listen.clone(),
        #[cfg(feature = "wasm")]
        wasm_filter_engine: current.wasm_filter_engine.clone(),
        performance: loaded.performance.clone(),
        // L4 リスナーはホットリロード対象外（再起動が必要）
        #[cfg(feature = "l4-proxy")]
        l4_listeners: current.l4_listeners.clone(),
    };

    // アトミックに設定を入れ替え
    CURRENT_CONFIG.store(Arc::new(runtime_config));

    info!("Configuration reloaded successfully (TLS certificates unchanged - restart required for TLS updates)");
    Ok(())
}

/// ホットリロード用の設定（TLS証明書を除く）
///
/// Landlock適用後はTLS証明書ファイルへのアクセスが制限されるため、
/// ホットリロード時はルーティング設定等のみを更新します。
pub struct LoadedConfigWithoutTls {
    /// 統合ルーティング（唯一のルーティング方式）
    pub route: Arc<Vec<Route>>,
    /// 最適化ルーター（Phase 1-4最適化適用）
    pub optimized_router: Arc<routing::OptimizedRouter>,
    pub global_security: GlobalSecurityConfig,
    pub prometheus_config: PrometheusConfig,
    #[cfg(feature = "admin")]
    pub admin_config: AdminConfig,
    /// 構造化アクセスログ設定（F-21）
    #[cfg(feature = "access-log")]
    pub access_log_config: crate::access_log::AccessLogConfig,
    pub upstream_groups: Arc<HashMap<String, Arc<UpstreamGroup>>>,
    #[cfg(feature = "http2")]
    pub http2_enabled: bool,
    #[cfg(feature = "http2")]
    pub http2_config: Http2ConfigSection,
    #[cfg(feature = "http3")]
    pub http3_config: Http3ConfigSection,
    #[cfg(feature = "http2")]
    pub h2c_enabled: bool,
    #[cfg(feature = "http2")]
    pub h2c_listen: Option<String>,
    pub performance: PerformanceConfigSection,
}

/// TLS証明書を除いた設定をロード（ホットリロード用）
///
/// Landlock適用後は証明書ファイルへのアクセスが制限されるため、
/// この関数ではTLS関連の読み込みをスキップします。
// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
fn load_config_without_tls(path: &Path) -> io::Result<LoadedConfigWithoutTls> {
    let config_str = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&config_str).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("TOML parse error: {}", e),
        )
    })?;

    // 設定ファイルのバリデーション
    validate_config(&config)?;

    // HTTP/2・HTTP/3・H2C 設定を読み込み
    #[cfg(feature = "http2")]
    let http2_enabled = config.server.http2_enabled;
    #[cfg(feature = "http2")]
    let http2_config = config.http2.clone();
    #[cfg(feature = "http3")]
    let http3_config = config.http3.clone();
    #[cfg(feature = "http2")]
    let h2c_enabled = config.server.h2c_enabled;
    #[cfg(feature = "http2")]
    let h2c_listen = config.server.h2c_listen.clone();

    // Serverヘッダー設定を更新（リロード対応）
    init_server_header(
        config.server.server_header_enabled,
        &config.server.server_header_value,
    );

    // Alt-Svc（HTTP/3 広告、F-94）— リロードでも同期
    apply_alt_svc_from_config(&config);

    // Upstream グループを構築（ロードバランシング用）
    let mut upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();
    if let Some(upstreams) = &config.upstreams {
        for (name, cfg) in upstreams {
            let algorithm = resolve_algorithm(&cfg.algorithm, &cfg.hash_key);
            if let Some(group) = UpstreamGroup::new(
                name.clone(),
                cfg.servers.clone(),
                algorithm.clone(),
                cfg.health_check.clone(),
                cfg.tls_insecure,
            ) {
                let group = group.with_resilience(&cfg.circuit_breaker, &cfg.outlier_detection);
                info!(
                    "Reloaded upstream '{}' with {} servers ({:?})",
                    name,
                    group.len(),
                    algorithm
                );
                upstream_groups.insert(name.clone(), Arc::new(group));
            } else {
                warn!("Failed to reload upstream '{}': no valid servers", name);
            }
        }
    }

    // 統合ルーティング（[[route]]）の読み込み
    let routes = if let Some(routes_config) = config.route {
        let mut routes_vec = Vec::with_capacity(routes_config.len());
        for route in routes_config {
            routes_vec.push(route);
        }
        Arc::new(routes_vec)
    } else {
        Arc::new(Vec::new())
    };

    // OptimizedRouter を構築（Phase 1-4 最適化）
    let optimized_router = build_optimized_router(&routes);

    // グローバルOpenFileCache設定を適用
    let performance_config = &config.performance;
    cache::configure_global_open_file_cache(
        performance_config.open_file_cache_enabled,
        performance_config.open_file_cache_valid_duration_secs,
        performance_config.open_file_cache_max_entries,
    );

    // F-35: グローバル IP ブロックリストを適用（起動時・SIGHUP リロード時の両方で本関数が
    // 呼ばれるためここで一元的に適用する）。CIDR はパース済みで保持され accept ホットパスでは
    // 文字列解析を行わない。
    set_global_blocked_ips(&config.security.blocked_ips);

    // AdminConfig の事前計算フィールドを補完
    #[cfg(feature = "admin")]
    let admin_config = {
        let mut admin = config.admin;
        admin.compute_derived();
        admin
    };

    Ok(LoadedConfigWithoutTls {
        route: routes,
        optimized_router,
        global_security: config.security,
        prometheus_config: config.prometheus,
        #[cfg(feature = "admin")]
        admin_config,
        #[cfg(feature = "access-log")]
        access_log_config: config.access_log,
        upstream_groups: Arc::new(upstream_groups),
        #[cfg(feature = "http2")]
        http2_enabled,
        #[cfg(feature = "http2")]
        http2_config,
        #[cfg(feature = "http3")]
        http3_config,
        #[cfg(feature = "http2")]
        h2c_enabled,
        #[cfg(feature = "http2")]
        h2c_listen,
        performance: config.performance.clone(),
    })
}

/// ルート配列から OptimizedRouter を構築
///
/// Phase 1: Host-based グループ化
/// Phase 2: Path Radix Tree (matchit)
/// Phase 3: CIDR Tree 最適化
/// Phase 4: LRU キャッシュ（構築時に初期化）
fn build_optimized_router(routes: &[Route]) -> Arc<routing::OptimizedRouter> {
    let mut router = routing::OptimizedRouter::with_cache_capacity(10000);

    for (idx, route) in routes.iter().enumerate() {
        let conditions = &route.conditions;

        // host条件
        let host = conditions.host.as_deref();
        // path条件
        let path = conditions.path.as_deref();
        // source_ip条件
        let source_ip = conditions.source_ip.as_deref();

        router.add_route(idx, host, path, source_ip);
    }

    // CIDRマッチャーを最適化（ソート）
    router.finalize();

    info!(
        "Built OptimizedRouter: {} routes indexed (host groups: {} exact + {} wildcard, path patterns: {})",
        routes.len(),
        router.host_router.exact_count(),
        router.host_router.wildcard_count(),
        router.path_router.patterns_count()
    );

    Arc::new(router)
}

// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub fn load_config(path: &Path) -> io::Result<LoadedConfig> {
    let config_str = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&config_str).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("TOML parse error: {}", e),
        )
    })?;

    // 設定ファイルのバリデーション
    validate_config(&config)?;

    // kTLS設定（TLS設定より先に読み込む）
    let ktls_config = KtlsConfig {
        enabled: config.tls.ktls_enabled,
        enable_tx: config.tls.ktls_enabled,
        enable_rx: config.tls.ktls_enabled,
        fallback_enabled: config.tls.ktls_fallback_enabled,
        tcp_cork_enabled: config.tls.tcp_cork_enabled,
    };

    // HTTP/2・HTTP/3・H2C 設定を読み込み
    // 有効化フラグは server セクションで管理、詳細設定は [http2]/[http3] セクション
    #[cfg(feature = "http2")]
    let http2_enabled = config.server.http2_enabled;
    #[cfg(feature = "http3")]
    let http3_enabled = config.server.http3_enabled;
    #[cfg(feature = "http2")]
    let http2_config = config.http2.clone();
    #[cfg(feature = "http3")]
    let http3_config = config.http3.clone();
    #[cfg(feature = "http3")]
    let http3_listen = http3_config.listen.clone();
    #[cfg(feature = "http2")]
    let h2c_enabled = config.server.h2c_enabled;
    #[cfg(feature = "http2")]
    let h2c_listen = config.server.h2c_listen.clone();

    // Serverヘッダー設定を初期化
    init_server_header(
        config.server.server_header_enabled,
        &config.server.server_header_value,
    );

    // Alt-Svc（HTTP/3 広告、F-94）
    apply_alt_svc_from_config(&config);

    // バッファプール設定を初期化
    init_buffer_pool_config(config.buffer_pool.clone());

    // TLS設定（kTLS有効時はシークレット抽出を有効化、HTTP/2有効時はALPN設定）
    #[cfg(feature = "http2")]
    let tls_config = load_tls_config(&config.tls, ktls_config.enabled, http2_enabled)?;
    #[cfg(not(feature = "http2"))]
    let tls_config = load_tls_config(&config.tls, ktls_config.enabled, false)?;

    // Upstream グループを構築（ロードバランシング用）
    let mut upstream_groups: HashMap<String, Arc<UpstreamGroup>> = HashMap::new();
    if let Some(upstreams) = &config.upstreams {
        for (name, cfg) in upstreams {
            let algorithm = resolve_algorithm(&cfg.algorithm, &cfg.hash_key);
            if let Some(group) = UpstreamGroup::new(
                name.clone(),
                cfg.servers.clone(),
                algorithm.clone(),
                cfg.health_check.clone(),
                cfg.tls_insecure,
            ) {
                let group = group.with_resilience(&cfg.circuit_breaker, &cfg.outlier_detection);
                info!(
                    "Loaded upstream '{}' with {} servers ({:?})",
                    name,
                    group.len(),
                    algorithm
                );
                if cfg.health_check.is_some() {
                    info!("  Health check enabled for '{}'", name);
                }
                upstream_groups.insert(name.clone(), Arc::new(group));
            } else {
                warn!("Failed to load upstream '{}': no valid servers", name);
            }
        }
    }

    // 統合ルーティング（[[route]]）の読み込み
    let routes = if let Some(routes_config) = config.route {
        let mut routes_vec = Vec::with_capacity(routes_config.len());
        for route in routes_config {
            routes_vec.push(route);
        }
        Arc::new(routes_vec)
    } else {
        Arc::new(Vec::new())
    };

    // OptimizedRouter を構築（Phase 1-4 最適化）
    let optimized_router = build_optimized_router(&routes);

    // グローバルOpenFileCache設定を適用
    let performance_config = &config.performance;
    cache::configure_global_open_file_cache(
        performance_config.open_file_cache_enabled,
        performance_config.open_file_cache_valid_duration_secs,
        performance_config.open_file_cache_max_entries,
    );

    // F-35: グローバル IP ブロックリストを適用（起動時・SIGHUP リロード時の両方で本関数が
    // 呼ばれるためここで一元的に適用する）。CIDR はパース済みで保持され accept ホットパスでは
    // 文字列解析を行わない。
    set_global_blocked_ips(&config.security.blocked_ips);

    // スレッド数の決定: 未指定または0の場合はCPUコア数を使用
    let num_threads = match config.server.threads {
        Some(n) if n > 0 => n,
        _ => num_cpus::get(),
    };

    // HTTPリスナーアドレスをパース（HTTPSリダイレクト用）
    let listen_http_addr =
        config
            .server
            .http
            .as_ref()
            .and_then(|addr| match addr.parse::<SocketAddr>() {
                Ok(socket_addr) => Some(socket_addr),
                Err(e) => {
                    warn!("Invalid HTTP listen address '{}': {}", addr, e);
                    None
                }
            });

    // Prometheusメトリクス設定をログ出力
    // F-09: ランタイムトグルを設定（enabled=false で record_* がノーオップ・endpoint が 404）
    crate::metrics::set_metrics_runtime_enabled(config.prometheus.enabled);
    if config.prometheus.enabled {
        info!(
            "Prometheus metrics enabled at path: {}",
            config.prometheus.path
        );
        if !config.prometheus.allowed_ips.is_empty() {
            info!("  Allowed IPs: {:?}", config.prometheus.allowed_ips);
        }
    } else {
        info!("Prometheus metrics disabled");
    }

    // TLS証明書をバイト列として読み込み（HTTP/3用、Landlock適用前に読み込み）
    // これによりLandlock適用後も証明書ファイルへのアクセスなしで動作可能
    let tls_cert_pem = fs::read(&config.tls.cert_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to read TLS cert '{}': {}", config.tls.cert_path, e),
        )
    })?;
    let tls_key_pem = fs::read(&config.tls.key_path).map_err(|e| {
        io::Error::new(
            e.kind(),
            format!("Failed to read TLS key '{}': {}", config.tls.key_path, e),
        )
    })?;

    info!(
        "TLS certificates pre-loaded for Landlock compatibility (cert: {} bytes, key: {} bytes)",
        tls_cert_pem.len(),
        tls_key_pem.len()
    );

    // WASM Filter Engineの初期化
    #[cfg(feature = "wasm")]
    let wasm_filter_engine = if let Some(wasm_config) = &config.wasm {
        if wasm_config.enabled {
            // バリデーション（validate_configで既に実行されているが、念のため）
            if let Err(e) = wasm_config.validate() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("WASM config validation failed: {}", e),
                ));
            }

            // FilterEngine初期化
            info!("Initializing WASM Filter Engine...");
            match crate::wasm::init(wasm_config) {
                Ok(engine) => {
                    info!("WASM Filter Engine initialized successfully");
                    Some(Arc::new(engine))
                }
                Err(e) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        format!("Failed to initialize WASM Filter Engine: {}", e),
                    ));
                }
            }
        } else {
            info!("WASM extensions disabled");
            None
        }
    } else {
        None
    };

    Ok(LoadedConfig {
        listen_addr: config.server.listen,
        listen_http_addr,
        tls_config,
        tls_cert_path: config.tls.cert_path.clone(),
        tls_key_path: config.tls.key_path.clone(),
        tls_auto_reload: config.tls.auto_reload,
        tls_reload_interval_secs: config.tls.reload_interval_secs,
        tls_cipher_suites: config.tls.cipher_suites.clone(),
        tls_cert_pem: Arc::new(tls_cert_pem),
        tls_key_pem: Arc::new(tls_key_pem),
        route: routes,
        optimized_router,
        ktls_config,
        reuseport_balancing: config.performance.reuseport_balancing,
        num_threads,
        huge_pages_enabled: config.performance.huge_pages_enabled,
        global_security: config.security,
        logging: config.logging,
        prometheus_config: config.prometheus,
        #[cfg(feature = "admin")]
        admin_config: {
            let mut a = config.admin;
            a.compute_derived();
            a
        },
        #[cfg(feature = "access-log")]
        access_log_config: config.access_log,
        opentelemetry: config.opentelemetry,
        upstream_groups: Arc::new(upstream_groups),
        #[cfg(feature = "http2")]
        http2_enabled,
        #[cfg(feature = "http3")]
        http3_enabled,
        #[cfg(feature = "http3")]
        http3_listen,
        #[cfg(feature = "http2")]
        http2_config,
        #[cfg(feature = "http3")]
        http3_config,
        #[cfg(feature = "http2")]
        h2c_enabled,
        #[cfg(feature = "http2")]
        h2c_listen,
        #[cfg(feature = "wasm")]
        wasm_filter_engine,
        performance: config.performance.clone(),
        graceful_shutdown_timeout_secs: config.server.graceful_shutdown_timeout_secs,
        #[cfg(feature = "l4-proxy")]
        l4_listeners: config.l4.unwrap_or_default(),
    })
}

/// 設定ファイルからログ設定のみを読み込む（ログ初期化前用）
// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub fn load_logging_config(path: &Path) -> io::Result<LoggingConfigSection> {
    let config_str = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&config_str).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("TOML parse error: {}", e),
        )
    })?;
    Ok(config.logging)
}

// JSON形式ログフォーマッタ（JsonLogFormat 等）と init_logging は
// crate::logging モジュールに移動しました。

// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub fn load_backend(
    route: &Route,
    upstream_groups: &HashMap<String, Arc<UpstreamGroup>>,
) -> io::Result<Backend> {
    // Routeレベルの設定を取得（route直下の設定のみを使用）
    let security = route.security.clone().unwrap_or_default();
    let compression = route.compression.clone().unwrap_or_default();
    let buffering = route.buffering.clone().unwrap_or_default();
    let cache = route.cache.clone().unwrap_or_default();
    let modules_arc = route.modules.as_ref().map(|m| Arc::new(m.clone()));

    match &route.action {
        BackendConfig::Proxy {
            url,
            sni_name,
            use_h2c,
        } => {
            // 単一URLの場合は UpstreamGroup::single で単一サーバーのグループを作成
            let target = ProxyTarget::parse(url)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid proxy URL"))?
                .with_sni_name(sni_name.clone())
                .with_h2c(*use_h2c);

            if *use_h2c && !target.use_tls {
                info!("H2C (HTTP/2 over cleartext) enabled for backend: {}", url);
            }

            // 圧縮設定のログ出力
            if compression.enabled {
                info!(
                    "Response compression enabled for backend: {} (gzip_level={}, brotli_level={})",
                    url, compression.gzip_level, compression.brotli_level
                );
            }

            // バッファリング設定のログ出力
            if buffering.is_enabled() {
                info!(
                    "Response buffering enabled for backend: {} (mode={:?}, max_memory={})",
                    url, buffering.mode, buffering.max_memory_buffer
                );
            }

            // キャッシュ設定のログ出力
            if cache.enabled {
                info!(
                    "Proxy cache enabled for backend: {} (max_memory={}, ttl={}s)",
                    url, cache.max_memory_size, cache.default_ttl_secs
                );
            }

            let group = UpstreamGroup::single(target);
            Ok(Backend::Proxy(
                Arc::new(group),
                Arc::new(security.clone()),
                Arc::new(compression.clone()),
                Arc::new(buffering.clone()),
                Arc::new(cache.clone()),
                modules_arc.clone(),
            ))
        }
        BackendConfig::ProxyUpstream { upstream, use_h2c } => {
            // Upstream グループ参照
            let group = upstream_groups.get(upstream).ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("Upstream '{}' not found", upstream),
                )
            })?;

            // ルート設定で use_h2c が指定されている場合はオーバーライド
            let group = if *use_h2c {
                Arc::new(group.with_h2c(true))
            } else {
                group.clone()
            };

            // 圧縮設定のログ出力
            if compression.enabled {
                info!("Response compression enabled for upstream: {} (gzip_level={}, brotli_level={})", 
                      upstream, compression.gzip_level, compression.brotli_level);
            }

            // バッファリング設定のログ出力
            if buffering.is_enabled() {
                info!(
                    "Response buffering enabled for upstream: {} (mode={:?}, max_memory={})",
                    upstream, buffering.mode, buffering.max_memory_buffer
                );
            }

            // キャッシュ設定のログ出力
            if cache.enabled {
                info!(
                    "Proxy cache enabled for upstream: {} (max_memory={}, ttl={}s)",
                    upstream, cache.max_memory_size, cache.default_ttl_secs
                );
            }

            Ok(Backend::Proxy(
                group.clone(),
                Arc::new(security.clone()),
                Arc::new(compression.clone()),
                Arc::new(buffering.clone()),
                Arc::new(cache.clone()),
                modules_arc.clone(),
            ))
        }
        BackendConfig::File { path, mode, index } => {
            // Routeレベルの設定のみを使用
            let security = route.security.clone().unwrap_or_default();
            let cache = route.cache.clone().unwrap_or_default();
            let metadata = fs::metadata(path).map_err(|e| {
                let error_msg = format!(
                    "Failed to access file '{}': {} (error code: {})",
                    path,
                    e,
                    e.raw_os_error().unwrap_or(-1)
                );
                io::Error::new(e.kind(), error_msg)
            })?;
            let is_dir = metadata.is_dir();
            // インデックスファイル名を Arc<str> に変換（None = デフォルトで "index.html"）
            let index_file: Option<Arc<str>> = index.as_ref().map(|s| Arc::from(s.as_str()));
            let security = Arc::new(security.clone());
            let cache = Arc::new(cache.clone());
            let open_file_cache_arc = route.open_file_cache.as_ref().map(|c| Arc::new(c.clone()));

            // キャッシュ設定のログ出力
            if cache.enabled {
                info!(
                    "File cache enabled for path: {} (max_memory={}, ttl={}s)",
                    path, cache.max_memory_size, cache.default_ttl_secs
                );
            }

            match mode.as_str() {
                "memory" => {
                    if is_dir {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "Memory mode not supported for directories",
                        ));
                    }
                    let data = fs::read(path)?;
                    let mime_type = mime_guess::from_path(path).first_or_octet_stream();

                    Ok(Backend::MemoryFile(
                        Arc::new(data),
                        Arc::from(mime_type.as_ref()),
                        security,
                        modules_arc.clone(),
                    ))
                }
                "sendfile" | "" => Ok(Backend::SendFile(
                    Arc::new(PathBuf::from(path)),
                    is_dir,
                    index_file,
                    security,
                    cache,
                    open_file_cache_arc,
                    modules_arc.clone(),
                )),
                _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid mode")),
            }
        }
        BackendConfig::Redirect {
            redirect_url,
            redirect_status,
            preserve_path,
        } => Ok(Backend::Redirect(
            Arc::from(redirect_url.as_str()),
            *redirect_status,
            *preserve_path,
            modules_arc.clone(),
        )),
    }
}

// ====================
// コマンドライン引数パース
// ====================

// ====================
// HTTP to HTTPS リダイレクトハンドラー
// ====================
//
// HTTPアクセスをHTTPSにリダイレクトするための軽量ハンドラー。
// セキュリティ上の理由から、HTTPではリダイレクトのみを行い、
// コンテンツは一切配信しません。
//
// 301 Moved Permanently を使用することで、ブラウザがリダイレクト先を
// キャッシュし、以降のアクセスでは直接HTTPSに接続します。

// HTTP_301_REDIRECT_TEMPLATE, HTTP_301_REDIRECT_SUFFIX は
// crate::constants モジュールに移動しました

/// HTTPリクエストを処理し、HTTPSにリダイレクトする
///
/// リクエストからHostヘッダーとパスを読み取り、
/// https://{host}:{port}{path} への301リダイレクトを返します。
/// ポートはHETTPS_REDIRECT_PORT（[server].listenから抽出）を使用し、
/// 443の場合はURL中のポート指定を省略します。
pub async fn handle_http_redirect(mut stream: TcpStream) {
    // リクエストを読み取るためのバッファ（ヘッダーのみなので小さめ）
    let mut buffer = vec![0u8; 4096];

    // タイムアウト付きで読み取り
    let read_result = timeout(Duration::from_secs(5), stream.read(buffer)).await;

    let (result, buf) = match read_result {
        Ok(r) => r,
        Err(_) => {
            // タイムアウト
            return;
        }
    };
    buffer = buf;

    let bytes_read = match result {
        Ok(n) if n > 0 => n,
        _ => return,
    };

    // HTTPリクエストをパース
    let mut headers = [httparse::EMPTY_HEADER; 32];
    let mut req = Request::new(&mut headers);

    let path = match req.parse(&buffer[..bytes_read]) {
        Ok(Status::Complete(_)) | Ok(Status::Partial) => req.path.unwrap_or("/"),
        Err(_) => "/",
    };

    // Hostヘッダーを取得
    let host = req
        .headers
        .iter()
        .find(|h| h.name.eq_ignore_ascii_case("Host"))
        .map(|h| std::str::from_utf8(h.value).unwrap_or(""))
        .unwrap_or("");

    // 設定から抽出したHTTPSポートを取得
    let port = HTTPS_REDIRECT_PORT.load(std::sync::atomic::Ordering::Relaxed);

    // リダイレクトURLを構築
    let redirect_url = if host.is_empty() {
        // Hostヘッダーがない場合はlocalhost
        if port == 443 {
            format!("https://localhost{}", path)
        } else {
            format!("https://localhost:{}{}", port, path)
        }
    } else {
        // ホストにポート番号が含まれている場合は除去
        let clean_host = host.split(':').next().unwrap_or(host);
        if port == 443 {
            format!("https://{}{}", clean_host, path)
        } else {
            format!("https://{}:{}{}", clean_host, port, path)
        }
    };

    // 301レスポンスを構築
    let mut response = Vec::with_capacity(
        HTTP_301_REDIRECT_TEMPLATE.len() + redirect_url.len() + HTTP_301_REDIRECT_SUFFIX.len(),
    );
    response.extend_from_slice(HTTP_301_REDIRECT_TEMPLATE);
    response.extend_from_slice(redirect_url.as_bytes());
    response.extend_from_slice(HTTP_301_REDIRECT_SUFFIX);

    // レスポンスを送信
    let _ = timeout(Duration::from_secs(5), stream.write_all(response)).await;
}

/// High-Performance Reverse Proxy Server
///
/// io_uring (monoio) と rustls を使用した高性能リバースプロキシサーバー
#[derive(Parser, Debug)]
#[command(name = "veil")]
#[command(author, version, about, long_about = None)]
pub struct CliArgs {
    /// 設定ファイルのパス
    #[arg(short, long, default_value = DEFAULT_CONFIG_PATH)]
    pub config: PathBuf,

    /// 設定ファイルの構文と内容を検証して終了（nginx -t 相当）
    #[arg(short = 't', long = "test")]
    pub test_config: bool,
}

/// 設定ファイルを検証（読み込みとバリデーションのみ、起動しない）
///
/// nginx -t 相当の機能を提供します。
/// - TOML構文のパース
/// - 設定値のバリデーション
/// - TLS証明書・秘密鍵の存在確認
// 理由付き allow: 起動・リロード・設定検証時のみ実行されるコールドパス（データプレーン非経由）。
#[allow(clippy::disallowed_methods)]
pub fn test_config_file(path: &Path) -> io::Result<()> {
    // ファイル存在確認
    if !path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("configuration file not found: {}", path.display()),
        ));
    }

    // TOMLパース
    let config_str = std::fs::read_to_string(path)?;
    let config: Config = toml::from_str(&config_str).map_err(|e| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("TOML parse error: {}", e),
        )
    })?;

    // 設定バリデーション
    validate_config(&config)?;

    // TLS証明書の存在確認
    let cert_path = Path::new(&config.tls.cert_path);
    if !cert_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("TLS certificate not found: {}", config.tls.cert_path),
        ));
    }

    // TLS秘密鍵の存在確認
    let key_path = Path::new(&config.tls.key_path);
    if !key_path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("TLS key not found: {}", config.tls.key_path),
        ));
    }

    Ok(())
}

// ====================
// ロードバランシングのテスト（F-19）
// ====================
#[cfg(test)]
mod blocklist_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    #[test]
    fn test_cidr_contains_addr_ipv4() {
        let cidr = CidrRange::parse("192.168.1.0/24").unwrap();
        assert!(cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 50))));
        assert!(cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 255))));
        assert!(!cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(192, 168, 2, 1))));
        // IPv6 は IPv4 CIDR にマッチしない
        assert!(!cidr.contains_addr(IpAddr::V6(Ipv6Addr::LOCALHOST)));
    }

    #[test]
    fn test_cidr_contains_addr_single_ipv4() {
        let cidr = CidrRange::parse("10.0.0.5").unwrap();
        assert!(cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 5))));
        assert!(!cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 6))));
    }

    #[test]
    fn test_cidr_contains_addr_ipv6() {
        let cidr = CidrRange::parse("2001:db8::/32").unwrap();
        assert!(cidr.contains_addr("2001:db8::1".parse::<IpAddr>().unwrap()));
        assert!(!cidr.contains_addr("2001:db9::1".parse::<IpAddr>().unwrap()));
        // IPv4 は IPv6 CIDR にマッチしない
        assert!(!cidr.contains_addr(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn test_global_blocklist_set_and_check() {
        // 空のときは常に false（accept ホットパスの早期 return）
        set_global_blocked_ips(&[]);
        assert!(!is_ip_blocked(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5))));

        set_global_blocked_ips(&["203.0.113.0/24".to_string(), "198.51.100.7".to_string()]);
        assert!(is_ip_blocked(IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5))));
        assert!(is_ip_blocked(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 7))));
        assert!(!is_ip_blocked(IpAddr::V4(Ipv4Addr::new(198, 51, 100, 8))));
        assert!(!is_ip_blocked(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));

        // グローバル状態を空に戻す（他テストへの影響を避ける）
        set_global_blocked_ips(&[]);
    }
}

#[cfg(test)]
mod load_balancing_tests {
    use super::*;

    fn entry(url: &str, weight: u32) -> UpstreamServerEntry {
        UpstreamServerEntry {
            url: url.to_string(),
            sni_name: None,
            use_h2c: false,
            weight,
        }
    }

    #[test]
    fn weighted_distribution_roughly_2_to_1() {
        let entries = vec![
            entry("http://10.0.0.1:80", 2),
            entry("http://10.0.0.2:80", 1),
        ];
        let group = UpstreamGroup::new(
            "wrr".into(),
            entries,
            LoadBalanceAlgorithm::Weighted,
            None,
            false,
        )
        .unwrap();

        let mut counts = [0usize; 2];
        for _ in 0..3000 {
            let s = group.select("1.2.3.4").unwrap();
            if s.target.host == "10.0.0.1" {
                counts[0] += 1;
            } else {
                counts[1] += 1;
            }
        }
        // 重み 2:1 なので server1 はおよそ server2 の2倍
        let ratio = counts[0] as f64 / counts[1] as f64;
        assert!(
            ratio > 1.7 && ratio < 2.3,
            "weighted ratio out of range: {} (counts={:?})",
            ratio,
            counts
        );
    }

    #[test]
    fn consistent_hash_same_key_same_server() {
        let entries = vec![
            entry("http://10.0.0.1:80", 1),
            entry("http://10.0.0.2:80", 1),
            entry("http://10.0.0.3:80", 1),
        ];
        let group = UpstreamGroup::new(
            "ch".into(),
            entries,
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Ip,
            },
            None,
            false,
        )
        .unwrap();

        // 同じキーは常に同じサーバーへ
        let first = group.select("192.168.1.50").unwrap().target.host.clone();
        for _ in 0..100 {
            let host = group.select("192.168.1.50").unwrap().target.host.clone();
            assert_eq!(host, first, "consistent hash not stable");
        }

        // 別のキーでも安定していること
        let second = group.select("8.8.8.8").unwrap().target.host.clone();
        for _ in 0..100 {
            let host = group.select("8.8.8.8").unwrap().target.host.clone();
            assert_eq!(host, second);
        }
    }

    #[test]
    fn consistent_hash_distributes_keys() {
        let entries = vec![
            entry("http://10.0.0.1:80", 1),
            entry("http://10.0.0.2:80", 1),
            entry("http://10.0.0.3:80", 1),
        ];
        let group = UpstreamGroup::new(
            "ch".into(),
            entries,
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Ip,
            },
            None,
            false,
        )
        .unwrap();

        let mut seen = std::collections::HashSet::new();
        for i in 0..200 {
            let key = format!("10.20.30.{}", i % 256);
            let host = group.select(&key).unwrap().target.host.clone();
            seen.insert(host);
        }
        // 150 vnodes/サーバーで十分に分散され、全サーバーが使われるはず
        assert_eq!(
            seen.len(),
            3,
            "keys not distributed across all servers: {:?}",
            seen
        );
    }

    #[test]
    fn hash_key_parse() {
        assert_eq!(HashKey::parse("ip").unwrap(), HashKey::Ip);
        assert_eq!(
            HashKey::parse("header:X-User-Id").unwrap(),
            HashKey::Header("X-User-Id".into())
        );
        assert_eq!(
            HashKey::parse("cookie:session_id").unwrap(),
            HashKey::Cookie("session_id".into())
        );
        assert!(HashKey::parse("garbage").is_err());
    }

    /// F-97: header キーの Consistent Hash が select_with_header_fn で安定
    #[test]
    fn consistent_hash_header_key_via_select_with_header_fn() {
        let entries = vec![
            entry("http://10.0.0.1:80", 1),
            entry("http://10.0.0.2:80", 1),
        ];
        let group = UpstreamGroup::new(
            "ch-hdr".into(),
            entries,
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Header("x-user-id".into()),
            },
            None,
            false,
        )
        .unwrap();

        let headers: Vec<(&[u8], &[u8])> = vec![(b"x-user-id", b"user-stable-1")];
        let first = group
            .select_with_header_fn("1.2.3.4", |name| {
                headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(name))
                    .map(|(_, v)| *v)
            })
            .unwrap()
            .target
            .host
            .clone();
        for _ in 0..20 {
            let host = group
                .select_with_header_fn("9.9.9.9", |name| {
                    headers
                        .iter()
                        .find(|(n, _)| n.eq_ignore_ascii_case(name))
                        .map(|(_, v)| *v)
                })
                .unwrap()
                .target
                .host
                .clone();
            assert_eq!(
                host, first,
                "same header value must stick to same backend regardless of client IP"
            );
        }

        // Cookie キー
        assert_eq!(
            extract_cookie_value("a=1; session_id=abc; b=2", "session_id"),
            Some("abc")
        );
    }

    #[test]
    fn weighted_entry_default_weight_is_one() {
        let s = entry("http://x:80", 1);
        assert_eq!(s.weight, 1);
    }

    #[test]
    fn round_robin_still_cycles() {
        let entries = vec![
            entry("http://10.0.0.1:80", 1),
            entry("http://10.0.0.2:80", 1),
        ];
        let group = UpstreamGroup::new(
            "rr".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();
        let a = group.select("x").unwrap().target.host.clone();
        let b = group.select("x").unwrap().target.host.clone();
        assert_ne!(a, b, "round robin should alternate");
    }
}

// ====================
// 管理 API 設定のテスト（F-20）
// ====================
#[cfg(all(test, feature = "admin"))]
mod admin_config_tests {
    use super::*;

    #[test]
    fn admin_auth_disabled_rejects() {
        let cfg = AdminConfig::default();
        assert!(!cfg.check_auth(Some("Bearer x")));
    }

    #[test]
    fn admin_auth_bearer_and_raw() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "topsecret".into(),
            allowed_ips: Vec::new(),
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.check_auth(Some("Bearer topsecret")));
        assert!(cfg.check_auth(Some("topsecret")));
        assert!(!cfg.check_auth(Some("wrong")));
        assert!(!cfg.check_auth(None));
    }

    #[test]
    fn admin_empty_secret_rejects() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: String::new(),
            allowed_ips: Vec::new(),
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(!cfg.check_auth(Some("Bearer ")));
    }

    #[test]
    fn admin_ip_allowed_empty_allows_all() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "s".into(),
            allowed_ips: Vec::new(),
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.is_ip_allowed("1.2.3.4"));
        assert!(cfg.is_ip_allowed("::1"));
    }

    #[test]
    fn admin_ip_allowed_single_ip() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "s".into(),
            allowed_ips: vec!["127.0.0.1".into()],
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.is_ip_allowed("127.0.0.1"));
        assert!(!cfg.is_ip_allowed("192.168.0.1"));
    }

    #[test]
    fn admin_ip_allowed_cidr() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "s".into(),
            allowed_ips: vec!["10.0.0.0/8".into()],
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.is_ip_allowed("10.1.2.3"));
        assert!(cfg.is_ip_allowed("10.255.255.255"));
        assert!(!cfg.is_ip_allowed("192.168.0.1"));
    }

    #[test]
    fn admin_ip_allowed_ipv6() {
        let cfg = AdminConfig {
            enabled: true,
            path_prefix: "/__admin".into(),
            secret: "s".into(),
            allowed_ips: vec!["::1".into(), "fe80::/10".into()],
            cache_purge_prefix: "/__admin/cache/purge".into(),
        };
        assert!(cfg.is_ip_allowed("::1"));
        assert!(cfg.is_ip_allowed("fe80::1"));
        assert!(!cfg.is_ip_allowed("2001:db8::1"));
    }
}

// ====================
// F-06: サーキットブレーカーと UpstreamGroup 統合テスト
// ====================
#[cfg(test)]
mod circuit_breaker_upstream_tests {
    use super::*;
    use crate::resilience::CircuitBreaker;

    fn make_entry(url: &str) -> UpstreamServerEntry {
        UpstreamServerEntry {
            url: url.to_string(),
            sni_name: None,
            use_h2c: false,
            weight: 1,
        }
    }

    fn trip_config() -> super::CircuitBreakerConfig {
        super::CircuitBreakerConfig {
            enabled: true,
            failure_threshold: 2,
            failure_window_secs: 60,
            open_duration_secs: 300,
            half_open_probes: 1,
            success_threshold: 1,
            trip_on_timeout: true,
        }
    }

    /// CBがOpenのサーバーはselect()でスキップされる（2台構成の場合）
    #[test]
    fn tripped_circuit_breaker_skips_server_with_two_servers() {
        let entries = vec![
            make_entry("http://10.0.0.1:80"),
            make_entry("http://10.0.0.2:80"),
        ];
        let mut group = UpstreamGroup::new(
            "cb-test".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        // サーバー0にCBを付与してトリップさせる
        let cb = CircuitBreaker::new(trip_config());
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open(), "CB should be open after exceeding threshold");
        group.servers[0] = group.servers[0].clone().with_circuit_breaker(Some(cb));

        // 50回 select() して、トリップしたサーバー0は選ばれないことを確認
        // （2台構成なので candidates() の healthy fallback は不要）
        for _ in 0..50 {
            let s = group.select("1.2.3.4");
            if let Some(s) = s {
                assert_ne!(
                    s.target.host, "10.0.0.1",
                    "Tripped server (10.0.0.1) should not be selected when healthy fallback exists"
                );
            }
        }
    }

    /// 全サーバーのCBがOpenの場合は healthy fallback が動作する
    ///
    /// candidates() の設計: avail が空なら healthy なサーバーにフォールバック（完全停止回避）。
    /// よって1台構成でCBがOpenでも、サーバー自体は healthy なら選択される。
    #[test]
    fn all_servers_tripped_falls_back_to_healthy() {
        let entries = vec![make_entry("http://10.0.0.1:80")];
        let mut group = UpstreamGroup::new(
            "cb-all-tripped".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        let cb = CircuitBreaker::new(trip_config());
        cb.record_failure();
        cb.record_failure();
        assert!(cb.is_open(), "CB should be open");
        group.servers[0] = group.servers[0].clone().with_circuit_breaker(Some(cb));

        // 設計上: 全 CB が Open でもサーバー自体が healthy なら fallback で選択される
        // （完全停止（None）を避けるための安全策）
        let result = group.select("1.2.3.4");
        // fallback が動作するため Some が返る（設計通り）
        assert!(
            result.is_some(),
            "Should fallback to healthy server even if all CBs are open (by design)"
        );
    }

    /// record_outcome で失敗を記録すると CB の状態が更新される
    #[test]
    fn record_outcome_updates_circuit_breaker_state() {
        let entries = vec![
            make_entry("http://10.0.0.1:80"),
            make_entry("http://10.0.0.2:80"),
        ];
        let mut group = UpstreamGroup::new(
            "cb-outcome".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        let cb = CircuitBreaker::new(trip_config());
        group.servers[0] = group.servers[0].clone().with_circuit_breaker(Some(cb));

        // トリップ前は サーバー0 が選ばれることがある
        let before_trip = (0..10)
            .filter_map(|_| group.select("1.1.1.1"))
            .any(|s| s.target.host == "10.0.0.1");
        assert!(before_trip, "Server 0 should be reachable before tripping");

        // 失敗を記録してトリップ閾値（2回）に達する
        group.record_outcome(0, false, 100);
        group.record_outcome(0, false, 100);

        // トリップ後は2台のうちサーバー1のみが選ばれるはず
        // （サーバー0は avail から除かれ、candidates が [server1] になる）
        let after_trip_server0 = (0..20)
            .filter_map(|_| group.select("1.1.1.1"))
            .filter(|s| s.target.host == "10.0.0.1")
            .count();
        assert_eq!(
            after_trip_server0, 0,
            "Tripped server (10.0.0.1) should not be selected when alternative exists"
        );
    }

    /// スライディングウィンドウ内の失敗率が閾値未満なら CB は開かない
    #[test]
    fn below_threshold_does_not_trip() {
        let entries = vec![make_entry("http://10.0.0.1:80")];
        let mut group = UpstreamGroup::new(
            "cb-below".into(),
            entries,
            LoadBalanceAlgorithm::RoundRobin,
            None,
            false,
        )
        .unwrap();

        let cb = CircuitBreaker::new(trip_config());
        group.servers[0] = group.servers[0].clone().with_circuit_breaker(Some(cb));

        // 閾値-1回の失敗では開かない
        group.record_outcome(0, false, 10);

        assert!(
            group.select("1.1.1.1").is_some(),
            "Should still select server when below failure threshold"
        );
    }
}

// ====================
// F-19: 追加ロードバランシング統合テスト
// ====================
#[cfg(test)]
mod advanced_lb_integration_tests {
    use super::*;

    fn w_entry(url: &str, weight: u32) -> UpstreamServerEntry {
        UpstreamServerEntry {
            url: url.to_string(),
            sni_name: None,
            use_h2c: false,
            weight,
        }
    }

    /// Weighted RR: weight=0 は weight=1 として扱われる（最小重みは1）
    ///
    /// 設計上、`weight.max(1)` により 0 は 1 に切り上げられる。
    #[test]
    fn zero_weight_treated_as_one() {
        let entries = vec![
            w_entry("http://10.0.0.1:80", 100), // 重み 100
            w_entry("http://10.0.0.2:80", 0),   // weight=0 → 内部で 1 として扱われる
        ];
        let group = UpstreamGroup::new(
            "wt-zero".into(),
            entries,
            LoadBalanceAlgorithm::Weighted,
            None,
            false,
        )
        .unwrap();

        let mut count1 = 0usize;
        let mut count2 = 0usize;
        for _ in 0..200 {
            if let Some(s) = group.select("1.2.3.4") {
                if s.target.host == "10.0.0.1" {
                    count1 += 1;
                } else {
                    count2 += 1;
                }
            }
        }
        // weight=100 vs weight=0(→1) → server1 が圧倒的多数
        assert!(
            count1 > count2 * 10,
            "weight=100 server should dominate weight=0(→1): {count1} vs {count2}"
        );
    }

    /// Consistent Hash: 同一キーは unhealthy サーバーが除かれても次のサーバーに転送される
    #[test]
    fn consistent_hash_falls_back_on_unhealthy() {
        let entries = vec![
            w_entry("http://10.0.0.1:80", 1),
            w_entry("http://10.0.0.2:80", 1),
            w_entry("http://10.0.0.3:80", 1),
        ];
        let group = UpstreamGroup::new(
            "ch-fallback".into(),
            entries,
            LoadBalanceAlgorithm::ConsistentHash {
                hash_key: HashKey::Ip,
            },
            None,
            false,
        )
        .unwrap();

        // 正常状態で選択できること
        let first = group.select("192.168.1.100");
        assert!(first.is_some(), "Should select a server in normal state");

        // 1台をunhealthyにしても選択できること
        group.servers[0]
            .healthy
            .store(false, std::sync::atomic::Ordering::SeqCst);
        let after = group.select("192.168.1.100");
        assert!(
            after.is_some(),
            "Should still select a server with one unhealthy"
        );
    }

    /// IpHash: 同じIPは常に同じサーバーへ（既存動作の確認）
    #[test]
    fn ip_hash_consistent_routing() {
        let entries = vec![
            w_entry("http://10.0.0.1:80", 1),
            w_entry("http://10.0.0.2:80", 1),
            w_entry("http://10.0.0.3:80", 1),
        ];
        let group = UpstreamGroup::new(
            "iphash".into(),
            entries,
            LoadBalanceAlgorithm::IpHash,
            None,
            false,
        )
        .unwrap();

        let ip = "203.0.113.42";
        let first = group.select(ip).unwrap().target.host.clone();
        for _ in 0..20 {
            let host = group.select(ip).unwrap().target.host.clone();
            assert_eq!(
                host, first,
                "IP hash should always route same IP to same server"
            );
        }
    }
}

// ====================
// F-22: HealthCheckType / HealthCheckConfig serde テスト
// ====================
#[cfg(test)]
mod health_check_type_tests {
    use super::*;
    use serde::Deserialize;

    // TOML はトップレベルにスカラー値を置けないため、ラッパーを使う
    #[derive(Deserialize)]
    struct Wrapper {
        t: HealthCheckType,
    }

    #[test]
    fn test_health_check_type_deser_http() {
        let w: Wrapper = toml::from_str("t = \"http\"").unwrap();
        assert_eq!(w.t, HealthCheckType::Http);
    }

    #[test]
    fn test_health_check_type_deser_tcp() {
        let w: Wrapper = toml::from_str("t = \"tcp\"").unwrap();
        assert_eq!(w.t, HealthCheckType::Tcp);
    }

    #[test]
    fn test_health_check_type_deser_grpc() {
        let w: Wrapper = toml::from_str("t = \"grpc\"").unwrap();
        assert_eq!(w.t, HealthCheckType::Grpc);
    }

    #[test]
    fn test_health_check_type_default() {
        assert_eq!(HealthCheckType::default(), HealthCheckType::Http);
    }

    #[test]
    fn test_health_check_config_deser_tcp() {
        let toml = r#"
check_type = "tcp"
interval_secs = 15
timeout_secs = 3
"#;
        let cfg: HealthCheckConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.check_type, HealthCheckType::Tcp);
        assert_eq!(cfg.interval_secs, 15);
        assert_eq!(cfg.timeout_secs, 3);
    }

    #[test]
    fn test_health_check_config_deser_grpc() {
        let toml = r#"
check_type = "grpc"
path = "my.service.Health"
timeout_secs = 5
use_tls = true
verify_cert = false
"#;
        let cfg: HealthCheckConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.check_type, HealthCheckType::Grpc);
        assert_eq!(cfg.path, "my.service.Health");
        assert!(cfg.use_tls);
        assert!(!cfg.verify_cert);
    }

    #[test]
    fn test_health_check_config_deser_defaults() {
        // check_type を省略すると Http になる
        let cfg: HealthCheckConfig = toml::from_str("").unwrap();
        assert_eq!(cfg.check_type, HealthCheckType::Http);
    }
}

// ====================
// F-18: L4ListenerConfig serde テスト
// ====================
#[cfg(all(test, feature = "l4-proxy"))]
mod l4_config_tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct TlsWrapper {
        t: L4TlsMode,
    }
    #[derive(Deserialize)]
    struct LbWrapper {
        t: L4LbAlgorithm,
    }

    #[test]
    fn test_l4_tls_mode_deser() {
        let none: TlsWrapper = toml::from_str("t = \"none\"").unwrap();
        let passthrough: TlsWrapper = toml::from_str("t = \"passthrough\"").unwrap();
        let terminate: TlsWrapper = toml::from_str("t = \"terminate\"").unwrap();
        assert_eq!(none.t, L4TlsMode::None);
        assert_eq!(passthrough.t, L4TlsMode::Passthrough);
        assert_eq!(terminate.t, L4TlsMode::Terminate);
    }

    #[test]
    fn test_l4_lb_algorithm_deser() {
        let rr: LbWrapper = toml::from_str("t = \"round_robin\"").unwrap();
        let lc: LbWrapper = toml::from_str("t = \"least_conn\"").unwrap();
        assert_eq!(rr.t, L4LbAlgorithm::RoundRobin);
        assert_eq!(lc.t, L4LbAlgorithm::LeastConn);
    }

    #[test]
    fn test_l4_listener_config_full_deser() {
        let toml = r#"
name = "db-proxy"
listen = "0.0.0.0:5432"
lb = "least_conn"
tls = "passthrough"
max_connections = 200
connect_timeout_secs = 5

[[upstreams]]
addr = "10.0.0.1:5432"
weight = 2

[[upstreams]]
addr = "10.0.0.2:5432"
weight = 1
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.name, "db-proxy");
        assert_eq!(cfg.listen, "0.0.0.0:5432");
        assert_eq!(cfg.lb, L4LbAlgorithm::LeastConn);
        assert_eq!(cfg.tls, L4TlsMode::Passthrough);
        assert_eq!(cfg.max_connections, 200);
        assert_eq!(cfg.connect_timeout_secs, 5);
        assert_eq!(cfg.upstreams.len(), 2);
        assert_eq!(cfg.upstreams[0].addr, "10.0.0.1:5432");
        assert_eq!(cfg.upstreams[0].weight, 2);
        assert_eq!(cfg.upstreams[1].weight, 1);
    }

    #[test]
    fn test_l4_listener_config_defaults() {
        // lb, tls, max_connections, connect_timeout_secs, idle_timeout_secs を省略したときデフォルト値になる
        let toml = r#"
name = "minimal"
listen = "0.0.0.0:9000"

[[upstreams]]
addr = "127.0.0.1:9001"
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.lb, L4LbAlgorithm::RoundRobin);
        assert_eq!(cfg.tls, L4TlsMode::None);
        assert_eq!(cfg.max_connections, 0);
        assert_eq!(cfg.connect_timeout_secs, 10);
        assert_eq!(cfg.idle_timeout_secs, 600);
        assert_eq!(cfg.upstreams[0].weight, 1);
    }

    #[test]
    fn test_l4_listener_config_idle_timeout() {
        let toml = r#"
name = "fast-idle"
listen = "0.0.0.0:8000"
idle_timeout_secs = 30

[[upstreams]]
addr = "127.0.0.1:8001"
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        assert_eq!(cfg.idle_timeout_secs, 30);
    }

    #[test]
    fn test_l4_listener_config_with_health_check() {
        let toml = r#"
name = "mysql"
listen = "0.0.0.0:3306"

[[upstreams]]
addr = "10.0.0.1:3306"

[health_check]
check_type = "tcp"
interval_secs = 10
timeout_secs = 3
"#;
        let cfg: L4ListenerConfig = toml::from_str(toml).unwrap();
        let hc = cfg.health_check.unwrap();
        assert_eq!(hc.check_type, HealthCheckType::Tcp);
        assert_eq!(hc.interval_secs, 10);
        assert_eq!(hc.timeout_secs, 3);
    }
}

// ====================
// TLS 暗号スイート設定（F-50）のテスト
// ====================

#[cfg(test)]
mod cipher_suites_tests {
    // 理由付き allow: テストコードは同期 I/O・sleep を使用してよい（データプレーン非経由）。
    #![allow(clippy::disallowed_methods)]
    use super::*;

    #[test]
    fn resolve_known_suites_preserves_order() {
        let names = vec![
            "TLS13_AES_256_GCM_SHA384".to_string(),
            "TLS13_AES_128_GCM_SHA256".to_string(),
            "TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384".to_string(),
        ];
        let suites = resolve_cipher_suites(&names).unwrap();
        assert_eq!(suites.len(), 3);
        // 設定順 = 優先度順が保持される
        for (name, suite) in names.iter().zip(suites.iter()) {
            assert_eq!(&format!("{:?}", suite.suite()), name);
        }
    }

    #[test]
    fn resolve_is_case_insensitive() {
        let names = vec!["tls13_aes_128_gcm_sha256".to_string()];
        let suites = resolve_cipher_suites(&names).unwrap();
        assert_eq!(
            format!("{:?}", suites[0].suite()),
            "TLS13_AES_128_GCM_SHA256"
        );
    }

    #[test]
    fn resolve_unknown_suite_is_error() {
        let names = vec!["TLS_RSA_WITH_RC4_128_MD5".to_string()];
        let err = resolve_cipher_suites(&names).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        // エラーメッセージに有効な候補一覧が含まれる
        assert!(err.to_string().contains("TLS13_AES_256_GCM_SHA384"));
    }

    #[test]
    fn resolve_duplicate_suite_is_error() {
        let names = vec![
            "TLS13_AES_128_GCM_SHA256".to_string(),
            "TLS13_AES_128_GCM_SHA256".to_string(),
        ];
        let err = resolve_cipher_suites(&names).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert!(err.to_string().contains("duplicate"));
    }

    #[test]
    fn resolve_empty_returns_empty() {
        assert!(resolve_cipher_suites(&[]).unwrap().is_empty());
    }

    #[test]
    fn tls_section_deserializes_cipher_suites() {
        let toml = r#"
cert_path = "/tmp/cert.pem"
key_path = "/tmp/key.pem"
cipher_suites = ["TLS13_AES_256_GCM_SHA384", "TLS13_AES_128_GCM_SHA256"]
"#;
        let section: TlsConfigSection = toml::from_str(toml).unwrap();
        assert_eq!(
            section.cipher_suites,
            vec!["TLS13_AES_256_GCM_SHA384", "TLS13_AES_128_GCM_SHA256"]
        );
    }

    #[test]
    fn tls_section_cipher_suites_defaults_to_empty() {
        let toml = r#"
cert_path = "/tmp/cert.pem"
key_path = "/tmp/key.pem"
"#;
        let section: TlsConfigSection = toml::from_str(toml).unwrap();
        assert!(section.cipher_suites.is_empty());
    }

    /// build_server_config_from_paths が cipher_suites を反映した ServerConfig を
    /// 構築できることを確認する（自己署名証明書使用）。
    #[test]
    fn build_server_config_with_custom_suites() {
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

        let suites = vec!["TLS13_AES_256_GCM_SHA384".to_string()];
        let config =
            build_server_config_from_paths(&cert_path, &key_path, false, false, &suites).unwrap();
        // ServerConfig 構築成功 = スイート解決と provider 差し替えが機能
        assert!(Arc::strong_count(&config) >= 1);

        // 不正なスイート名はエラー
        let bad = vec!["NOT_A_SUITE".to_string()];
        assert!(build_server_config_from_paths(&cert_path, &key_path, false, false, &bad).is_err());
    }
}

// ====================
// 同梱 examples/config.toml の同期検証（F-51）
// ====================

#[cfg(test)]
mod shipped_config_tests {
    // 理由付き allow: テストコードは同期 I/O・sleep を使用してよい（データプレーン非経由）。
    #![allow(clippy::disallowed_methods)]
    use super::*;

    /// リポジトリ同梱の examples/config.toml が常にパース・バリデーション可能であることを保証する。
    ///
    /// config.rs の設定構造とドキュメント（examples/config.toml）の乖離を CI で検出する。
    /// cert_path / key_path のプレースホルダーのみ、実在する自己署名証明書に差し替える。
    #[test]
    fn shipped_config_toml_parses_and_validates() {
        // 同梱リファレンス examples/config.toml は開発／CI 環境（リポジトリツリー）でのみ存在する。
        // コンテナビルド等、ビルドコンテキストに含めない環境ではファイルが無いためスキップする
        // （コンテナ実行時の設定は docker/assets/conf.d/config.toml を使用する）。
        let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples/config.toml");
        let content = match std::fs::read_to_string(&shipped) {
            Ok(c) => c,
            Err(_) => {
                eprintln!(
                    "skip shipped_config_toml_parses_and_validates: {} が存在しない環境のためスキップ",
                    shipped.display()
                );
                return;
            }
        };

        // プレースホルダー証明書を実在ファイルに差し替え
        let dir = tempfile::tempdir().unwrap();
        let cert_path = dir.path().join("cert.pem");
        let key_path = dir.path().join("key.pem");
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()]).unwrap();
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

        // File ルート例のプレースホルダー（/var/www/...）も実在ディレクトリへ差し替え
        // （バリデーションはファイル/ディレクトリの実在を確認するため）
        let www = dir.path().join("www");
        for sub in ["assets", "user", "app", "docs"] {
            std::fs::create_dir_all(www.join(sub)).unwrap();
        }
        std::fs::write(www.join("robots.txt"), "User-agent: *\n").unwrap();
        std::fs::write(www.join("index.html"), "<html></html>").unwrap();

        let content = content
            .replace("/path/to/cert.pem", &cert_path.to_string_lossy())
            .replace("/path/to/key.pem", &key_path.to_string_lossy())
            .replace("/var/www", &www.to_string_lossy());

        let test_path = dir.path().join("config.toml");
        std::fs::write(&test_path, content).unwrap();

        // nginx -t 相当の検証（TOML パース + バリデーション + 証明書存在確認）
        test_config_file(&test_path).expect("shipped config.toml must parse and validate");
    }
}
