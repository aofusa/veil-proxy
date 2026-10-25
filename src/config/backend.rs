//! ランタイムの Backend
//!
//! F-233 で `src/config.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// ランタイムBackend
// ====================

#[derive(Clone)]
pub enum Backend {
    /// 固定応答（F-219: `type = "Respond"`・フォルトの中断・CORS のプリフライト）
    Respond(Arc<crate::route_traffic::FixedResponse>),
    /// Proxy バックエンド（ロードバランシング対応）
    /// - `Arc<UpstreamGroup>`: アップストリームグループ（単一または複数バックエンド）
    /// - `Arc<SecurityConfig>`: ルートごとのセキュリティ設定
    /// - `Arc<CompressionConfig>`: 圧縮設定
    /// - Arc<buffering::BufferingConfig>: バッファリング設定
    /// - Arc<cache::CacheConfig>: キャッシュ設定
    Proxy(
        Arc<UpstreamGroup>,
        Arc<SecurityConfig>,
        Arc<CompressionConfig>,
        Arc<buffering::BufferingConfig>,
        Arc<cache::CacheConfig>,
        /// このバックエンドに適用するWASMモジュール参照のリスト（F-148: 名前 + ルート単位
        /// で解決済みの実効 plugin configuration）
        Option<Arc<Vec<ModuleRef>>>,
    ),
    /// MemoryFile バックエンド
    /// - `Arc<Vec<u8>>`: ファイルコンテンツ
    /// - `Arc<str>`: MIMEタイプ
    /// - `Arc<SecurityConfig>`: ルートごとのセキュリティ設定
    MemoryFile(
        Arc<Vec<u8>>,
        Arc<str>,
        Arc<SecurityConfig>,
        /// このバックエンドに適用するWASMモジュール参照のリスト（F-148）
        Option<Arc<Vec<ModuleRef>>>,
    ),
    /// SendFile バックエンド
    /// - `Arc<PathBuf>`: ベースパス（設定に書かれた原形。パストラバーサル検査の
    ///   フォールバック・capsicum のルート照合キーとして使う）
    /// - bool: ディレクトリかどうか
    /// - `Option<Arc<str>>`: インデックスファイル名（None = "index.html"）
    /// - `Arc<SecurityConfig>`: ルートごとのセキュリティ設定
    /// - Arc<cache::CacheConfig>: キャッシュ設定
    /// - Option<Arc<cache::OpenFileCacheConfig>>: OpenFileCache設定（ルーティングごと）
    /// - `Option<Arc<Path>>`: base_path の canonical 形（F-145、`load_backend` で
    ///   config ロード時に一度だけ解決。ディレクトリルートでのみ Some になり得る。
    ///   解決失敗時は None＝リクエスト時は生の base_path へフォールバック）
    /// - Option<Arc<cache::StaticContentCacheRouteConfig>>: 静的コンテンツキャッシュ
    ///   設定（F-146、ルーティングごとのグローバル上書き）
    SendFile(
        Arc<PathBuf>,
        bool,
        Option<Arc<str>>,
        Arc<SecurityConfig>,
        Arc<cache::CacheConfig>,
        Option<Arc<cache::OpenFileCacheConfig>>,
        Option<Arc<Path>>,
        Option<Arc<cache::StaticContentCacheRouteConfig>>,
        /// このバックエンドに適用するWASMモジュール参照のリスト（F-148）
        Option<Arc<Vec<ModuleRef>>>,
    ),
    /// Redirect バックエンド
    /// - `Arc<str>`: リダイレクト先URL
    /// - u16: ステータスコード（301, 302, 307, 308）
    /// - bool: 元のパスを保持するか
    Redirect(
        Arc<str>,
        u16,
        bool,
        /// このバックエンドに適用するWASMモジュール参照のリスト（F-148）
        Option<Arc<Vec<ModuleRef>>>,
        /// ルートの `[route.security]`（B-133 より前は持たず、既定値で検査していた）
        Arc<SecurityConfig>,
    ),
    /// ネイティブハンドラ（API ライブラリ、F-188）。設定ロード時に名前解決済みのエントリ・
    /// ルートのセキュリティ設定・WASM モジュール参照。
    #[cfg(feature = "handler")]
    Handler(
        Arc<crate::api::handler::HandlerEntry>,
        Arc<SecurityConfig>,
        Option<Arc<Vec<ModuleRef>>>,
        /// F-270: 応答の圧縮（ルートの `[route.compression]`。無効・ルートが `compress = false` なら None）
        Option<Arc<CompressionConfig>>,
    ),
}

// F-159: `Route` に `resolved_backend: Option<Backend>` を追加したため `Route` の
// `#[derive(Debug)]` を満たす必要がある。内部の `UpstreamGroup` 等が Debug 未実装のため
// derive は使えず、バリアント名のみを出す最小実装にする（ログ／assert 用途で十分）。
impl std::fmt::Debug for Backend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Backend::Proxy(..) => write!(f, "Backend::Proxy(..)"),
            Backend::MemoryFile(..) => write!(f, "Backend::MemoryFile(..)"),
            Backend::SendFile(..) => write!(f, "Backend::SendFile(..)"),
            Backend::Redirect(..) => write!(f, "Backend::Redirect(..)"),
            Backend::Respond(..) => write!(f, "Backend::Respond(..)"),
            #[cfg(feature = "handler")]
            Backend::Handler(..) => write!(f, "Backend::Handler(..)"),
        }
    }
}

impl Backend {
    /// このバックエンドのセキュリティ設定を取得
    #[inline]
    pub fn security(&self) -> &SecurityConfig {
        match self {
            Backend::Proxy(_, security, _, _, _, _) => security,
            Backend::MemoryFile(_, _, security, _) => security,
            Backend::SendFile(_, _, _, security, _, _, _, _, _) => security,
            Backend::Redirect(_, _, _, _, security) => security,
            Backend::Respond(r) => &r.security,
            #[cfg(feature = "handler")]
            Backend::Handler(_, security, _, _) => security,
        }
    }

    /// セキュリティ設定だけを差し替えた写し（F-273: CORS のオリジンごとの写しを設定ロード時に作る）。
    /// 固定応答はセキュリティ設定をそのまま使う（CORS のヘッダは応答のヘッダへ組み込む）
    pub fn with_security(&self, sec: Arc<SecurityConfig>) -> Backend {
        match self {
            Backend::Proxy(a, _, c, d, e, f) => {
                Backend::Proxy(a.clone(), sec, c.clone(), d.clone(), e.clone(), f.clone())
            }
            Backend::MemoryFile(a, b, _, d) => {
                Backend::MemoryFile(a.clone(), b.clone(), sec, d.clone())
            }
            Backend::SendFile(a, b, c, _, e, f, g, h, i) => Backend::SendFile(
                a.clone(),
                *b,
                c.clone(),
                sec,
                e.clone(),
                f.clone(),
                g.clone(),
                h.clone(),
                i.clone(),
            ),
            Backend::Redirect(a, b, c, d, _) => {
                Backend::Redirect(a.clone(), *b, *c, d.clone(), sec)
            }
            Backend::Respond(r) => Backend::Respond(r.clone()),
            #[cfg(feature = "handler")]
            Backend::Handler(a, _, c, d) => Backend::Handler(a.clone(), sec, c.clone(), d.clone()),
        }
    }

    /// このバックエンドに適用するWASMモジュール参照のリストを取得
    #[inline]
    /// F-43/F-148: WASM モジュールリストを Arc 共有で取得する（リクエストごとの deep copy 排除）。
    pub fn modules_arc(&self) -> Option<&Arc<Vec<ModuleRef>>> {
        match self {
            Backend::Proxy(_, _, _, _, _, modules) => modules.as_ref(),
            Backend::MemoryFile(_, _, _, modules) => modules.as_ref(),
            Backend::SendFile(_, _, _, _, _, _, _, _, modules) => modules.as_ref(),
            Backend::Redirect(_, _, _, modules, _) => modules.as_ref(),
            Backend::Respond(_) => None,
            #[cfg(feature = "handler")]
            Backend::Handler(_, _, modules, _) => modules.as_ref(),
        }
    }

    pub fn modules(&self) -> Option<&[ModuleRef]> {
        match self {
            Backend::Proxy(_, _, _, _, _, modules) => {
                modules.as_deref().map(|v: &Vec<ModuleRef>| v.as_slice())
            }
            Backend::MemoryFile(_, _, _, modules) => {
                modules.as_deref().map(|v: &Vec<ModuleRef>| v.as_slice())
            }
            Backend::SendFile(_, _, _, _, _, _, _, _, modules) => {
                modules.as_deref().map(|v: &Vec<ModuleRef>| v.as_slice())
            }
            Backend::Redirect(_, _, _, modules, _) => {
                modules.as_deref().map(|v: &Vec<ModuleRef>| v.as_slice())
            }
            Backend::Respond(_) => None,
            #[cfg(feature = "handler")]
            Backend::Handler(_, _, modules, _) => {
                modules.as_deref().map(|v: &Vec<ModuleRef>| v.as_slice())
            }
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
    /// HTTPS 上流で HTTP/2 を使うか（F-175）。`use_tls` のときだけ意味を持つ。
    pub http2: UpstreamHttp2,
    /// 事前パース済みの接続先 `SocketAddr`（`host` が IP アドレスリテラルの場合のみ `Some`）。
    /// 設定ロード時に一度だけ解決しておくことで、ホットパス（接続確立のたびに実行される
    /// `to_socket_addrs()`。内部で `Vec` 確保が発生し、`host` がホスト名の場合は
    /// 同期 `getaddrinfo` によるブロッキング DNS 解決も伴う）を排除する。
    /// `host` がホスト名の場合は `None` となり、従来どおり `TcpStream::connect_str` 経由で
    /// 名前解決する。
    pub socket_addr: Option<std::net::SocketAddr>,
    /// UDS バックエンド（F-170）。`Some` のときソケットパス文字列を持つ。
    /// `host`/`port` は Host ヘッダ・SNI・表示用の論理値（既定 `"localhost"` / `0`）。
    ///
    /// `Arc<str>`（`Arc<PathBuf>` ではなく）にしているのは (1) `load_backend` が
    /// リクエストごとに `ProxyTarget` を clone しうる（F-159 で `resolved_backend` に
    /// したが単一 URL プロキシのフォールバック経路は残っている）ため参照カウント増分の
    /// みでディープコピーを避けたい、(2) 接続先表記（`unix:<path>`）の組み立て
    /// （`HostPortStr::unix`）に必要なのは UTF-8 文字列であり、`PathBuf` を経由すると
    /// `to_str()` の変換が毎回挟まる、の 2 点から。非 unix ターゲットでは常に `None`。
    pub unix_path: Option<Arc<str>>,
}

impl ProxyTarget {
    pub fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = if let Some(rest) = url.strip_prefix("https://") {
            (true, rest)
        } else {
            let rest = url.strip_prefix("http://")?;
            (false, rest)
        };

        // F-170: `http(s)://unix:<socket-path>[:<path-prefix>]`（nginx の
        // `proxy_pass http://unix:/path:/uri;` と同じ表記）。UDS 経路は host:port の
        // 分割ロジックと独立させ、専用のパース関数へ委譲する。
        if let Some(uds_rest) = rest.strip_prefix("unix:") {
            return Self::parse_unix(scheme, uds_rest);
        }

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

        // host が IP アドレスリテラルであれば、設定ロード時に一度だけ SocketAddr を
        // 構築しておく（ホットパスでのブロッキング DNS 解決・to_socket_addrs() の
        // Vec 確保を排除するため）。IPv6 リテラルは `[::1]` のように角括弧付きで
        // 保持され得るため、判定前に前後の `[` `]` を剥がす。
        let ip_literal = host
            .strip_prefix('[')
            .and_then(|s| s.strip_suffix(']'))
            .unwrap_or(&host);
        let socket_addr = ip_literal
            .parse::<std::net::IpAddr>()
            .ok()
            .map(|ip| std::net::SocketAddr::new(ip, port));

        Some(ProxyTarget {
            host,
            port,
            use_tls: scheme,
            path_prefix: path.to_string(),
            sni_name: None,
            use_h2c: false, // デフォルトでは無効
            http2: UpstreamHttp2::default(),
            socket_addr,
            unix_path: None,
        })
    }

    /// UDS 表記 (`unix:<socket-path>[:<path-prefix>]`、`unix:` 接頭辞は既に剥がれている)
    /// をパースする（F-170）。
    ///
    /// ソケットパスの後ろの **最後の `:`** を境界候補とし、その直後が `/` で始まる
    /// ときだけパスプレフィックスとして解釈する（省略時は `/`）。ソケットパスに `:`
    /// は使えない（区切りと衝突するため）。ソケットパスが空なら設定エラーとして
    /// `None` を返す。
    #[cfg(unix)]
    fn parse_unix(scheme: bool, rest: &str) -> Option<Self> {
        let (socket_path, path_prefix) = match rest.rfind(':') {
            Some(idx) if rest[idx + 1..].starts_with('/') => (&rest[..idx], &rest[idx + 1..]),
            _ => (rest, "/"),
        };
        if socket_path.is_empty() {
            return None;
        }
        Some(ProxyTarget {
            host: "localhost".to_string(),
            port: 0,
            use_tls: scheme,
            path_prefix: path_prefix.to_string(),
            sni_name: None,
            use_h2c: false,
            http2: UpstreamHttp2::default(),
            socket_addr: None,
            unix_path: Some(Arc::from(socket_path)),
        })
    }

    /// 非 unix プラットフォームでは UDS 表記を受理しない（F-170）。
    #[cfg(not(unix))]
    fn parse_unix(_scheme: bool, _rest: &str) -> Option<Self> {
        None
    }

    /// UDS バックエンドかどうか（F-170）。
    #[inline]
    pub fn is_unix(&self) -> bool {
        self.unix_path.is_some()
    }

    /// 接続先表記。UDS なら `unix:<path>`、それ以外は従来どおり `host:port`
    /// （F-170）。プールキー・ログ・メトリクス・Consistent Hash のノード ID の
    /// 唯一の入口として使う。
    #[inline]
    pub(crate) fn conn_addr(&self) -> crate::http_utils::HostPortStr {
        match &self.unix_path {
            Some(p) => crate::http_utils::HostPortStr::unix(p.as_ref()),
            None => crate::http_utils::HostPortStr::new(&self.host, self.port),
        }
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

    /// HTTPS 上流の HTTP/2 設定を変更したコピーを作成（F-175）
    pub fn with_http2(mut self, http2: UpstreamHttp2) -> Self {
        self.http2 = http2;
        self
    }

    /// HTTPS 上流で HTTP/2 を試すか（F-175）。平文・`"off"` では `false`。
    #[inline]
    pub fn h2_over_tls(&self) -> bool {
        self.use_tls && self.http2 != UpstreamHttp2::Off
    }

    /// TLS接続時に使用するSNI名を取得
    #[inline]
    pub fn sni(&self) -> &str {
        self.sni_name.as_deref().unwrap_or(&self.host)
    }

    /// デフォルトポートかどうかを判定
    ///
    /// UDS バックエンド（F-170）は常に `true`（Host ヘッダへ `:0` を付けない）。
    #[inline]
    pub fn is_default_port(&self) -> bool {
        if self.is_unix() {
            return true;
        }
        if self.use_tls {
            self.port == 443
        } else {
            self.port == 80
        }
    }
}

#[cfg(test)]
pub(super) mod proxy_target_socket_addr_tests {
    use super::*;
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};

    #[test]
    fn socket_addr_is_some_for_ipv4_literal_host() {
        let t = ProxyTarget::parse("http://127.0.0.1:8080/").unwrap();
        assert_eq!(
            t.socket_addr,
            Some(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
                8080
            ))
        );
    }

    #[test]
    fn socket_addr_is_some_for_ipv4_literal_host_default_port() {
        let t = ProxyTarget::parse("https://10.0.0.1/api").unwrap();
        assert_eq!(
            t.socket_addr,
            Some(SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 443))
        );
    }

    #[test]
    fn socket_addr_is_none_for_hostname() {
        let t = ProxyTarget::parse("http://example.com:8080/").unwrap();
        assert_eq!(t.socket_addr, None);
        assert_eq!(t.host, "example.com");
    }

    /// IPv6 リテラル `[::1]:8080` は、既存の `host_port.find(':')` による
    /// ホスト/ポート分割ロジック（本タスクでは変更禁止）が角括弧を考慮しないため、
    /// 最初のコロンをポート区切りと誤認し `ProxyTarget::parse` 自体が `None` を返す
    /// （既存の挙動であり、本変更で新たに壊れたものではない）。
    /// socket_addr 側の角括弧除去処理はこの既存の制約下では到達しないが、将来
    /// host/port 分割ロジックが IPv6 に対応した場合に備えて安全側の実装としている。
    #[test]
    fn ipv6_bracket_literal_is_rejected_by_existing_split_logic() {
        assert!(ProxyTarget::parse("http://[::1]:8080/").is_none());
        assert!(ProxyTarget::parse("http://[::1]/").is_none());
    }

    // ==== F-170: UDS バックエンド ====

    #[cfg(unix)]
    #[test]
    fn parse_unix_without_path_prefix_defaults_to_slash() {
        let t = ProxyTarget::parse("http://unix:/run/app.sock").unwrap();
        assert!(t.is_unix());
        assert_eq!(t.unix_path.as_deref(), Some("/run/app.sock"));
        assert_eq!(t.path_prefix, "/");
        assert_eq!(t.host, "localhost");
        assert_eq!(t.port, 0);
        assert!(!t.use_tls);
        assert_eq!(t.socket_addr, None);
        assert!(t.is_default_port());
    }

    #[cfg(unix)]
    #[test]
    fn parse_unix_with_path_prefix() {
        let t = ProxyTarget::parse("http://unix:/run/app.sock:/api").unwrap();
        assert!(t.is_unix());
        assert_eq!(t.unix_path.as_deref(), Some("/run/app.sock"));
        assert_eq!(t.path_prefix, "/api");
    }

    #[cfg(unix)]
    #[test]
    fn parse_unix_https_scheme_sets_use_tls() {
        let t = ProxyTarget::parse("https://unix:/run/app.sock").unwrap();
        assert!(t.is_unix());
        assert!(t.use_tls);
    }

    #[cfg(unix)]
    #[test]
    fn parse_unix_rejects_empty_socket_path() {
        assert!(ProxyTarget::parse("http://unix:").is_none());
        // 最後の ':' 以降が '/' で始まらない（`:8080` のような数字）場合は
        // 全体がソケットパスとして扱われるため空判定にはならないが、コロンの
        // 直後が空文字列のケースだけを空扱いとする（"unix::/api" → ソケットパス空）。
        assert!(ProxyTarget::parse("http://unix::/api").is_none());
    }

    #[cfg(unix)]
    #[test]
    fn parse_unix_colon_boundary_requires_slash_after_last_colon() {
        // 最後の ':' の直後が '/' で始まらない場合は境界とみなさず、全体をソケット
        // パスとして扱う（ソケットパスに ':' が含まれるケースは非対応の明文化）。
        let t = ProxyTarget::parse("http://unix:/run/app.sock:8080").unwrap();
        assert_eq!(t.unix_path.as_deref(), Some("/run/app.sock:8080"));
        assert_eq!(t.path_prefix, "/");
    }

    #[cfg(not(unix))]
    #[test]
    fn parse_unix_notation_rejected_on_non_unix() {
        assert!(ProxyTarget::parse("http://unix:/run/app.sock").is_none());
    }

    /// F-170: `conn_addr()` は TCP（`unix_path = None`）で
    /// `HostPortStr::new(host, port)` と 1 バイトも変わらない（プールキー・
    /// Consistent Hash ノード ID の再利用を静かに壊さないための不変条件）。
    #[test]
    fn conn_addr_matches_host_port_str_for_tcp_target() {
        let t = ProxyTarget::parse("http://backend.example.com:8080/").unwrap();
        let expected = crate::http_utils::HostPortStr::new(&t.host, t.port);
        assert_eq!(t.conn_addr().as_str(), expected.as_str());
    }

    #[cfg(unix)]
    #[test]
    fn conn_addr_is_unix_prefixed_for_uds_target() {
        let t = ProxyTarget::parse("http://unix:/run/app.sock").unwrap();
        assert_eq!(t.conn_addr().as_str(), "unix:/run/app.sock");
    }
}
