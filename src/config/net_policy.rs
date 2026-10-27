//! kTLS の設定情報・IP 制限（CIDR）・WebSocket のポーリング設定
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

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
pub(super) fn default_max_body_size() -> usize {
    MAX_BODY_SIZE
}
pub(super) fn default_max_header_size() -> usize {
    MAX_HEADER_SIZE
}
pub(super) fn default_client_header_timeout() -> u64 {
    30
}
pub(super) fn default_client_body_timeout() -> u64 {
    30
}
pub(super) fn default_backend_connect_timeout() -> u64 {
    10
}
pub(super) fn default_max_idle_connections() -> usize {
    BACKEND_POOL_MAX_IDLE_PER_HOST
}
pub(super) fn default_idle_connection_timeout() -> u64 {
    BACKEND_POOL_IDLE_TIMEOUT_SECS
}

// WebSocket ポーリング設定のデフォルト値
pub(super) fn default_websocket_poll_timeout_ms() -> u64 {
    1
}
pub(super) fn default_websocket_poll_max_timeout_ms() -> u64 {
    100
}
pub(super) fn default_websocket_backoff_multiplier() -> f64 {
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
        if let Some(ipv6) = ip_str
            .parse::<std::net::Ipv6Addr>()
            .ok()
            .map(u128::from)
            .or_else(|| Self::parse_ipv6(ip_str))
        {
            let prefix = if prefix_len == 255 { 128 } else { prefix_len };
            if prefix > 128 {
                return None;
            }
            // IPv4 射影の範囲（::ffff:0:0/96 より細かいもの）は IPv4 の範囲として持つ（照合側が IPv4 に直すため）
            if prefix >= 96 && (ipv6 >> 32) == 0xffff {
                let v4_prefix = prefix - 96;
                return Some(CidrRange {
                    network: (ipv6 & 0xffff_ffff) & Self::mask_v4(v4_prefix),
                    prefix_len: v4_prefix,
                    is_ipv6: false,
                });
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
        // 標準の形なら IpAddr を経由して、IPv4 射影の IPv6（::ffff:a.b.c.d）を IPv4 として見る（F-236 の F8）
        if let Ok(addr) = ip.parse::<std::net::IpAddr>() {
            return self.contains_addr(addr);
        }
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
        // IPv4 射影の IPv6（デュアルスタックのソケットで受けた IPv4 のクライアント）は IPv4 として比べる。
        // そのままだと `10.0.0.0/8` の拒否を `::ffff:10.0.0.1` で素通りできた（F-236 の F8）
        match ip.to_canonical() {
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

    /// 転送ヘッダ（`X-Forwarded-For`・`X-Real-IP`・`X-Forwarded-Proto`・`Forwarded`）の扱い（B-232）
    ///
    /// - `"append"`（既定）: `trusted_proxies` に入らないクライアントが送ったこれらは上流へ送らず、
    ///   `X-Forwarded-For` にクライアントの IP を入れる。`trusted_proxies` からの要求は受けた値を残し、
    ///   `X-Forwarded-For` にクライアントの IP を足す（別の行として。リストとしては末尾に連なる）。
    ///   `X-Real-IP` は足さない（必要なら `add_request_headers` に `$client_ip` で書く）。
    /// - `"passthrough"`: v0.10.0 より前の動き（受けた値をそのまま送り、何も足さない）。
    ///
    /// `add_request_headers` に同じ名前があればそちらを使う。
    #[serde(default)]
    pub forwarded_headers: ForwardedHeadersMode,

    /// 転送ヘッダを信頼する送信元（IP/CIDR）。前段のロードバランサ等（B-232）
    #[serde(default)]
    pub trusted_proxies: Vec<String>,

    /// `trusted_proxies` を解析したもの（初回に作る）
    #[serde(skip)]
    pub trusted_proxy_ranges: std::sync::OnceLock<Vec<CidrRange>>,

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

    /// 実際の応答（プリフライト以外）に足す CORS のヘッダ（F-273）。`[route.traffic.cors]` のあるルートで、
    /// 要求の `Origin` ごとに設定ロード時に作った `Backend` の写しだけが持つ（それ以外は `None`）
    #[serde(skip)]
    pub cors_actual: Option<Arc<crate::route_cors::CorsActual>>,

    /// レート制限の表の鍵（F-273）。0 ならこの設定のアドレス。CORS の写しは元の設定のアドレスを持ち、
    /// オリジンごとに枠が分かれないようにする
    #[serde(skip)]
    pub rate_limit_zone: usize,
}

/// 転送ヘッダの扱い（`[route.security] forwarded_headers`。B-232）
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ForwardedHeadersMode {
    /// 信頼しない送信元の転送ヘッダを捨て、クライアントの IP を入れる
    #[default]
    Append,
    /// 受けたまま送る（何も足さない）
    Passthrough,
}

impl SecurityConfig {
    /// 送信元が `trusted_proxies` に入るか（B-232）。リストが空なら解析もしない。
    #[inline]
    pub fn is_trusted_proxy(&self, client_ip: &str) -> bool {
        if self.trusted_proxies.is_empty() {
            return false;
        }
        let ranges = self.trusted_proxy_ranges.get_or_init(|| {
            self.trusted_proxies
                .iter()
                .filter_map(|s| CidrRange::parse(s.trim()))
                .collect()
        });
        match client_ip.parse::<std::net::IpAddr>() {
            Ok(addr) => ranges.iter().any(|r| r.contains_addr(addr)),
            Err(_) => false,
        }
    }

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
            forwarded_headers: ForwardedHeadersMode::default(),
            trusted_proxies: Vec::new(),
            trusted_proxy_ranges: std::sync::OnceLock::new(),
            add_response_headers: HashMap::new(),
            remove_response_headers: Vec::new(),
            // WebSocket設定
            websocket_poll_mode: WebSocketPollMode::default(),
            websocket_poll_timeout_ms: default_websocket_poll_timeout_ms(),
            websocket_poll_max_timeout_ms: default_websocket_poll_max_timeout_ms(),
            websocket_poll_backoff_multiplier: default_websocket_backoff_multiplier(),
            cors_actual: None,
            rate_limit_zone: 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ipv4_mapped_ipv6_matches_ipv4_ranges() {
        let r = CidrRange::parse("10.0.0.0/8").unwrap();
        assert!(r.contains("::ffff:10.0.0.1"));
        assert!(r.contains_addr("::ffff:10.1.2.3".parse().unwrap()));
        assert!(!r.contains("::ffff:11.0.0.1"));
        let f = IpFilter::from_lists(&[], &["10.0.0.0/8".to_string()]);
        assert!(!f.is_allowed("::ffff:10.0.0.1"));
        // 射影の形で書いた範囲も IPv4 のクライアントに効く
        let m = CidrRange::parse("::ffff:192.168.0.0/112").unwrap();
        assert!(m.contains("192.168.3.4"));
        assert!(!m.contains("192.169.0.1"));
        // 通常の IPv6 は変わらない
        let v6 = CidrRange::parse("2001:db8::/32").unwrap();
        assert!(v6.contains("2001:db8::1"));
        assert!(!v6.contains("10.0.0.1"));
    }
}
