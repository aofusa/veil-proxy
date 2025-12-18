//! # High-Performance Reverse Proxy Server
//!
//! io_uring (monoio) と rustls + ktls2 を使用した高性能リバースプロキシサーバー。
//!
//! ## 特徴
//!
//! - **非同期I/O**: monoio (io_uring) による効率的なI/O処理
//! - **TLS**: rustls によるPure Rust TLS実装
//! - **kTLS**: rustls + ktls2 によるカーネルTLSオフロード対応
//! - **コネクションプール**: バックエンド接続の再利用
//! - **バッファプール**: メモリアロケーションの削減
//! - **Keep-Alive**: HTTP/1.1 Keep-Alive完全サポート
//!
//! ## kTLS（Kernel TLS）サポート
//!
//! ### 概要
//!
//! kTLSはLinuxカーネルの機能で、TLSデータ転送フェーズの暗号化/復号化を
//! カーネルレベルで行うことにより、以下のパフォーマンス向上を実現します：
//!
//! | 項目 | 効果 |
//! |------|------|
//! | CPU使用率 | 20-40%削減（高負荷時） |
//! | スループット | 最大2倍向上 |
//! | レイテンシ | コンテキストスイッチ削減 |
//! | ゼロコピー | sendfile + TLS暗号化 |
//!
//! ### 有効化方法
//!
//! kTLSはrustls + ktls2経由でサポートされています。
//!
//! ```bash
//! # 1. カーネルモジュールのロード
//! sudo modprobe tls
//!
//! # 2. ktlsフィーチャー付きでビルド
//! cargo build --release --features ktls
//!
//! # 3. 設定ファイルで有効化
//! # config.toml:
//! # [tls]
//! # cert_path = "cert.pem"
//! # key_path = "key.pem"
//! # ktls_enabled = true
//! ```
//!
//! ### 要件
//!
//! - Linux 5.15以上（推奨）
//! - `tls`カーネルモジュールがロード済み
//! - AES-GCM暗号スイート（TLS 1.2/1.3）
//! - ktlsフィーチャーでビルド（`--features ktls`）
//!
//! ### セキュリティ考慮事項
//!
//! | リスク | 緩和策 |
//! |--------|--------|
//! | カーネルバグ | カーネルバージョン固定、定期的なパッチ適用 |
//! | セッションキー露出 | TLSハンドシェイクはrustlsで実行 |
//! | DoS攻撃 | カーネルリソース監視、レート制限 |
//! | NICファームウェア脆弱性 | ハードウェアオフロード無効化オプション |
//!
//! ### パフォーマンス測定
//!
//! kTLSの効果を測定するには：
//!
//! ```bash
//! # 1. ベースライン（kTLS無効 / rustls使用）
//! cargo build --release
//! ./target/release/zerocopy-server &
//! wrk -t4 -c100 -d30s https://localhost/
//!
//! # 2. kTLS有効（rustls + ktls2使用）
//! cargo build --release --features ktls
//! # config.tomlでktls_enabled = true
//! ./target/release/zerocopy-server &
//! wrk -t4 -c100 -d30s https://localhost/
//!
//! # CPU使用率の比較
//! # スループット（req/sec）の比較
//! ```
//!
//! ### 参考資料
//!
//! - [Linux Kernel TLS](https://docs.kernel.org/networking/tls.html)
//! - [rustls](https://github.com/rustls/rustls): Pure Rust TLS実装
//! - [ktls2](https://crates.io/crates/ktls2): rustls用kTLS統合クレート

use mimalloc::MiMalloc;

#[global_allocator]
static GLOBAL: MiMalloc = MiMalloc;

// ktls_rustls モジュール（kTLS 対応）
#[cfg(feature = "ktls")]
mod ktls_rustls;

use httparse::{Request, Status, Header};
use monoio::fs::OpenOptions;
use monoio::io::{AsyncReadRent, AsyncWriteRentExt};
use monoio::net::{TcpListener, TcpStream};
use monoio::RuntimeBuilder;
use monoio::time::timeout;
use serde::Deserialize;
use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fs;
use std::fs::File;
use std::io;
use std::io::BufReader;
use std::net::SocketAddr;
#[cfg(target_os = "linux")]
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(target_os = "linux")]
use std::sync::atomic::AtomicUsize;
use std::thread;
use std::time::Duration;
use ftlog::{info, error, warn};
use memchr::memchr3;
use time::OffsetDateTime;
use arc_swap::ArcSwap;
use once_cell::sync::Lazy;

// rustls 共通インポート
use rustls::ServerConfig;
use rustls::crypto::CryptoProvider;
use rustls_pemfile::{certs, private_key};

// ktls_rustls（kTLS 対応）
#[cfg(feature = "ktls")]
use ktls_rustls::{RustlsAcceptor, RustlsConnector, KtlsServerStream, KtlsClientStream};

// ====================
// TLS ストリーム型エイリアス
// ====================
// 
// ktls フィーチャーの有無に応じて、使用する TLS ストリーム型を切り替えます。
// kTLS 有効時は ktls_rustls モジュールの型を使用し、
// 無効時はシンプルな rustls ラッパーを使用します。

#[cfg(feature = "ktls")]
type ServerTls = KtlsServerStream;

#[cfg(feature = "ktls")]
type ClientTls = KtlsClientStream;

// kTLS 無効時は直接 rustls を使用するシンプルなラッパー
#[cfg(not(feature = "ktls"))]
mod simple_tls;

#[cfg(not(feature = "ktls"))]
type ServerTls = simple_tls::SimpleTlsServerStream;

#[cfg(not(feature = "ktls"))]
type ClientTls = simple_tls::SimpleTlsClientStream;

// ====================
// kTLS設定情報
// ====================
//
// kTLSはs2n-tls経由でサポートされています。
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
}

// ====================
// Huge Pages (Large OS Pages) 設定
// ====================
//
// mimallocでHuge Pages（2MB）を優先使用することで、
// TLB（Translation Lookaside Buffer）ミスを削減し、
// パフォーマンスを5-10%向上させます。
//
// 特にkTLS/splice時のカーネル連携で効果的です。
//
// ## 要件（Linux）
// - /proc/sys/vm/nr_hugepages に十分な値を設定
//   例: echo 128 | sudo tee /proc/sys/vm/nr_hugepages
// - または /etc/sysctl.conf に vm.nr_hugepages=128 を追加
//
// ## コンテナ環境
// Docker/K8sでは以下の設定が必要な場合があります：
// - --cap-add=SYS_ADMIN または --privileged
// - ホスト側でHuge Pagesを事前に確保
//

/// Huge Pages の可用性情報
#[derive(Debug)]
struct HugePagesInfo {
    /// Huge Pagesが利用可能かどうか
    available: bool,
    /// 確保済みHuge Pages総数
    total: u64,
    /// 空きHuge Pages数
    free: u64,
    /// Huge Pagesサイズ（KB）
    page_size_kb: u64,
}

/// /proc/meminfo から Huge Pages 情報を取得
/// 
/// Linux以外の環境やファイルアクセスに失敗した場合は
/// available=false を返します。
#[cfg(target_os = "linux")]
fn check_huge_pages_availability() -> HugePagesInfo {
    use std::fs::File as StdFile;
    use std::io::{BufRead, BufReader as StdBufReader};
    
    let mut info = HugePagesInfo {
        available: false,
        total: 0,
        free: 0,
        page_size_kb: 0,
    };
    
    let file = match StdFile::open("/proc/meminfo") {
        Ok(f) => f,
        Err(_) => return info,
    };
    
    let reader = StdBufReader::new(file);
    for line in reader.lines().flatten() {
        if line.starts_with("HugePages_Total:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.total = val.parse().unwrap_or(0);
            }
        } else if line.starts_with("HugePages_Free:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.free = val.parse().unwrap_or(0);
            }
        } else if line.starts_with("Hugepagesize:") {
            if let Some(val) = line.split_whitespace().nth(1) {
                info.page_size_kb = val.parse().unwrap_or(0);
            }
        }
    }
    
    info.available = info.total > 0;
    info
}

/// Linux以外の環境ではHuge Pagesは利用不可
#[cfg(not(target_os = "linux"))]
fn check_huge_pages_availability() -> HugePagesInfo {
    HugePagesInfo {
        available: false,
        total: 0,
        free: 0,
        page_size_kb: 0,
    }
}

/// mimalloc の Large OS Pages 設定を有効化し、状態をログ出力
/// 
/// Huge Pagesが利用可能な場合は有効化し、
/// 利用不可の場合は警告を出力して通常ページにフォールバックします。
fn configure_huge_pages(enabled: bool) {
    if !enabled {
        info!("Huge Pages: Disabled in configuration");
        return;
    }
    
    let hp_info = check_huge_pages_availability();
    
    if hp_info.available {
        // libmimalloc-sys を使用して Large OS Pages を有効化
        #[cfg(target_os = "linux")]
        {
            unsafe {
                // mi_option_large_os_pages = 6 (2MiB large pages)
                libmimalloc_sys::mi_option_set(
                    libmimalloc_sys::mi_option_large_os_pages,
                    1,
                );
            }
        }
        
        info!("Huge Pages: Enabled (Total: {}, Free: {}, Size: {}KB)", 
              hp_info.total, hp_info.free, hp_info.page_size_kb);
        info!("Huge Pages: TLB miss reduction active, expected 5-10% performance improvement");
        
        // 空きページが少ない場合は警告
        if hp_info.free < hp_info.total / 2 {
            warn!("Huge Pages: Free pages running low ({}/{}), consider increasing nr_hugepages", 
                  hp_info.free, hp_info.total);
        }
    } else {
        warn!("Huge Pages: Requested but not available on this system");
        #[cfg(target_os = "linux")]
        {
            warn!("Huge Pages: To enable, run: echo 128 | sudo tee /proc/sys/vm/nr_hugepages");
            warn!("Huge Pages: In containers, ensure hugepages are allocated on the host");
        }
        #[cfg(not(target_os = "linux"))]
        {
            warn!("Huge Pages: Only supported on Linux");
        }
        info!("Huge Pages: Falling back to standard 4KB pages");
    }
}

// ====================
// 定数定義（パフォーマンスチューニング済み）
// ====================

// エラーレスポンス用静的バッファ
static ERR_MSG_BAD_REQUEST: &[u8] = b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_FORBIDDEN: &[u8] = b"HTTP/1.1 403 Forbidden\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_NOT_FOUND: &[u8] = b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_METHOD_NOT_ALLOWED: &[u8] = b"HTTP/1.1 405 Method Not Allowed\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_TOO_MANY_REQUESTS: &[u8] = b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_BAD_GATEWAY: &[u8] = b"HTTP/1.1 502 Bad Gateway\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_REQUEST_TOO_LARGE: &[u8] = b"HTTP/1.1 413 Request Entity Too Large\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
static ERR_MSG_GATEWAY_TIMEOUT: &[u8] = b"HTTP/1.1 504 Gateway Timeout\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

// HTTP ヘッダー部品（事前計算）
static HTTP_200_PREFIX: &[u8] = b"HTTP/1.1 200 OK\r\nContent-Type: ";
static CONTENT_LENGTH_HEADER: &[u8] = b"\r\nContent-Length: ";
static CONNECTION_KEEP_ALIVE: &[u8] = b"\r\nConnection: keep-alive\r\n\r\n";
static CONNECTION_CLOSE: &[u8] = b"\r\nConnection: close\r\n\r\n";

// バッファサイズ（ページアライン・L2キャッシュ最適化）
const BUF_SIZE: usize = 65536;           // 64KB - io_uring最適サイズ
const HEADER_BUF_CAPACITY: usize = 512;  // HTTPヘッダー用

// セキュリティ制限
const MAX_HEADER_SIZE: usize = 8192;     // 8KB - ヘッダーサイズ上限
const MAX_BODY_SIZE: usize = 10485760;   // 10MB - ボディサイズ上限

// タイムアウト設定
const READ_TIMEOUT: Duration = Duration::from_secs(30);
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

// バックエンドコネクションプール設定（デフォルト値）
const BACKEND_POOL_MAX_IDLE_PER_HOST: usize = 8;    // ホストあたりの最大アイドル接続数
const BACKEND_POOL_IDLE_TIMEOUT_SECS: u64 = 30;     // アイドル接続のタイムアウト（秒）

// ====================
// セキュリティ設定構造体
// ====================
//
// ルートごとのセキュリティ設定を保持します。
// config.toml の各ルートに security セクションを追加することで
// 個別の制限を設定できます。
// ====================

/// セキュリティ設定のデフォルト値関数
fn default_max_body_size() -> usize { MAX_BODY_SIZE }
fn default_max_header_size() -> usize { MAX_HEADER_SIZE }
fn default_client_header_timeout() -> u64 { 30 }
fn default_client_body_timeout() -> u64 { 30 }
fn default_backend_connect_timeout() -> u64 { 10 }
fn default_max_idle_connections() -> usize { BACKEND_POOL_MAX_IDLE_PER_HOST }
fn default_idle_connection_timeout() -> u64 { BACKEND_POOL_IDLE_TIMEOUT_SECS }

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
        }
    }
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
}

// ====================
// 権限降格機能
// ====================
//
// root権限で起動した後、非特権ユーザーに降格することで
// セキュリティを向上させます。
//
// 注意: 特権ポート（1024未満）を使用する場合は、
// リスナー作成後に権限降格を行う必要があります。
// 現在のSO_REUSEPORT設計では、各スレッドがリスナーを作成するため、
// CAP_NET_BIND_SERVICEケイパビリティを付与するか、
// 非特権ポート（1024以上）を使用することを推奨します。
//
// ケイパビリティ付与例:
//   sudo setcap 'cap_net_bind_service=+ep' ./target/release/zerocopy-server
// ====================

/// ユーザー名からUIDを取得
#[cfg(target_os = "linux")]
fn get_uid_by_name(username: &str) -> Option<u32> {
    use std::ffi::CString;
    
    let username_cstr = CString::new(username).ok()?;
    
    unsafe {
        let pwd = libc::getpwnam(username_cstr.as_ptr());
        if pwd.is_null() {
            None
        } else {
            Some((*pwd).pw_uid)
        }
    }
}

/// グループ名からGIDを取得
#[cfg(target_os = "linux")]
fn get_gid_by_name(groupname: &str) -> Option<u32> {
    use std::ffi::CString;
    
    let groupname_cstr = CString::new(groupname).ok()?;
    
    unsafe {
        let grp = libc::getgrnam(groupname_cstr.as_ptr());
        if grp.is_null() {
            None
        } else {
            Some((*grp).gr_gid)
        }
    }
}

/// 権限降格を実行
/// 
/// グループ→ユーザーの順で降格する（逆順では失敗する可能性あり）
#[cfg(target_os = "linux")]
fn drop_privileges(security: &GlobalSecurityConfig) -> io::Result<()> {
    // rootでない場合は何もしない
    if unsafe { libc::getuid() } != 0 {
        info!("Not running as root, skipping privilege drop");
        return Ok(());
    }
    
    // グループ降格（先に行う）
    if let Some(ref group_name) = security.drop_privileges_group {
        let gid = get_gid_by_name(group_name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, 
                format!("Group '{}' not found", group_name)))?;
        
        if unsafe { libc::setgid(gid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        
        // 補助グループをクリア
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            warn!("Failed to clear supplementary groups: {}", io::Error::last_os_error());
        }
        
        info!("Dropped group privileges to '{}' (gid={})", group_name, gid);
    }
    
    // ユーザー降格
    if let Some(ref user_name) = security.drop_privileges_user {
        let uid = get_uid_by_name(user_name)
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, 
                format!("User '{}' not found", user_name)))?;
        
        if unsafe { libc::setuid(uid) } != 0 {
            return Err(io::Error::last_os_error());
        }
        
        info!("Dropped user privileges to '{}' (uid={})", user_name, uid);
    }
    
    // 降格成功の確認
    if security.drop_privileges_user.is_some() || security.drop_privileges_group.is_some() {
        let current_uid = unsafe { libc::getuid() };
        let current_gid = unsafe { libc::getgid() };
        info!("Current privileges: uid={}, gid={}", current_uid, current_gid);
        
        // rootに戻れないことを確認
        if security.drop_privileges_user.is_some() {
            if unsafe { libc::setuid(0) } == 0 {
                warn!("WARNING: Process can still regain root privileges!");
            }
        }
    }
    
    Ok(())
}

/// Linux以外のプラットフォーム用のスタブ
#[cfg(not(target_os = "linux"))]
fn drop_privileges(_security: &GlobalSecurityConfig) -> io::Result<()> {
    warn!("Privilege dropping is only supported on Linux");
    Ok(())
}

// ====================
// Graceful Shutdown フラグ
// ====================

static SHUTDOWN_FLAG: AtomicBool = AtomicBool::new(false);

// ====================
// TLSコネクタ（スレッドローカル）
// ====================

// rustls 用の TLS コネクター（kTLS 有効時は ktls_rustls を使用）
// kTLSフィーチャー有効時はシークレット抽出を有効化し、kTLS利用可能な状態にする
#[cfg(feature = "ktls")]
thread_local! {
    static TLS_CONNECTOR: RustlsConnector = {
        // kTLSフィーチャーが有効な場合はシークレット抽出を有効化
        // kTLSを有効にし、失敗時はrustlsにフォールバック
        let config = ktls_rustls::client_config(true);
        RustlsConnector::new(config)
            .with_ktls(true)        // kTLSを有効化
            .with_fallback(true)    // kTLS失敗時はrustlsにフォールバック
    };
}

// rustls 用の TLS コネクター（kTLS 無効時は simple_tls を使用）
#[cfg(not(feature = "ktls"))]
thread_local! {
    static TLS_CONNECTOR: simple_tls::SimpleTlsConnector = {
        let config = simple_tls::default_client_config();
        simple_tls::SimpleTlsConnector::new(config)
    };
}

// ====================
// バックエンドコネクションプール
// ====================
//
// スレッドローカルなコネクションプールにより、バックエンドへの接続を再利用します。
// HTTP用とHTTPS用で別々のプールを管理し、ホスト:ポートをキーにしています。
// ====================

/// プールされた接続のエントリ
struct PooledConnection<T> {
    stream: T,
    created_at: std::time::Instant,
}

impl<T> PooledConnection<T> {
    fn new(stream: T) -> Self {
        Self {
            stream,
            created_at: std::time::Instant::now(),
        }
    }
    
    /// 接続がまだ有効かどうかを判定（タイムアウトチェック）
    fn is_valid(&self) -> bool {
        self.created_at.elapsed().as_secs() < BACKEND_POOL_IDLE_TIMEOUT_SECS
    }
}

/// HTTPバックエンド用コネクションプール（TcpStream）
struct HttpConnectionPool {
    connections: HashMap<String, VecDeque<PooledConnection<TcpStream>>>,
}

impl HttpConnectionPool {
    fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }
    
    /// プールから接続を取得（有効な接続がなければNone）
    fn get(&mut self, key: &str) -> Option<TcpStream> {
        if let Some(queue) = self.connections.get_mut(key) {
            while let Some(entry) = queue.pop_front() {
                if entry.is_valid() {
                    return Some(entry.stream);
                }
                // 無効な接続は破棄
            }
        }
        None
    }
    
    /// 接続をプールに返却
    fn put(&mut self, key: String, stream: TcpStream) {
        let queue = self.connections.entry(key).or_insert_with(VecDeque::new);
        
        // 古い接続を削除
        while queue.len() >= BACKEND_POOL_MAX_IDLE_PER_HOST {
            queue.pop_front();
        }
        
        queue.push_back(PooledConnection::new(stream));
    }
}

/// HTTPSバックエンド用コネクションプール（ClientTls型エイリアス使用）
struct HttpsConnectionPool {
    connections: HashMap<String, VecDeque<PooledConnection<ClientTls>>>,
}

impl HttpsConnectionPool {
    fn new() -> Self {
        Self {
            connections: HashMap::new(),
        }
    }
    
    /// プールから接続を取得（有効な接続がなければNone）
    fn get(&mut self, key: &str) -> Option<ClientTls> {
        if let Some(queue) = self.connections.get_mut(key) {
            while let Some(entry) = queue.pop_front() {
                if entry.is_valid() {
                    return Some(entry.stream);
                }
                // 無効な接続は破棄
            }
        }
        None
    }
    
    /// 接続をプールに返却
    fn put(&mut self, key: String, stream: ClientTls) {
        let queue = self.connections.entry(key).or_insert_with(VecDeque::new);
        
        // 古い接続を削除
        while queue.len() >= BACKEND_POOL_MAX_IDLE_PER_HOST {
            queue.pop_front();
        }
        
        queue.push_back(PooledConnection::new(stream));
    }
}

thread_local! {
    static HTTP_POOL: RefCell<HttpConnectionPool> = RefCell::new(HttpConnectionPool::new());
    static HTTPS_POOL: RefCell<HttpsConnectionPool> = RefCell::new(HttpsConnectionPool::new());
}

// kTLS 有効時のスレッドローカル Splice パイプ
// splice(2) によるゼロコピー転送に使用
#[cfg(feature = "ktls")]
thread_local! {
    static SPLICE_PIPE: RefCell<Option<ktls_rustls::SplicePipe>> = RefCell::new(None);
}

/// スレッドローカルな Splice パイプを取得または初期化
#[cfg(feature = "ktls")]
fn get_splice_pipe() -> std::cell::Ref<'static, Option<ktls_rustls::SplicePipe>> {
    SPLICE_PIPE.with(|p| {
        {
            let mut pipe = p.borrow_mut();
            if pipe.is_none() {
                match ktls_rustls::SplicePipe::new() {
                    Ok(new_pipe) => {
                        *pipe = Some(new_pipe);
                        ftlog::info!("Splice pipe initialized for this thread");
                    }
                    Err(e) => {
                        ftlog::warn!("Failed to create splice pipe: {}", e);
                    }
                }
            }
        }
        // Safety: ライフタイムを'staticに拡張（thread_localなので安全）
        unsafe { std::mem::transmute(p.borrow()) }
    })
}

// ====================
// Raw I/O ヘルパー関数（kTLS + splice 用）
// ====================
//
// monoio の所有権ベースの I/O を回避するため、
// libc::read/write を直接使用します。
// 非同期待機は TcpStream::readable()/writable() を使用。
// ====================

/// libc::read のラッパー（ノンブロッキング対応）
#[cfg(feature = "ktls")]
#[inline]
fn raw_read(fd: std::os::unix::io::RawFd, buf: &mut [u8]) -> io::Result<usize> {
    let result = unsafe {
        libc::read(fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// libc::write のラッパー（ノンブロッキング対応）
#[cfg(feature = "ktls")]
#[inline]
fn raw_write(fd: std::os::unix::io::RawFd, buf: &[u8]) -> io::Result<usize> {
    let result = unsafe {
        libc::write(fd, buf.as_ptr() as *const libc::c_void, buf.len())
    };
    if result < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(result as usize)
    }
}

/// 非同期 raw read（TcpStream から FD 経由で読み取り）
/// 
/// monoio の所有権ベース I/O を回避し、libc::read を直接使用。
/// WouldBlock の場合は readable() で待機してリトライ。
#[cfg(feature = "ktls")]
async fn async_raw_read(stream: &TcpStream, buf: &mut [u8]) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    
    loop {
        match raw_read(fd, buf) {
            Ok(n) => return Ok(n),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 読み取り可能になるまで待機
                stream.readable(false).await?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// 非同期 raw write（TcpStream へ FD 経由で書き込み）
/// 
/// monoio の所有権ベース I/O を回避し、libc::write を直接使用。
/// WouldBlock の場合は writable() で待機してリトライ。
#[cfg(feature = "ktls")]
async fn async_raw_write(stream: &TcpStream, buf: &[u8]) -> io::Result<usize> {
    use std::os::unix::io::AsRawFd;
    let fd = stream.as_raw_fd();
    let mut written = 0;
    
    while written < buf.len() {
        match raw_write(fd, &buf[written..]) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "write returned 0")),
            Ok(n) => written += n,
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 書き込み可能になるまで待機
                stream.writable(false).await?;
            }
            Err(e) => return Err(e),
        }
    }
    
    Ok(written)
}

/// 非同期 raw write all（全バイト書き込み完了まで）
#[cfg(feature = "ktls")]
async fn async_raw_write_all(stream: &TcpStream, buf: &[u8]) -> io::Result<()> {
    let written = async_raw_write(stream, buf).await?;
    if written < buf.len() {
        Err(io::Error::new(io::ErrorKind::WriteZero, "failed to write all bytes"))
    } else {
        Ok(())
    }
}

// ====================
// バッファプール（パフォーマンス最適化版）
// ====================
//
// 注意: monoioのAsyncWriteRentExtはバッファの所有権を取るため、
// 完全なゼロコピーは実現できません。バッファプールによりアロケーション
// コストを削減していますが、Arc<Vec<u8>>からのコピーは避けられません。
//
// ## パフォーマンス最適化: ゼロ埋め削除
//
// バッファの再利用時にゼロ埋め（memset）を行わず、`set_len()` のみを使用。
// これにより64KB × N回のmemsetコストを完全に削除しています。
//
// ## セキュリティ保証
//
// Heartbleed類似の脆弱性（未初期化メモリの漏洩）を防ぐため、
// 以下の不変条件を厳守する必要があります:
//
// 1. 読み込み操作後は、必ず読み込まれたサイズ `n` のみを参照する
//    - OK: `&buf[..n]`, `accumulated.extend_from_slice(&buf[..n])`
//    - NG: `&buf[..buf.len()]`, `buf.as_slice()`
//
// 2. バッファの全長を信頼しない
//    - `buf.len()` は常に BUF_SIZE だが、有効なデータは読み込みサイズのみ
//
// ====================

thread_local! {
    /// スレッドローカルバッファプール
    /// 
    /// 各バッファは64KBの容量を持ち、読み込み操作に使用されます。
    /// バッファの内容は未初期化状態ですが、io_uringの読み込み操作で
    /// カーネルが必要な領域を上書きするため、安全に使用できます。
    static BUF_POOL: RefCell<Vec<Vec<u8>>> = RefCell::new(
        (0..32).map(|_| {
            let mut buf = Vec::with_capacity(BUF_SIZE);
            // SAFETY: 読み込み専用バッファとして使用
            // 読み込み後は &buf[..n] のように読み込まれたサイズのみを参照
            unsafe {
                buf.set_len(BUF_SIZE);
            }
            buf
        }).collect()
    );
}

/// バッファ取得ヘルパー
/// 
/// # Safety
/// プールからの取得時、バッファには以前のデータが残っている可能性があります。
/// ただし、これは安全です：
/// - io_uringの読み込み操作でカーネルが必要な領域を上書きする
/// - 読み込み後は `&buf[..n]` のように読み込まれたサイズのみを参照する
/// 
/// 新規作成時のみゼロ初期化を行います（プール枯渇時のみ発生）。
#[inline(always)]
fn buf_get() -> Vec<u8> {
    BUF_POOL.with(|p| {
        p.borrow_mut().pop().unwrap_or_else(|| {
            // プールが空の場合のみ新規作成
            // 新規作成時はゼロ初期化（allocatorがゼロページを返す場合もある）
            let mut buf = Vec::with_capacity(BUF_SIZE);
            // SAFETY: capacity は BUF_SIZE 以上確保済み
            // 新規バッファなのでゼロ初期化は不要（io_uringが上書きする）
            unsafe {
                buf.set_len(BUF_SIZE);
            }
            buf
        })
    })
}

/// バッファ返却ヘルパー（パフォーマンス最適化版）
/// 
/// # Safety
/// `unsafe { buf.set_len(BUF_SIZE) }` を使用してゼロ埋めコストを削減しています。
/// 
/// ## セキュリティ分析（Heartbleed類似リスクの回避）
/// 
/// このコードが安全である理由:
/// 
/// 1. **読み込みサイズの厳密な管理**
///    - `read` 操作後は必ず `&returned_buf[..n]` のように読み込まれたサイズのみを参照
///    - 未初期化領域（以前のデータが残る可能性のある領域）にはアクセスしない
/// 
/// 2. **io_uringの動作特性**
///    - monoioはio_uringを使用しており、カーネルが直接バッファに書き込む
///    - 読み込み操作前にバッファがゼロ初期化されている必要はない
/// 
/// 3. **バッファの使用パターン**
///    - バッファは読み込み専用として使用され、読み込まれたサイズ分のみが
///      `extend_from_slice(&buf[..n])` などで安全にコピーされる
/// 
/// ## パフォーマンス改善
/// - ゼロ埋めコスト（64KB × memset）を完全に削除
/// - 高頻度のバッファ再利用時に顕著な効果
/// 
/// ## 注意事項
/// - バッファを使用する際は、必ず読み込まれたサイズ `n` のみを参照すること
/// - `buf.len()` を信頼して全領域を参照してはならない
#[inline(always)]
fn buf_put(mut buf: Vec<u8>) {
    BUF_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        if pool.len() < 128 {
            // バッファの容量が十分であることを確認
            if buf.capacity() >= BUF_SIZE {
                // SAFETY: 
                // - capacity() >= BUF_SIZE を事前に確認済み
                // - このバッファは読み込み用として使用され、読み込まれたサイズのみが参照される
                // - 未初期化領域へのアクセスは呼び出し側で防止されている
                unsafe {
                    buf.set_len(BUF_SIZE);
                }
            } else {
                // 容量が足りない場合は新規作成（通常は発生しない）
                buf = vec![0u8; BUF_SIZE];
            }
            pool.push(buf);
        }
    });
}

// ====================
// 設定構造体
// ====================

#[derive(Deserialize)]
struct Config {
    server: ServerConfigSection,
    tls: TlsConfigSection,
    #[serde(default)]
    performance: PerformanceConfigSection,
    /// グローバルセキュリティ設定（権限降格など）
    #[serde(default)]
    security: GlobalSecurityConfig,
    host_routes: Option<HashMap<String, BackendConfig>>,
    path_routes: Option<HashMap<String, HashMap<String, BackendConfig>>>,
}

#[derive(Deserialize)]
struct ServerConfigSection {
    listen: String,
    /// ワーカースレッド数
    /// 未指定または0の場合はCPUコア数と同じスレッド数を使用
    #[serde(default)]
    threads: Option<usize>,
}

#[derive(Deserialize)]
struct TlsConfigSection {
    cert_path: String,
    key_path: String,
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
    ktls_enabled: bool,
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
    ktls_fallback_enabled: bool,
}

/// kTLSフォールバックのデフォルト値（true = フォールバック有効）
fn default_ktls_fallback() -> bool {
    true
}

/// パフォーマンス設定
#[derive(Deserialize, Clone, Default)]
struct PerformanceConfigSection {
    /// SO_REUSEPORTの振り分け方式
    /// - "kernel": カーネルデフォルト（3元タプルハッシュ）
    /// - "cbpf": クライアントIPベースのCBPF振り分け（Linux 4.6+必須）
    #[serde(default)]
    reuseport_balancing: ReuseportBalancing,
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
    huge_pages_enabled: bool,
}

/// SO_REUSEPORTの振り分け方式
#[derive(Clone, Copy, Debug, PartialEq, Default)]
enum ReuseportBalancing {
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

#[derive(Clone)]
enum BackendConfig {
    Proxy { url: String, security: SecurityConfig },
    /// File バックエンド設定
    /// - path: ファイルまたはディレクトリのパス
    /// - mode: "sendfile" または "memory"
    /// - index: ディレクトリアクセス時に返すファイル名（デフォルト: "index.html"）
    /// - security: ルートごとのセキュリティ設定
    File { path: String, mode: String, index: Option<String>, security: SecurityConfig },
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
                let mut path: Option<String> = None;
                let mut mode: Option<String> = None;
                let mut index: Option<String> = None;
                let mut security: Option<SecurityConfig> = None;
                
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "type" => backend_type = Some(map.next_value()?),
                        "url" => url = Some(map.next_value()?),
                        "path" => path = Some(map.next_value()?),
                        "mode" => mode = Some(map.next_value()?),
                        "index" => index = Some(map.next_value()?),
                        "security" => security = Some(map.next_value()?),
                        _ => { let _: serde::de::IgnoredAny = map.next_value()?; }
                    }
                }
                
                let backend_type = backend_type.unwrap_or_else(|| "File".to_string());
                let security = security.unwrap_or_default();
                
                match backend_type.as_str() {
                    "Proxy" => {
                        let url = url.ok_or_else(|| serde::de::Error::missing_field("url"))?;
                        Ok(BackendConfig::Proxy { url, security })
                    }
                    "File" | _ => {
                        let path = path.ok_or_else(|| serde::de::Error::missing_field("path"))?;
                        let mode = mode.unwrap_or_else(|| "sendfile".to_string());
                        Ok(BackendConfig::File { path, mode, index, security })
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
enum Backend {
    /// Proxy バックエンド
    /// - Arc<ProxyTarget>: プロキシターゲット
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    Proxy(Arc<ProxyTarget>, Arc<SecurityConfig>),
    /// MemoryFile バックエンド
    /// - Arc<Vec<u8>>: ファイルコンテンツ
    /// - Arc<str>: MIMEタイプ
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    MemoryFile(Arc<Vec<u8>>, Arc<str>, Arc<SecurityConfig>),
    /// SendFile バックエンド
    /// - Arc<PathBuf>: ベースパス
    /// - bool: ディレクトリかどうか
    /// - Option<Arc<str>>: インデックスファイル名（None = "index.html"）
    /// - Arc<SecurityConfig>: ルートごとのセキュリティ設定
    SendFile(Arc<PathBuf>, bool, Option<Arc<str>>, Arc<SecurityConfig>),
}

impl Backend {
    /// このバックエンドのセキュリティ設定を取得
    #[inline]
    fn security(&self) -> &SecurityConfig {
        match self {
            Backend::Proxy(_, security) => security,
            Backend::MemoryFile(_, _, security) => security,
            Backend::SendFile(_, _, _, security) => security,
        }
    }
}

#[derive(Clone)]
struct ProxyTarget {
    host: String,
    port: u16,
    use_tls: bool,
    path_prefix: String,
}

impl ProxyTarget {
    fn parse(url: &str) -> Option<Self> {
        let (scheme, rest) = if url.starts_with("https://") {
            (true, &url[8..])
        } else if url.starts_with("http://") {
            (false, &url[7..])
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
        })
    }
    
    /// デフォルトポートかどうかを判定
    #[inline]
    fn is_default_port(&self) -> bool {
        if self.use_tls {
            self.port == 443
        } else {
            self.port == 80
        }
    }
}

// ====================
// nginx風パスルーター（最長プレフィックス一致）
// ====================
//
// nginxの location ディレクティブと同様のマッチング動作を実現。
//
// ## nginxのマッチング規則（優先順位順）
//
// | 優先度 | nginx記法 | 説明                     | 本実装での対応       |
// |--------|-----------|--------------------------|---------------------|
// | 1      | = /path   | 完全一致                 | 将来対応予定         |
// | 2      | ^~ /path  | 優先プレフィックス       | 将来対応予定         |
// | 3      | ~ regex   | 正規表現                 | 対応なし             |
// | 4      | /path     | **最長プレフィックス一致** | ✓ 実装済み         |
//
// ## 動作例
//
// 設定:
//   "/static" = { path = "./www/static/" }  ← 末尾スラッシュなしでもOK
//   "/api"    = { url = "http://backend:8080" }
//   "/"       = { path = "./www/index.html" }
//
// リクエスト → マッチするルート:
//   /                     → /       (完全一致)
//   /static               → /static (完全一致) → index.html を返す
//   /static/              → /static (ディレクトリアクセス) → index.html を返す
//   /static/css/style.css → /static (プレフィックスマッチ)
//   /api/users            → /api    (プレフィックスマッチ)
//   /favicon.ico          → 404     (マッチなし)
//   /unknown              → 404     (マッチなし)
//
// ## パフォーマンス
//
// matchit (Radix Tree) を使用: O(log n)
// | ルート数 | 線形探索 O(n) | Radix Tree O(log n) |
// |----------|---------------|---------------------|
// | 10       | ~100ns        | ~50ns               |
// | 100      | ~1μs          | ~100ns              |
// | 1000     | ~10μs         | ~150ns              |

/// nginx風パスルーター
/// 
/// 最長プレフィックス一致による高速なルーティングを提供。
/// "/" が登録されている場合はcatch-all（フォールバック）として機能。
#[derive(Clone)]
struct PathRouter {
    /// matchit Router（Radix Tree実装）
    router: Arc<matchit::Router<usize>>,
    /// Backendの実体を保持
    backends: Arc<Vec<(Arc<[u8]>, Backend)>>,
}

impl PathRouter {
    /// 新しいPathRouterを構築
    /// 
    /// nginx風の最長プレフィックス一致を実現するため、
    /// 各プレフィックスに対して完全一致とワイルドカードの両方を登録。
    fn new(entries: Vec<(String, Backend)>) -> io::Result<Self> {
        let mut router = matchit::Router::new();
        let mut backends = Vec::with_capacity(entries.len());
        
        // バックエンドを登録（インデックスを確定）
        for (prefix, backend) in entries.iter() {
            backends.push((Arc::from(prefix.as_bytes()), backend.clone()));
        }
        
        // ルートを登録
        // matchitの優先順位: 静的パス > パラメータ > ワイルドカード
        // より具体的なプレフィックスが自動的に優先される
        for (i, (prefix, _)) in entries.iter().enumerate() {
            if prefix == "/" {
                // "/" は完全一致のみ（"/" へのリクエストのみマッチ）
                // catch-all動作なし: マッチしないパスは404エラー
                if let Err(e) = router.insert("/".to_string(), i) {
                    warn!("Route registration failed for '/': {}", e);
                }
            } else if prefix.ends_with('/') {
                // "/api/" スタイル（ディレクトリ）
                let base = prefix.trim_end_matches('/');
                
                // "/api" への完全一致
                let _ = router.insert(base.to_string(), i);
                // "/api/" への完全一致
                let _ = router.insert(prefix.clone(), i);
                // "/api/{*rest}" でサブパスにマッチ
                let _ = router.insert(format!("{base}/{{*rest}}"), i);
            } else {
                // "/api" スタイル（通常のプレフィックス）
                // 末尾スラッシュなしでもディレクトリ配信やプロキシが動作するように
                // 3つのパターンを登録：
                // 1. "/api" への完全一致
                let _ = router.insert(prefix.clone(), i);
                // 2. "/api/" への完全一致（ディレクトリアクセス）
                let _ = router.insert(format!("{prefix}/"), i);
                // 3. "/api/{*rest}" でサブパスにマッチ
                let _ = router.insert(format!("{prefix}/{{*rest}}"), i);
            }
        }
        
        Ok(Self {
            router: Arc::new(router),
            backends: Arc::new(backends),
        })
    }
    
    /// パスに最長一致するバックエンドを検索（nginx風）
    /// 
    /// matchitが内部でRadix Treeを使用し、
    /// 最も具体的な（＝最長の）プレフィックスを自動的に選択。
    fn find_longest(&self, path: &[u8]) -> Option<(&[u8], &Backend)> {
        let path_str = std::str::from_utf8(path).ok()?;
        
        match self.router.at(path_str) {
            Ok(matched) => {
                let idx = *matched.value;
                let (prefix, backend) = &self.backends[idx];
                Some((prefix.as_ref(), backend))
            }
            Err(_) => None,
        }
    }
}

// 旧SortedPathMapとの互換性のため、型エイリアスを提供
// TODO: 将来的にはPathRouterに完全移行
type SortedPathMap = PathRouter;

// ====================
// 非同期I/Oトレイト（コード重複解消）
// ====================

/// 非同期読み込みトレイト
trait AsyncReader {
    async fn read_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>);
}

/// 非同期書き込みトレイト
trait AsyncWriter {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>);
}

// TcpStream用の実装
impl AsyncReader for TcpStream {
    async fn read_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.read(buf).await
    }
}

impl AsyncWriter for TcpStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// KtlsServerStream用の実装（rustls + ktls2）
#[cfg(feature = "ktls")]
impl AsyncReader for KtlsServerStream {
    async fn read_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.read(buf).await
    }
}

#[cfg(feature = "ktls")]
impl AsyncWriter for KtlsServerStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// KtlsClientStream用の実装（rustls + ktls2）
#[cfg(feature = "ktls")]
impl AsyncReader for KtlsClientStream {
    async fn read_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.read(buf).await
    }
}

#[cfg(feature = "ktls")]
impl AsyncWriter for KtlsClientStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// SimpleTlsServerStream用の実装（rustls のみ）
#[cfg(not(feature = "ktls"))]
impl AsyncReader for simple_tls::SimpleTlsServerStream {
    async fn read_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.read(buf).await
    }
}

#[cfg(not(feature = "ktls"))]
impl AsyncWriter for simple_tls::SimpleTlsServerStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// SimpleTlsClientStream用の実装（rustls のみ）
#[cfg(not(feature = "ktls"))]
impl AsyncReader for simple_tls::SimpleTlsClientStream {
    async fn read_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.read(buf).await
    }
}

#[cfg(not(feature = "ktls"))]
impl AsyncWriter for simple_tls::SimpleTlsClientStream {
    async fn write_buf(&mut self, buf: Vec<u8>) -> (io::Result<usize>, Vec<u8>) {
        self.write_all(buf).await
    }
}

// ====================
// 設定読み込み
// ====================

// rustls 用の TLS 設定読み込み（統一）
fn load_tls_config(tls_config: &TlsConfigSection, ktls_enabled: bool) -> io::Result<Arc<ServerConfig>> {
    let cert_file = File::open(&tls_config.cert_path)?;
    let key_file = File::open(&tls_config.key_path)?;

    let mut cert_reader = BufReader::new(cert_file);
    let cert_chain = certs(&mut cert_reader).collect::<Result<Vec<_>, _>>()?;

    let mut key_reader = BufReader::new(key_file);
    let keys = private_key(&mut key_reader)?
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "Private key not found"))?;

    // kTLS 有効時のみ config を変更するため、条件付きで mut を使用
    #[allow(unused_mut)]
    let mut config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, keys)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

    // kTLS が有効な場合のみシークレット抽出を有効化
    // これにより dangerous_extract_secrets() が使用可能になる
    #[cfg(feature = "ktls")]
    if ktls_enabled {
        config.enable_secret_extraction = true;
        info!("TLS secret extraction enabled for kTLS support");
    }

    // kTLS 無効時は警告を抑制
    #[cfg(not(feature = "ktls"))]
    let _ = ktls_enabled;

    Ok(Arc::new(config))
}

/// 設定読み込みの戻り値型（統一）
struct LoadedConfig {
    listen_addr: String,
    tls_config: Arc<ServerConfig>,
    host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: Arc<HashMap<Box<[u8]>, PathRouter>>,
    ktls_config: KtlsConfig,
    reuseport_balancing: ReuseportBalancing,
    num_threads: usize,
    /// Huge Pages (Large OS Pages) を有効化するかどうか
    huge_pages_enabled: bool,
    /// グローバルセキュリティ設定
    global_security: GlobalSecurityConfig,
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
#[allow(dead_code)]
struct RuntimeConfig {
    /// ホストベースのルーティング（O(1) HashMap）
    host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
    /// パスベースのルーティング（O(log n) Radix Tree）
    path_routes: Arc<HashMap<Box<[u8]>, PathRouter>>,
    /// TLS設定
    tls_config: Option<Arc<ServerConfig>>,
    /// kTLS設定
    ktls_config: Arc<KtlsConfig>,
    /// グローバルセキュリティ設定
    global_security: Arc<GlobalSecurityConfig>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            host_routes: Arc::new(HashMap::new()),
            path_routes: Arc::new(HashMap::new()),
            tls_config: None,
            ktls_config: Arc::new(KtlsConfig::default()),
            global_security: Arc::new(GlobalSecurityConfig::default()),
        }
    }
}

/// グローバルな設定保持用（ホットリロード対応）
/// 読み込みはロックフリーで非常に高速
static CURRENT_CONFIG: Lazy<ArcSwap<RuntimeConfig>> =
    Lazy::new(|| ArcSwap::from_pointee(RuntimeConfig::default()));

/// 設定をホットリロードする
/// 
/// 実行中のリクエストは古い設定を参照し続け、
/// 新規リクエストは新しい設定を使用します。
#[allow(dead_code)]
fn reload_config(path: &Path) -> io::Result<()> {
    let loaded = load_config(path)?;
    
    let runtime_config = RuntimeConfig {
        host_routes: loaded.host_routes,
        path_routes: loaded.path_routes,
        tls_config: Some(loaded.tls_config),
        ktls_config: Arc::new(loaded.ktls_config),
        global_security: Arc::new(loaded.global_security),
    };
    
    // アトミックに設定を入れ替え
    CURRENT_CONFIG.store(Arc::new(runtime_config));
    
    info!("Configuration reloaded successfully");
    Ok(())
}

fn load_config(path: &Path) -> io::Result<LoadedConfig> {
    let config_str = fs::read_to_string(path)?;
    let config: Config = toml::from_str(&config_str)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, format!("TOML parse error: {}", e)))?;

    // kTLS設定（TLS設定より先に読み込む）
    let ktls_config = KtlsConfig {
        enabled: config.tls.ktls_enabled,
        enable_tx: config.tls.ktls_enabled,
        enable_rx: config.tls.ktls_enabled,
        fallback_enabled: config.tls.ktls_fallback_enabled,
    };

    // TLS設定（kTLS有効時はシークレット抽出を有効化）
    let tls_config = load_tls_config(&config.tls, ktls_config.enabled)?;

    let mut host_routes_bytes: HashMap<Box<[u8]>, Backend> = HashMap::new();
    if let Some(host_routes) = config.host_routes {
        for (k, v) in host_routes {
            let backend = load_backend(&v)?;
            host_routes_bytes.insert(k.into_bytes().into_boxed_slice(), backend);
        }
    }

    let mut path_routes_bytes: HashMap<Box<[u8]>, PathRouter> = HashMap::new();
    if let Some(path_routes) = config.path_routes {
        for (host, path_map) in path_routes {
            let mut entries: Vec<(String, Backend)> = Vec::with_capacity(path_map.len());
            for (k, v) in path_map {
                entries.push((k, load_backend(&v)?));
            }
            // 長さ降順でソート（最長一致の優先順位を維持）
            // PathRouter内部でRadix Treeに登録する際に使用
            entries.sort_by(|a, b| b.0.len().cmp(&a.0.len()));
            
            // PathRouter（Radix Treeベース）を構築
            let router = PathRouter::new(entries)?;
            
            path_routes_bytes.insert(
                host.into_bytes().into_boxed_slice(),
                router
            );
        }
    }

    // スレッド数の決定: 未指定または0の場合はCPUコア数を使用
    let num_threads = match config.server.threads {
        Some(n) if n > 0 => n,
        _ => num_cpus::get(),
    };

    Ok(LoadedConfig {
        listen_addr: config.server.listen,
        tls_config,
        host_routes: Arc::new(host_routes_bytes),
        path_routes: Arc::new(path_routes_bytes),
        ktls_config,
        reuseport_balancing: config.performance.reuseport_balancing,
        num_threads,
        huge_pages_enabled: config.performance.huge_pages_enabled,
        global_security: config.security,
    })
}

fn load_backend(config: &BackendConfig) -> io::Result<Backend> {
    match config {
        BackendConfig::Proxy { url, security } => {
            let target = ProxyTarget::parse(url)
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "Invalid proxy URL"))?;
            Ok(Backend::Proxy(Arc::new(target), Arc::new(security.clone())))
        }
        BackendConfig::File { path, mode, index, security } => {
            let metadata = fs::metadata(path)?;
            let is_dir = metadata.is_dir();
            // インデックスファイル名を Arc<str> に変換（None = デフォルトで "index.html"）
            let index_file: Option<Arc<str>> = index.as_ref().map(|s| Arc::from(s.as_str()));
            let security = Arc::new(security.clone());
            
            match mode.as_str() {
                "memory" => {
                    if is_dir {
                        return Err(io::Error::new(io::ErrorKind::InvalidInput, "Memory mode not supported for directories"));
                    }
                    let data = fs::read(path)?;
                    let mime_type = mime_guess::from_path(path).first_or_octet_stream();
                    
                    Ok(Backend::MemoryFile(Arc::new(data), Arc::from(mime_type.as_ref()), security))
                }
                "sendfile" | "" => Ok(Backend::SendFile(Arc::new(PathBuf::from(path)), is_dir, index_file, security)),
                _ => Err(io::Error::new(io::ErrorKind::InvalidInput, "Invalid mode")),
            }
        }
    }
}

// ====================
// メイン関数
// ====================

fn main() {
    // プロセスレベルで暗号プロバイダーをインストール（ring使用）
    CryptoProvider::install_default(rustls::crypto::ring::default_provider())
        .expect("Failed to install rustls crypto provider");
    
    let _guard = ftlog::Builder::new().try_init().unwrap();

    let loaded_config = match load_config(Path::new("config.toml")) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Config load error: {}", e);
            return;
        }
    };
    
    // Huge Pages (Large OS Pages) 設定
    // mimallocでHuge Pagesを有効化し、TLBミスを削減
    // 注: グローバルアロケータは静的初期化されるため、
    //     この設定は以降の新規割り当てに影響する
    configure_huge_pages(loaded_config.huge_pages_enabled);
    
    // TLS アクセプターを作成
    #[cfg(feature = "ktls")]
    let acceptor = RustlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled)
        .with_fallback(loaded_config.ktls_config.fallback_enabled);
    
    #[cfg(not(feature = "ktls"))]
    let acceptor = simple_tls::SimpleTlsAcceptor::new(loaded_config.tls_config.clone())
        .with_ktls(loaded_config.ktls_config.enabled);
    
    let listen_addr = loaded_config.listen_addr.parse::<SocketAddr>()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], 443)));
    let ktls_config = Arc::new(loaded_config.ktls_config);

    let hostname = hostname::get()
        .map(|h| h.to_string_lossy().into_owned())
        .unwrap_or_else(|_| "unknown".to_string());
    
    let num_threads = loaded_config.num_threads;
    
    info!("============================================");
    info!("High-Performance Reverse Proxy Server");
    info!("Hostname: {}", hostname);
    info!("Listen Address: {}", listen_addr);
    info!("Threads: {} (CPU cores: {})", num_threads, num_cpus::get());
    info!("CPU Affinity: Enabled (pinning workers to cores)");
    info!("Reuseport Balancing: {:?}", loaded_config.reuseport_balancing);
    info!("Read Timeout: {:?}", READ_TIMEOUT);
    info!("Write Timeout: {:?}", WRITE_TIMEOUT);
    info!("Connect Timeout: {:?}", CONNECT_TIMEOUT);
    info!("Idle Timeout: {:?}", IDLE_TIMEOUT);
    
    // kTLS設定のログ出力
    log_ktls_status(&ktls_config);
    
    info!("============================================");

    // Graceful Shutdown用のシグナルハンドラを設定
    setup_signal_handler();

    let mut handles = Vec::with_capacity(num_threads);

    // CPUアフィニティ設定のためのコアID取得
    let core_ids = core_affinity::get_core_ids();
    let core_ids_available = core_ids.as_ref().map(|ids| ids.len()).unwrap_or(0);
    
    if core_ids.is_some() && core_ids_available > 0 {
        info!("CPU Affinity: {} cores available, pinning {} worker threads", 
              core_ids_available, num_threads);
    } else {
        warn!("CPU Affinity: Could not detect core IDs, workers will not be pinned");
    }

    // SO_REUSEPORT振り分け設定
    let reuseport_balancing = loaded_config.reuseport_balancing;

    // 権限降格（設定されている場合）
    // 注意: 特権ポート（1024未満）を使用する場合は、
    // CAP_NET_BIND_SERVICEケイパビリティを付与するか、
    // 権限降格を無効にする必要があります。
    if let Err(e) = drop_privileges(&loaded_config.global_security) {
        error!("Failed to drop privileges: {}", e);
        return;
    }

    for thread_id in 0..num_threads {
        let acceptor_clone = acceptor.clone();
        let host_routes_clone = loaded_config.host_routes.clone();
        let path_routes_clone = loaded_config.path_routes.clone();
        let ktls_config_clone = ktls_config.clone();
        let addr = listen_addr;
        let balancing = reuseport_balancing;
        let workers = num_threads;
        
        // このスレッドに割り当てるコアIDを決定
        // コア数よりスレッド数が多い場合はモジュロ演算でラップアラウンド
        let assigned_core = core_ids.as_ref().map(|ids| {
            let core_index = thread_id % ids.len();
            ids[core_index]
        });

        let handle = thread::spawn(move || {
            // スレッド開始直後にCPUアフィニティを設定
            // これによりL1/L2キャッシュミスを削減し、レイテンシのジッターを安定化
            if let Some(core_id) = assigned_core {
                if core_affinity::set_for_current(core_id) {
                    info!("[Thread {}] Pinned to CPU core {:?}", thread_id, core_id);
                } else {
                    warn!("[Thread {}] Failed to pin to CPU core {:?}, running unpinned", 
                          thread_id, core_id);
                }
            }
            
            let mut rt = RuntimeBuilder::<monoio::IoUringDriver>::new()
                .enable_timer()
                .build()
                .expect("Failed to create runtime");
            rt.block_on(async move {
                let listener = match create_listener(addr, balancing, workers, thread_id) {
                    Ok(l) => l,
                    Err(e) => {
                        error!("[Thread {}] Bind error: {}", thread_id, e);
                        return;
                    }
                };

                info!("[Thread {}] Worker started", thread_id);

                loop {
                    // Shutdown チェック
                    if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        info!("[Thread {}] Shutting down...", thread_id);
                        break;
                    }

                    // タイムアウト付きaccept（Graceful Shutdown対応）
                    let accept_result = timeout(Duration::from_secs(1), listener.accept()).await;
                    
                    let (stream, peer_addr) = match accept_result {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            error!("[Thread {}] Accept error: {}", thread_id, e);
                            continue;
                        }
                        Err(_) => {
                            // タイムアウト - ループを継続してshutdownチェック
                            continue;
                        }
                    };
                    
                    let _ = stream.set_nodelay(true);
                    
                    let acceptor = acceptor_clone.clone();
                    let host_routes = host_routes_clone.clone();
                    let path_routes = path_routes_clone.clone();
                    let ktls_cfg = ktls_config_clone.clone();
                    
                    monoio::spawn(async move {
                        handle_connection(stream, acceptor, host_routes, path_routes, peer_addr, ktls_cfg).await;
                    });
                }
                
                info!("[Thread {}] Worker stopped", thread_id);
            });
        });
        handles.push(handle);
    }

    for handle in handles {
        let _ = handle.join();
    }
    
    info!("Server shutdown complete");
}

/// kTLSの状態をログ出力
fn log_ktls_status(ktls_config: &KtlsConfig) {
    if ktls_config.enabled {
        // rustls + ktls2 使用時
        #[cfg(feature = "ktls")]
        {
            if ktls_rustls::is_ktls_available() {
                info!("kTLS: Enabled via rustls + ktls2 (TX={}, RX={})", ktls_config.enable_tx, ktls_config.enable_rx);
                info!("kTLS: Kernel TLS offload active - reduced CPU usage expected");
                if ktls_config.fallback_enabled {
                    info!("kTLS: Fallback to rustls enabled (graceful degradation)");
                } else {
                    info!("kTLS: Fallback disabled (kTLS required, connections will fail if unavailable)");
                }
            } else {
                warn!("kTLS: Requested but kernel support not available");
                warn!("kTLS: Ensure 'modprobe tls' has been run and kernel 5.15+ is used");
                if ktls_config.fallback_enabled {
                    warn!("kTLS: Falling back to userspace TLS via rustls");
                } else {
                    error!("kTLS: Fallback disabled but kTLS unavailable - connections will fail!");
                    error!("kTLS: Either enable fallback or run 'modprobe tls'");
                }
            }
        }
        // kTLS フィーチャー無効時
        #[cfg(not(feature = "ktls"))]
        {
            warn!("kTLS: Enabled in config but ktls feature is not enabled");
            warn!("kTLS: Rebuild with: cargo build --features ktls for kTLS support");
        }
    } else {
        info!("kTLS: Disabled (using userspace TLS via rustls)");
    }
}

/// シグナルハンドラのセットアップ
fn setup_signal_handler() {
    // SIGINT, SIGTERM をキャッチしてシャットダウンフラグを設定
    ctrlc::set_handler(move || {
        info!("Received shutdown signal, initiating graceful shutdown...");
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
    }).expect("Failed to set signal handler");
}

// ====================
// SO_REUSEPORT CBPF ロードバランシング
// ====================

/// CBPFプログラムがアタッチ済みかどうかを追跡するグローバルカウンター
/// 最初のワーカーのみがCBPFプログラムをアタッチする
#[cfg(target_os = "linux")]
static CBPF_ATTACHED: AtomicUsize = AtomicUsize::new(0);

/// クライアントIPハッシュに基づく振り分けCBPFプログラムを生成
/// 
/// このBPFプログラムは、accept()時に呼び出され、
/// クライアントのソースIPアドレスをハッシュしてワーカーインデックスを返す
/// 
/// # 引数
/// * `num_workers` - ワーカースレッド数
/// 
/// # 戻り値
/// BPF命令列（sock_filter配列）
#[cfg(target_os = "linux")]
fn create_reuseport_cbpf_program(num_workers: u32) -> Vec<libc::sock_filter> {
    // BPF命令セット:
    // 1. ソースIPアドレスを取得（sk_reuseport_mdからオフセット0でソースIPを読み取り）
    // 2. ワーカー数でmod演算
    // 3. 結果をソケットインデックスとして返す
    //
    // BPF_LD + BPF_W + BPF_ABS: 32ビットワードをパケットから絶対オフセットで読み込み
    // BPF_ALU + BPF_MOD + BPF_K: 即値でmod演算
    // BPF_RET + BPF_A: Aレジスタの値を返す
    vec![
        // LD A, [0]: ソースIPアドレスを読み込み（sk_reuseport_md構造体のオフセット0）
        libc::sock_filter {
            code: (libc::BPF_LD | libc::BPF_W | libc::BPF_ABS) as u16,
            jt: 0,
            jf: 0,
            k: 0, // remote_ip4 のオフセット
        },
        // ALU MOD #num_workers: A = A % num_workers
        libc::sock_filter {
            code: (libc::BPF_ALU | libc::BPF_MOD | libc::BPF_K) as u16,
            jt: 0,
            jf: 0,
            k: num_workers,
        },
        // RET A: Aレジスタの値（ソケットインデックス）を返す
        libc::sock_filter {
            code: (libc::BPF_RET | libc::BPF_A) as u16,
            jt: 0,
            jf: 0,
            k: 0,
        },
    ]
}

/// CBPFプログラムをソケットにアタッチする
/// 
/// SO_ATTACH_REUSEPORT_CBPF を使用して、クライアントIPベースの
/// 振り分けロジックをカーネルに設定する
/// 
/// # 引数
/// * `fd` - ソケットファイルディスクリプタ
/// * `num_workers` - ワーカースレッド数
/// 
/// # 戻り値
/// 成功時はOk(()), 失敗時はエラー
#[cfg(target_os = "linux")]
fn attach_reuseport_cbpf(fd: i32, num_workers: usize) -> io::Result<()> {
    let program = create_reuseport_cbpf_program(num_workers as u32);
    
    #[repr(C)]
    struct SockFprog {
        len: u16,
        filter: *const libc::sock_filter,
    }
    
    let prog = SockFprog {
        len: program.len() as u16,
        filter: program.as_ptr(),
    };
    
    // SO_ATTACH_REUSEPORT_CBPF の値（Linux 4.5+）
    // include/uapi/asm-generic/socket.h: #define SO_ATTACH_REUSEPORT_CBPF 51
    const SO_ATTACH_REUSEPORT_CBPF: libc::c_int = 51;
    
    let result = unsafe {
        libc::setsockopt(
            fd,
            libc::SOL_SOCKET,
            SO_ATTACH_REUSEPORT_CBPF,
            &prog as *const _ as *const libc::c_void,
            std::mem::size_of::<SockFprog>() as libc::socklen_t,
        )
    };
    
    if result < 0 {
        let err = io::Error::last_os_error();
        warn!("Failed to attach CBPF program: {} (errno: {})", err, err.raw_os_error().unwrap_or(-1));
        return Err(err);
    }
    
    Ok(())
}

/// リスナーソケットを作成する（SO_REUSEPORT + オプションのCBPF振り分け）
/// 
/// # 引数
/// * `addr` - バインドするアドレス
/// * `balancing` - 振り分け方式
/// * `num_workers` - ワーカースレッド数（CBPF使用時に必要）
/// * `worker_id` - このワーカーのID（最初のワーカーがCBPFをアタッチ）
fn create_listener(
    addr: SocketAddr,
    #[allow(unused_variables)] balancing: ReuseportBalancing,
    #[allow(unused_variables)] num_workers: usize,
    #[allow(unused_variables)] worker_id: usize,
) -> io::Result<TcpListener> {
    let config = monoio::net::ListenerConfig::default()
        .reuse_port(true)
        .backlog(8192);
    let listener = TcpListener::bind_with_config(addr, &config)?;
    
    // Linux環境でCBPF振り分けが有効な場合、最初のワーカーのみCBPFプログラムをアタッチ
    // 後続のワーカーはreuseportグループに参加し、自動的にBPFプログラムを継承する
    #[cfg(target_os = "linux")]
    if balancing == ReuseportBalancing::Cbpf {
        // CAS操作で最初の1回だけアタッチを実行
        let prev = CBPF_ATTACHED.compare_exchange(
            0,
            1,
            Ordering::SeqCst,
            Ordering::SeqCst,
        );
        
        if prev.is_ok() {
            // このワーカーが最初にリスナーを作成した
            let fd = listener.as_raw_fd();
            match attach_reuseport_cbpf(fd, num_workers) {
                Ok(()) => {
                    info!("[Worker {}] CBPF reuseport load balancing enabled (client IP hash -> {} workers)", 
                          worker_id, num_workers);
                }
                Err(e) => {
                    // CBPFアタッチに失敗した場合はカーネルデフォルトにフォールバック
                    warn!("[Worker {}] CBPF attach failed, falling back to kernel default: {}", 
                          worker_id, e);
                    // フラグをリセットして他のワーカーも試行できるようにする（オプション）
                    // CBPF_ATTACHED.store(0, Ordering::SeqCst);
                }
            }
        }
    }
    
    Ok(listener)
}

// ====================
// 接続処理
// ====================

// kTLS 有効時の接続処理（rustls + ktls2）
#[cfg(feature = "ktls")]
async fn handle_connection(
    stream: TcpStream,
    acceptor: RustlsAcceptor,
    host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: Arc<HashMap<Box<[u8]>, SortedPathMap>>,
    _peer_addr: SocketAddr,
    ktls_config: Arc<KtlsConfig>,
) {
    // TLSハンドシェイクにタイムアウトを設定
    // rustls でハンドシェイク後、ktls2 で kTLS を有効化
    let tls_result = timeout(CONNECT_TIMEOUT, acceptor.accept(stream)).await;
    
    let tls_stream = match tls_result {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS handshake error: {}", e);
            return;
        }
        Err(_) => {
            warn!("TLS handshake timeout");
            return;
        }
    };

    // 警告抑制
    let _ = &ktls_config;

    handle_requests(tls_stream, &host_routes, &path_routes).await;
}

// kTLS 無効時の接続処理（rustls のみ）
#[cfg(not(feature = "ktls"))]
async fn handle_connection(
    stream: TcpStream,
    acceptor: simple_tls::SimpleTlsAcceptor,
    host_routes: Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: Arc<HashMap<Box<[u8]>, SortedPathMap>>,
    _peer_addr: SocketAddr,
    _ktls_config: Arc<KtlsConfig>,
) {
    // TLSハンドシェイクにタイムアウトを設定
    let tls_result = timeout(CONNECT_TIMEOUT, acceptor.accept(stream)).await;
    
    let tls_stream = match tls_result {
        Ok(Ok(tls)) => tls,
        Ok(Err(e)) => {
            warn!("TLS handshake error: {}", e);
            return;
        }
        Err(_) => {
            warn!("TLS handshake timeout");
            return;
        }
    };

    handle_requests(tls_stream, &host_routes, &path_routes).await;
}

// ====================
// リクエスト処理ループ
// ====================

// 統一されたリクエスト処理ループ（型エイリアスを使用）
async fn handle_requests(
    mut tls_stream: ServerTls,
    host_routes: &Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: &Arc<HashMap<Box<[u8]>, SortedPathMap>>,
) {
    let mut accumulated = Vec::with_capacity(BUF_SIZE);

    loop {
        // 読み込み（アイドルタイムアウト付き）
        let read_buf = buf_get();
        let read_result = timeout(IDLE_TIMEOUT, tls_stream.read(read_buf)).await;
        
        let (res, returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                // アイドルタイムアウト - 接続を閉じる
                return;
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                return;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return;
            }
        };
        
        // 読み込んだデータを蓄積
        accumulated.extend_from_slice(&returned_buf[..n]);
        buf_put(returned_buf);

        // ヘッダーサイズ制限チェック
        if accumulated.len() > MAX_HEADER_SIZE {
            let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return;
        }

        // HTTPリクエストをパース
        let mut headers_storage = [httparse::EMPTY_HEADER; 64];
        let mut req = Request::new(&mut headers_storage);
        
        match req.parse(&accumulated) {
            Ok(Status::Complete(header_len)) => {
                // HTTPメソッド取得
                let method_bytes: Box<[u8]> = req.method
                    .map(|m| m.as_bytes().into())
                    .unwrap_or_else(|| Box::from(b"GET" as &[u8]));
                
                // ヘッダー情報抽出
                let host_bytes: Box<[u8]> = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("host"))
                    .map(|h| Box::from(h.value))
                    .unwrap_or_else(|| Box::from([] as [u8; 0]));
                
                let path_bytes: Box<[u8]> = req.path
                    .map(|p| p.as_bytes().into())
                    .unwrap_or_else(|| Box::from(b"/" as &[u8]));
                
                let user_agent: Box<[u8]> = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("user-agent"))
                    .map(|h| Box::from(h.value))
                    .unwrap_or_else(|| Box::from([] as [u8; 0]));
                
                let content_length: usize = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                    .and_then(|h| std::str::from_utf8(h.value).ok())
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0);
                
                // Transfer-Encoding: chunked チェック（改善版）
                let is_chunked: bool = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
                    .map(|h| is_chunked_encoding(h.value))
                    .unwrap_or(false);
                
                // Connection: close チェック（Keep-Alive対応）
                let client_wants_close: bool = req.headers.iter()
                    .find(|h| h.name.eq_ignore_ascii_case("connection"))
                    .map(|h| h.value.eq_ignore_ascii_case(b"close"))
                    .unwrap_or(false);

                // ボディサイズ制限
                if !is_chunked && content_length > MAX_BODY_SIZE {
                    drop(req);
                    let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                let headers_for_proxy: Vec<(Box<[u8]>, Box<[u8]>)> = req.headers.iter()
                    .filter(|h| !h.name.is_empty())
                    .map(|h| (h.name.as_bytes().into(), h.value.into()))
                    .collect();
                
                drop(req);

                // Backend選択
                let backend_result = find_backend(
                    &host_bytes,
                    &path_bytes,
                    host_routes,
                    path_routes,
                );

                let (prefix, backend) = match backend_result {
                    Some(b) => b,
                    None => {
                        let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                };
                
                // セキュリティ設定を取得
                let security = backend.security();
                
                // 許可メソッドチェック
                if !security.allowed_methods.is_empty() {
                    let method_str = std::str::from_utf8(&method_bytes).unwrap_or("GET");
                    let is_allowed = security.allowed_methods.iter()
                        .any(|m| m.eq_ignore_ascii_case(method_str));
                    if !is_allowed {
                        let err_buf = ERR_MSG_METHOD_NOT_ALLOWED.to_vec();
                        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                        return;
                    }
                }
                
                // ルートごとのボディサイズ制限（chunked以外）
                if !is_chunked && content_length > security.max_request_body_size {
                    let err_buf = ERR_MSG_REQUEST_TOO_LARGE.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                    return;
                }

                let start_time = OffsetDateTime::now_utc();
                
                // 初期ボディ（ヘッダー後のデータ）
                let initial_body: Vec<u8> = if header_len < accumulated.len() {
                    accumulated[header_len..].to_vec()
                } else {
                    Vec::new()
                };

                // バッファクリア（次のリクエストに備える）
                accumulated.clear();

                // Backend処理
                let result = handle_backend(
                    tls_stream,
                    backend,
                    &method_bytes,
                    &path_bytes,
                    prefix,
                    content_length,
                    is_chunked,
                    &headers_for_proxy,
                    &initial_body,
                    client_wants_close,
                ).await;

                match result {
                    Some((stream_back, status, resp_size, should_close)) => {
                        log_access(&path_bytes, &user_agent, content_length as u64, status, resp_size, start_time);
                        tls_stream = stream_back;
                        
                        // Connection: close が要求された場合、またはエラー時は接続を閉じる
                        if should_close {
                            return;
                        }
                        // Keep-Alive: ループを継続して次のリクエストを待機
                    }
                    None => {
                        return;
                    }
                }
            }
            Ok(Status::Partial) => {
                // データ不足、次の読み込みを待つ
                continue;
            }
            Err(_) => {
                let err_buf = ERR_MSG_BAD_REQUEST.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return;
            }
        }
    }
}

// ====================
// HTTPヘッダー検証（Header Injection防止）
// ====================
//
// httparseがパースしたヘッダーを再検証し、不正な文字を含む
// ヘッダーを除外することで、HTTP Request Smuggling攻撃を防止します。
// 多層防御（Defense in Depth）の原則に基づく追加チェックです。
// ====================

/// ヘッダー名が有効か検証（RFC 7230 token準拠）
/// 
/// token = 1*tchar
/// tchar = "!" / "#" / "$" / "%" / "&" / "'" / "*" / "+" / "-" / "." /
///         "^" / "_" / "`" / "|" / "~" / DIGIT / ALPHA
#[inline]
fn is_valid_header_name(name: &[u8]) -> bool {
    if name.is_empty() {
        return false;
    }
    for &b in name {
        let is_tchar = matches!(b,
            b'!' | b'#' | b'$' | b'%' | b'&' | b'\'' | b'*' | b'+' | b'-' | b'.' |
            b'^' | b'_' | b'`' | b'|' | b'~' |
            b'0'..=b'9' | b'A'..=b'Z' | b'a'..=b'z'
        );
        if !is_tchar {
            return false;
        }
    }
    true
}

/// ヘッダー値が有効か検証（Header Injection防止）
/// 
/// RFC 7230 field-value に基づき、以下を禁止:
/// - CR (\r): ヘッダーインジェクションの主要ベクトル
/// - LF (\n): ヘッダーインジェクションの主要ベクトル
/// - NUL (0x00): セキュリティ上の理由
/// 
/// obs-fold（折り返しヘッダー）は許容しない方針とする。
/// これにより、プロキシとバックエンド間の解釈の違いを悪用した
/// HTTP Request Smuggling攻撃を防止する。
/// 
/// ## 実装詳細
/// 
/// `memchr`クレートのSIMD最適化された`memchr3`関数を使用して、
/// 3つの禁止文字（CR, LF, NUL）を並列に検索します。
/// 
/// - AVX2対応CPUでは32バイト単位で並列検査
/// - SSE2対応CPUでは16バイト単位で並列検査
/// - 小さな入力では自動的に最適なフォールバックを選択
/// 
/// これにより、大きなヘッダー値（Cookie、Authorization等）の
/// 検証パフォーマンスが向上します。
#[inline]
fn is_valid_header_value(value: &[u8]) -> bool {
    // memchr3: 3つの文字を一度に検索（SIMD最適化）
    // いずれかの禁止文字が見つかった場合はSome(位置)を返す
    // 見つからなければNone -> 有効なヘッダー値
    memchr3(b'\r', b'\n', 0, value).is_none()
}

// ====================
// Transfer-Encoding: chunked 検出（改善版）
// ====================

/// Transfer-Encoding ヘッダー値から chunked かどうかを正確に判定
#[inline]
fn is_chunked_encoding(value: &[u8]) -> bool {
    // カンマ区切りの各値をチェック
    for part in value.split(|&b| b == b',') {
        // 空白をトリム
        let trimmed: Vec<u8> = part.iter()
            .skip_while(|&&b| b == b' ' || b == b'\t')
            .copied()
            .collect();
        let trimmed: Vec<u8> = trimmed.iter()
            .rev()
            .skip_while(|&&b| b == b' ' || b == b'\t')
            .copied()
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        
        if trimmed.eq_ignore_ascii_case(b"chunked") {
            return true;
        }
    }
    false
}

// ====================
// Backend選択
// ====================

fn find_backend(
    host: &[u8],
    path: &[u8],
    host_routes: &Arc<HashMap<Box<[u8]>, Backend>>,
    path_routes: &Arc<HashMap<Box<[u8]>, SortedPathMap>>,
) -> Option<(Box<[u8]>, Backend)> {
    if let Some(backend) = host_routes.get(host) {
        return Some((Box::new([]), backend.clone()));
    }
    
    if let Some(sorted_map) = path_routes.get(host) {
        if let Some((prefix, backend)) = sorted_map.find_longest(path) {
            return Some((prefix.into(), backend.clone()));
        }
    }
    
    None
}

// ====================
// Backend処理
// ====================

// 統一された Backend 処理（型エイリアスを使用）
async fn handle_backend(
    mut tls_stream: ServerTls,
    backend: Backend,
    method: &[u8],
    req_path: &[u8],
    prefix: Box<[u8]>,
    content_length: usize,
    is_chunked: bool,
    headers: &[(Box<[u8]>, Box<[u8]>)],
    initial_body: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    match backend {
        Backend::Proxy(target, security) => {
            handle_proxy(tls_stream, &target, &security, method, req_path, &prefix, content_length, is_chunked, headers, initial_body, client_wants_close).await
        }
        Backend::MemoryFile(data, mime_type, _security) => {
            // ファイル完全一致チェック
            // MemoryFileはファイル指定なので、プレフィックス以降にパスがあれば404
            let path_str = std::str::from_utf8(req_path).unwrap_or("/");
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
            
            let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
                &path_str[prefix_str.len()..]
            } else {
                ""
            };
            
            let clean_remainder = remainder.trim_matches('/');
            if !clean_remainder.is_empty() {
                // ファイル指定なのにさらにパスが続いている場合は404
                let err_buf = ERR_MSG_NOT_FOUND.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 404, 0, true));
            }
            
            // Keep-Alive対応: クライアントの要求に応じてConnectionヘッダーを動的に生成
            let mut header = Vec::with_capacity(HEADER_BUF_CAPACITY);
            header.extend_from_slice(HTTP_200_PREFIX);
            header.extend_from_slice(mime_type.as_bytes());
            header.extend_from_slice(CONTENT_LENGTH_HEADER);
            let mut num_buf = itoa::Buffer::new();
            header.extend_from_slice(num_buf.format(data.len()).as_bytes());
            if client_wants_close {
                header.extend_from_slice(CONNECTION_CLOSE);
            } else {
                header.extend_from_slice(CONNECTION_KEEP_ALIVE);
            }
            
            // ヘッダー送信（タイムアウト付き）
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header)).await;
            match write_result {
                Ok((Ok(_), _)) => {}
                _ => return None,
            }
            
            // ボディ送信（タイムアウト付き）
            let data_len = data.len() as u64;
            let data_buf = data.to_vec();
            let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(data_buf)).await;
            match write_result {
                Ok((Ok(_), _)) => {
                    Some((tls_stream, 200, data_len, client_wants_close))
                }
                _ => None,
            }
        }
        Backend::SendFile(base_path, is_dir, index_file, _security) => {
            handle_sendfile(tls_stream, &base_path, is_dir, index_file.as_deref(), req_path, &prefix, client_wants_close).await
        }
    }
}

// ====================
// HTTPレスポンスパーサー（httparse使用）
// ====================

/// httparseを使用したレスポンス解析結果
struct ParsedResponse {
    /// ステータスコード
    status_code: u16,
    /// ヘッダー終端位置（ボディ開始位置）
    header_len: usize,
    /// Content-Length（存在する場合）
    content_length: Option<usize>,
    /// Transfer-Encoding: chunked かどうか
    is_chunked: bool,
    /// Connection: close かどうか（HTTP/1.1ではデフォルトはkeep-alive）
    is_connection_close: bool,
}

/// HTTPレスポンスをhttparseで解析
/// 
/// httparseを使用することで以下のメリットがある:
/// - RFC準拠の堅牢なパース
/// - \r\n と \n の両方に対応
/// - ヘッダー折り返し（obs-fold）の処理
/// - 不正なHTTPレスポンスの検出
fn parse_http_response(data: &[u8]) -> Option<ParsedResponse> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);
    
    match response.parse(data) {
        Ok(Status::Complete(header_len)) => {
            let status_code = response.code.unwrap_or(502);
            
            // Content-Length を取得
            let content_length = response.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("content-length"))
                .and_then(|h| std::str::from_utf8(h.value).ok())
                .and_then(|s| s.trim().parse().ok());
            
            // Transfer-Encoding: chunked をチェック
            let is_chunked = response.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("transfer-encoding"))
                .map(|h| is_chunked_encoding(h.value))
                .unwrap_or(false);
            
            // Connection: close をチェック（HTTP/1.1ではデフォルトはkeep-alive）
            let is_connection_close = response.headers.iter()
                .find(|h| h.name.eq_ignore_ascii_case("connection"))
                .map(|h| {
                    // 値をトリムして比較
                    let value = h.value;
                    let trimmed: Vec<u8> = value.iter()
                        .skip_while(|&&b| b == b' ' || b == b'\t')
                        .copied()
                        .collect();
                    trimmed.eq_ignore_ascii_case(b"close")
                })
                .unwrap_or(false);
            
            Some(ParsedResponse {
                status_code,
                header_len,
                content_length,
                is_chunked,
                is_connection_close,
            })
        }
        Ok(Status::Partial) => None, // データ不足
        Err(_) => None, // パースエラー
    }
}

/// HTTPレスポンスヘッダーが完全に受信されているかチェック
/// 
/// httparseを使用して、ヘッダーが完全に受信されたかを判定します。
/// 完全な場合はヘッダー終端位置（ボディ開始位置）を返します。
#[inline]
#[allow(dead_code)]
fn check_response_header_complete(data: &[u8]) -> Option<usize> {
    let mut headers_storage = [httparse::EMPTY_HEADER; 64];
    let mut response = httparse::Response::new(&mut headers_storage);
    
    match response.parse(data) {
        Ok(Status::Complete(header_len)) => Some(header_len),
        _ => None,
    }
}

// ====================
// Chunked Transfer Encoding パーサー（RFC 7230 Section 4.1 準拠）
// ====================
//
// Chunked-Bodyの構文:
//   chunked-body   = *chunk last-chunk trailer-part CRLF
//   chunk          = chunk-size [ chunk-ext ] CRLF chunk-data CRLF
//   chunk-size     = 1*HEXDIG
//   last-chunk     = 1*("0") [ chunk-ext ] CRLF
//   trailer-part   = *( header-field CRLF )
//
// トレーラーが存在する場合でも正確に終端を検出するために
// ステートマシンベースのパーサーを使用します。
// ====================

/// Chunkedデコーダの状態
#[derive(Debug, Clone, Copy, PartialEq)]
enum ChunkedState {
    /// チャンクサイズの16進数を読み取り中
    ReadingChunkSize,
    /// チャンク拡張（;以降）を読み取り中（サイズ行の終わりまでスキップ）
    ReadingChunkExtension,
    /// チャンクサイズ行の\r後、\nを期待
    ExpectingChunkSizeLF,
    /// チャンクデータを読み取り中（残りバイト数をchunk_remainingで追跡）
    ReadingChunkData,
    /// チャンクデータ後の\rを期待
    ExpectingChunkDataCR,
    /// チャンクデータ後の\nを期待
    ExpectingChunkDataLF,
    /// トレーラーヘッダーまたは終端の空行を読み取り中
    /// 空行（\r\n）で完了、それ以外はトレーラーヘッダー
    ReadingTrailerLine,
    /// トレーラー行または終端の\r後、\nを期待
    ExpectingTrailerLF,
    /// 転送完了
    Complete,
    /// サイズ制限超過（DoS対策）
    SizeLimitExceeded,
}

/// Chunkedデコーダのフィード結果
#[derive(Debug, Clone, Copy, PartialEq)]
enum ChunkedFeedResult {
    /// まだ転送中
    Continue,
    /// 転送完了
    Complete,
    /// サイズ制限超過
    SizeLimitExceeded,
}

/// Chunked転送デコーダ（ステートマシン）
/// 
/// RFC 7230 Section 4.1に準拠し、トレーラーの有無にかかわらず
/// 正確に終端を検出します。
/// 
/// DoS対策として累積サイズの制限機能を持ちます。
#[derive(Debug, Clone)]
struct ChunkedDecoder {
    /// 現在の状態
    state: ChunkedState,
    /// 現在のチャンクの残りバイト数
    chunk_remaining: u64,
    /// チャンクサイズの解析中に蓄積する16進数値
    size_accumulator: u64,
    /// サイズに少なくとも1文字は含まれているか
    size_has_digit: bool,
    /// トレーラー行が空かどうか（終端検出用）
    trailer_line_empty: bool,
    /// 累積ボディサイズ（DoS対策）
    total_body_size: u64,
    /// 最大許容ボディサイズ（0の場合は制限なし）
    max_body_size: u64,
}

impl ChunkedDecoder {
    /// 新しいChunkedDecoderを作成（サイズ制限付き）
    /// 
    /// # Arguments
    /// * `max_body_size` - 最大許容ボディサイズ（0の場合は制限なし）
    fn new(max_body_size: u64) -> Self {
        Self {
            state: ChunkedState::ReadingChunkSize,
            chunk_remaining: 0,
            size_accumulator: 0,
            size_has_digit: false,
            trailer_line_empty: true,
            total_body_size: 0,
            max_body_size,
        }
    }
    
    /// 新しいChunkedDecoderを作成（制限なし - レスポンス用）
    fn new_unlimited() -> Self {
        Self::new(0)
    }
    
    /// データをフィードして状態を更新
    /// 完了またはサイズ制限超過の場合は適切な結果を返す
    fn feed(&mut self, data: &[u8]) -> ChunkedFeedResult {
        for &byte in data {
            match self.feed_byte(byte) {
                ChunkedFeedResult::Continue => continue,
                result => return result,
            }
        }
        ChunkedFeedResult::Continue
    }
    
    /// サイズ制限を超過したかどうか
    #[inline]
    #[allow(dead_code)]
    fn is_size_exceeded(&self) -> bool {
        self.state == ChunkedState::SizeLimitExceeded
    }
    
    /// 1バイトを処理して状態を更新
    /// 完了またはサイズ制限超過の場合は適切な結果を返す
    #[inline]
    fn feed_byte(&mut self, byte: u8) -> ChunkedFeedResult {
        match self.state {
            ChunkedState::ReadingChunkSize => {
                match byte {
                    b'0'..=b'9' => {
                        self.size_accumulator = self.size_accumulator.saturating_mul(16)
                            .saturating_add((byte - b'0') as u64);
                        self.size_has_digit = true;
                    }
                    b'a'..=b'f' => {
                        self.size_accumulator = self.size_accumulator.saturating_mul(16)
                            .saturating_add((byte - b'a' + 10) as u64);
                        self.size_has_digit = true;
                    }
                    b'A'..=b'F' => {
                        self.size_accumulator = self.size_accumulator.saturating_mul(16)
                            .saturating_add((byte - b'A' + 10) as u64);
                        self.size_has_digit = true;
                    }
                    b';' => {
                        // チャンク拡張の開始
                        self.state = ChunkedState::ReadingChunkExtension;
                    }
                    b'\r' => {
                        self.state = ChunkedState::ExpectingChunkSizeLF;
                    }
                    _ => {
                        // 不正な文字 - 回復のためスキップ（緩い解析）
                    }
                }
            }
            
            ChunkedState::ReadingChunkExtension => {
                // チャンク拡張はCRまでスキップ
                if byte == b'\r' {
                    self.state = ChunkedState::ExpectingChunkSizeLF;
                }
            }
            
            ChunkedState::ExpectingChunkSizeLF => {
                if byte == b'\n' {
                    if !self.size_has_digit {
                        // サイズが解析できなかった場合、リセット
                        self.state = ChunkedState::ReadingChunkSize;
                    } else if self.size_accumulator == 0 {
                        // 最後のチャンク（サイズ0）- トレーラーセクションへ
                        self.state = ChunkedState::ReadingTrailerLine;
                        self.trailer_line_empty = true;
                    } else {
                        // 通常のチャンク - データセクションへ
                        // サイズ制限チェック（DoS対策）
                        if self.max_body_size > 0 {
                            let new_total = self.total_body_size.saturating_add(self.size_accumulator);
                            if new_total > self.max_body_size {
                                self.state = ChunkedState::SizeLimitExceeded;
                                return ChunkedFeedResult::SizeLimitExceeded;
                            }
                            self.total_body_size = new_total;
                        }
                        self.chunk_remaining = self.size_accumulator;
                        self.state = ChunkedState::ReadingChunkData;
                    }
                    // 次のチャンクのためにリセット
                    self.size_accumulator = 0;
                    self.size_has_digit = false;
                } else {
                    // LFが来なかった - リセット（緩い解析）
                    self.state = ChunkedState::ReadingChunkSize;
                    self.size_accumulator = 0;
                    self.size_has_digit = false;
                }
            }
            
            ChunkedState::ReadingChunkData => {
                self.chunk_remaining = self.chunk_remaining.saturating_sub(1);
                if self.chunk_remaining == 0 {
                    self.state = ChunkedState::ExpectingChunkDataCR;
                }
            }
            
            ChunkedState::ExpectingChunkDataCR => {
                if byte == b'\r' {
                    self.state = ChunkedState::ExpectingChunkDataLF;
                } else {
                    // 不正な形式 - 次のチャンクを探す（緩い解析）
                    self.state = ChunkedState::ReadingChunkSize;
                }
            }
            
            ChunkedState::ExpectingChunkDataLF => {
                if byte == b'\n' {
                    self.state = ChunkedState::ReadingChunkSize;
                } else {
                    // 不正な形式 - リセット
                    self.state = ChunkedState::ReadingChunkSize;
                }
            }
            
            ChunkedState::ReadingTrailerLine => {
                match byte {
                    b'\r' => {
                        self.state = ChunkedState::ExpectingTrailerLF;
                    }
                    _ => {
                        // トレーラーヘッダーの内容
                        self.trailer_line_empty = false;
                    }
                }
            }
            
            ChunkedState::ExpectingTrailerLF => {
                if byte == b'\n' {
                    if self.trailer_line_empty {
                        // 空行 = 転送完了
                        self.state = ChunkedState::Complete;
                        return ChunkedFeedResult::Complete;
                    } else {
                        // トレーラーヘッダー行が完了、次の行へ
                        self.state = ChunkedState::ReadingTrailerLine;
                        self.trailer_line_empty = true;
                    }
                } else {
                    // 不正な形式だが、トレーラー読み取りを継続
                    self.state = ChunkedState::ReadingTrailerLine;
                    self.trailer_line_empty = false;
                }
            }
            
            ChunkedState::Complete => {
                return ChunkedFeedResult::Complete;
            }
            
            ChunkedState::SizeLimitExceeded => {
                return ChunkedFeedResult::SizeLimitExceeded;
            }
        }
        ChunkedFeedResult::Continue
    }
    
    /// 転送が完了したかどうかを返す
    #[inline]
    #[allow(dead_code)]
    fn is_complete(&self) -> bool {
        self.state == ChunkedState::Complete
    }
}

// ====================
// プロキシ処理
// ====================
//
// バックエンドコネクションプールを使用して、接続を再利用します。
// Connection: keep-alive をバックエンドに送信し、レスポンスの
// Connection ヘッダーに基づいて接続をプールに返却します。
// ====================

async fn handle_proxy(
    client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    method: &[u8],
    req_path: &[u8],
    prefix: &[u8],
    content_length: usize,
    is_chunked: bool,
    headers: &[(Box<[u8]>, Box<[u8]>)],
    initial_body: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    let pool_key = format!("{}:{}", target.host, target.port);
    
    // リクエストパス構築
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if path_str.starts_with(prefix_str) {
            let remaining = &path_str[prefix_str.len()..];
            let base = target.path_prefix.trim_end_matches('/');
            
            if remaining.is_empty() {
                if base.is_empty() {
                    "/".to_string()
                } else {
                    format!("{}/", base)
                }
            } else if remaining.starts_with('/') {
                if base.is_empty() {
                    remaining.to_string()
                } else {
                    format!("{}{}", base, remaining)
                }
            } else {
                if base.is_empty() {
                    format!("/{}", remaining)
                } else {
                    format!("{}/{}", base, remaining)
                }
            }
        } else {
            path_str.to_string()
        }
    };
    
    let final_path = if sub_path.is_empty() { "/" } else { &sub_path };

    // HTTPリクエスト構築（Connection: keep-alive を使用）
    let mut request = Vec::with_capacity(1024);
    request.extend_from_slice(method);
    request.extend_from_slice(b" ");
    request.extend_from_slice(final_path.as_bytes());
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(target.host.as_bytes());
    
    if !target.is_default_port() {
        request.extend_from_slice(b":");
        let mut port_buf = itoa::Buffer::new();
        request.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    
    request.extend_from_slice(b"\r\n");
    
    for (name, value) in headers {
        // host と connection ヘッダーは別途処理済みのためスキップ
        if name.eq_ignore_ascii_case(b"host") || name.eq_ignore_ascii_case(b"connection") {
            continue;
        }
        
        // Header Injection防止: ヘッダー名と値の検証
        // httparseによるパース後も、多層防御として再検証を行う
        // 不正な文字（CR, LF, NUL等）を含むヘッダーは除外
        if !is_valid_header_name(name) {
            warn!("Invalid header name detected, skipping: {:?}", 
                  String::from_utf8_lossy(name));
            continue;
        }
        if !is_valid_header_value(value) {
            warn!("Invalid header value detected (possible header injection), skipping header: {:?}", 
                  String::from_utf8_lossy(name));
            continue;
        }
        
        request.extend_from_slice(name);
        request.extend_from_slice(b": ");
        request.extend_from_slice(value);
        request.extend_from_slice(b"\r\n");
    }
    // バックエンドにはKeep-Aliveを要求
    request.extend_from_slice(b"Connection: keep-alive\r\n\r\n");

    if target.use_tls {
        proxy_https_pooled(client_stream, target, security, &pool_key, request, content_length, is_chunked, initial_body, client_wants_close).await
    } else {
        proxy_http_pooled(client_stream, target, security, &pool_key, request, content_length, is_chunked, initial_body, client_wants_close).await
    }
}

// ====================
// HTTP プロキシ（コネクションプール対応）
// ====================

async fn proxy_http_pooled(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // プールから接続を取得、または新規作成
    let mut backend_stream = match HTTP_POOL.with(|p| p.borrow_mut().get(pool_key)) {
        Some(stream) => stream,
        None => {
            // 新規接続を作成
            let addr = format!("{}:{}", target.host, target.port);
            let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
            
            match connect_result {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    stream
                }
                Ok(Err(e)) => {
                    error!("Proxy connect error to {}: {}", addr, e);
                    let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 502, 0, true));
                }
                Err(_) => {
                    error!("Proxy connect timeout to {}", addr);
                    let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 504, 0, true));
                }
            }
        }
    };

    // セキュリティ設定からchunked最大サイズを取得
    let max_chunked = security.max_chunked_body_size as u64;
    
    // リクエスト送信とレスポンス受信
    // kTLS 有効時は splice(2) を使用してゼロコピー転送
    #[cfg(feature = "ktls")]
    let result = {
        // kTLS + splice 版を試みる（Content-Length の場合のみ有効）
        if client_stream.is_ktls_enabled() && !is_chunked {
            let splice_result = proxy_http_request_splice(
                &client_stream,
                &backend_stream,
                &request,
                content_length,
                is_chunked,
                initial_body,
            ).await;
            
            if splice_result.is_some() {
                splice_result
            } else {
                // splice 版が失敗した場合は通常版にフォールバック
                proxy_http_request(
                    &mut client_stream,
                    &mut backend_stream,
                    request,
                    content_length,
                    is_chunked,
                    initial_body,
                    max_chunked,
                ).await
            }
        } else {
            // kTLS が無効または Chunked の場合は通常版を使用
            proxy_http_request(
                &mut client_stream,
                &mut backend_stream,
                request,
                content_length,
                is_chunked,
                initial_body,
                max_chunked,
            ).await
        }
    };
    
    #[cfg(not(feature = "ktls"))]
    let result = proxy_http_request(
        &mut client_stream,
        &mut backend_stream,
        request,
        content_length,
        is_chunked,
        initial_body,
        max_chunked,
    ).await;

    match result {
        Some((status_code, total, backend_wants_keep_alive)) => {
            // バックエンドがKeep-Aliveを許可している場合、プールに返却
            if backend_wants_keep_alive {
                HTTP_POOL.with(|p| p.borrow_mut().put(pool_key.to_string(), backend_stream));
            }
            Some((client_stream, status_code, total, client_wants_close))
        }
        None => {
            // エラー発生時は接続を破棄
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            Some((client_stream, 502, 0, true))
        }
    }
}

/// HTTPリクエストを送信してレスポンスを受信（内部関数）
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
async fn proxy_http_request(
    client_stream: &mut ServerTls,
    backend_stream: &mut TcpStream,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
) -> Option<(u16, u64, bool)> {
    // 1. リクエストヘッダー送信（タイムアウト付き）
    let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // 2. リクエストボディ送信
    if !initial_body.is_empty() {
        let body_buf = initial_body.to_vec();
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(body_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    if is_chunked {
        // Chunked転送の場合（DoS対策: ルートごとのmax_chunked_body_sizeで制限）
        match transfer_chunked_body(client_stream, backend_stream, initial_body, max_chunked_body_size).await {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            ChunkedTransferResult::SizeLimitExceeded => {
                // サイズ制限超過 - 413エラーを返すべきだが、
                // ここではバックエンド接続を閉じて失敗として扱う
                return None;
            }
        }
    } else {
        // Content-Length転送の場合
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes(client_stream, backend_stream, remaining).await;
            if transferred < remaining as u64 {
                return None;
            }
        }
    }

    // 4. レスポンスを受信して転送（Connectionヘッダーも取得）
    let (total, status_code, backend_wants_keep_alive) = transfer_response_with_keepalive(backend_stream, client_stream).await;

    Some((status_code, total, backend_wants_keep_alive))
}

// ====================
// kTLS + splice(2) によるHTTPプロキシ（高速版）
// ====================
//
// kTLS が有効な場合、splice(2) を使用してカーネル空間で直接
// データを転送します。HTTPバックエンド（平文）への接続で効果的です。
//
// 注意: Chunked 転送の場合は終端検出のためユーザー空間での
// 処理が必要なため、splice は使用しません。
// ====================

/// kTLS + splice によるボディ転送（Content-Length固定長のみ）
///
/// FD間でsplice(2)を使用してゼロコピー転送を行います。
/// 非ブロッキングソケットに対応し、WouldBlockの場合は待機します。
///
/// splice(2) によるボディ転送（固定長）
#[cfg(feature = "ktls")]
async fn splice_body_transfer(
    src_stream: &TcpStream,
    dst_stream: &TcpStream,
    pipe: &ktls_rustls::SplicePipe,
    mut remaining: usize,
) -> u64 {
    use std::os::unix::io::AsRawFd;
    
    let src_fd = src_stream.as_raw_fd();
    let dst_fd = dst_stream.as_raw_fd();
    let mut total = 0u64;
    
    const SPLICE_CHUNK_SIZE: usize = 65536;
    
    while remaining > 0 {
        let chunk_size = remaining.min(SPLICE_CHUNK_SIZE);
        
        match pipe.transfer(src_fd, dst_fd, chunk_size) {
            Ok(0) => break,
            Ok(n) => {
                total += n as u64;
                remaining = remaining.saturating_sub(n);
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                // 読み取り可能になるまで待機
                if let Err(_) = src_stream.readable(false).await {
                    break;
                }
            }
            Err(e) => {
                warn!("splice body transfer error: {}", e);
                break;
            }
        }
    }
    
    total
}

// ====================
// kTLS + splice(2) によるHTTPプロキシ転送
// ====================
//
// kTLS が有効な場合、以下のフローでゼロコピー転送を実現：
//
// [リクエスト] クライアント(kTLS) → splice → バックエンド(TCP)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ(Content-Length): splice(2) でゼロコピー転送
//   3. ボディ(Chunked): 通常の転送（終端検出が必要）
//
// [レスポンス] バックエンド(TCP) → splice → クライアント(kTLS)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ(Content-Length): splice(2) でゼロコピー転送
//   3. ボディ(Chunked): 通常の転送（終端検出が必要）
// ====================

/// kTLS + splice を使用したHTTPプロキシリクエスト処理
///
/// Content-Length が指定されている場合はボディ転送に splice を使用。
/// Chunked 転送の場合は通常の転送を使用。
#[cfg(feature = "ktls")]
async fn proxy_http_request_splice(
    client_stream: &KtlsServerStream,
    backend_stream: &TcpStream,
    request: &[u8],
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
) -> Option<(u16, u64, bool)> {
    // splice パイプを取得
    let pipe_ref = get_splice_pipe();
    let pipe = match pipe_ref.as_ref() {
        Some(p) => p,
        None => {
            warn!("splice pipe not available, falling back to normal transfer");
            return None;
        }
    };
    
    // kTLS が有効でない場合はフォールバック
    if !client_stream.is_ktls_enabled() {
        return None;
    }
    
    let client_tcp = client_stream.get_ref();
    
    // 1. リクエストヘッダーをバックエンドに送信（raw_write）
    if let Err(e) = async_raw_write_all(backend_stream, request).await {
        warn!("Failed to send request header: {}", e);
        return None;
    }
    
    // 2. 初期ボディがあれば送信
    if !initial_body.is_empty() {
        if let Err(e) = async_raw_write_all(backend_stream, initial_body).await {
            warn!("Failed to send initial body: {}", e);
            return None;
        }
    }
    
    // 3. 残りのリクエストボディを転送
    let remaining_body = content_length.saturating_sub(initial_body.len());
    if remaining_body > 0 {
        if is_chunked {
            // Chunked 転送はフォールバック（終端検出が必要）
            return None;
        }
        
        // Content-Length の場合: splice でゼロコピー転送
        // kTLS クライアント → バックエンド TCP
        let transferred = splice_body_transfer(
            client_tcp,
            backend_stream,
            pipe,
            remaining_body,
        ).await;
        
        if transferred < remaining_body as u64 {
            warn!("Request body transfer incomplete: {} < {}", transferred, remaining_body);
            return None;
        }
    }
    
    // 4. レスポンスを受信して転送（splice 使用）
    let result = splice_transfer_response_ktls(
        backend_stream,
        client_stream,
        pipe,
    ).await;
    
    Some(result)
}

/// kTLS + splice によるレスポンス転送
///
/// バックエンド(TCP) からヘッダーを読み取り、パースしてクライアント(kTLS)に送信。
/// ボディは Content-Length の場合は splice、Chunked の場合は通常転送。
#[cfg(feature = "ktls")]
async fn splice_transfer_response_ktls(
    backend_stream: &TcpStream,
    client_stream: &KtlsServerStream,
    pipe: &ktls_rustls::SplicePipe,
) -> (u16, u64, bool) {
    let client_tcp = client_stream.get_ref();
    
    let mut total = 0u64;
    let mut status_code = 502u16;
    let mut accumulated = Vec::with_capacity(4096);
    let mut backend_wants_keep_alive: bool;
    
    // ヘッダー読み取り用バッファ
    let mut header_buf = [0u8; 8192];
    
    // 1. ヘッダーを読み取り（raw_read + パース）
    loop {
        // バックエンドからヘッダーを読み取り
        let n = match async_raw_read(backend_stream, &mut header_buf).await {
            Ok(0) => {
                // EOF
                return (status_code, total, false);
            }
            Ok(n) => n,
            Err(e) => {
                warn!("Failed to read response header: {}", e);
                return (status_code, total, false);
            }
        };
        
        accumulated.extend_from_slice(&header_buf[..n]);
        
        // ヘッダーが完全に受信されたかチェック
        if let Some(parsed) = parse_http_response(&accumulated) {
            status_code = parsed.status_code;
            backend_wants_keep_alive = !parsed.is_connection_close;
            
            let header_len = parsed.header_len;
            let body_start_len = accumulated.len().saturating_sub(header_len);
            
            // ヘッダー + 初期ボディをクライアントに送信（raw_write）
            if let Err(e) = async_raw_write_all(client_tcp, &accumulated).await {
                warn!("Failed to send response header: {}", e);
                return (status_code, total, false);
            }
            total += accumulated.len() as u64;
            
            // ボディ転送
            if parsed.is_chunked {
                // Chunked 転送: 通常の方法で転送（終端検出が必要）
                // レスポンス受信時は制限なし（バックエンドを信頼）
                let mut chunked_decoder = ChunkedDecoder::new_unlimited();
                
                // 初期ボディ部分をデコーダにフィード
                if body_start_len > 0 {
                    if chunked_decoder.feed(&accumulated[header_len..]) == ChunkedFeedResult::Complete {
                        // 初期ボディで完了
                        return (status_code, total, backend_wants_keep_alive);
                    }
                }
                
                // 残りの Chunked ボディを転送
                loop {
                    let n = match async_raw_read(backend_stream, &mut header_buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => {
                            backend_wants_keep_alive = false;
                            break;
                        }
                    };
                    
                    let feed_result = chunked_decoder.feed(&header_buf[..n]);
                    
                    if let Err(_) = async_raw_write_all(client_tcp, &header_buf[..n]).await {
                        backend_wants_keep_alive = false;
                        break;
                    }
                    total += n as u64;
                    
                    if feed_result == ChunkedFeedResult::Complete {
                        break;
                    }
                }
            } else if let Some(content_length) = parsed.content_length {
                // Content-Length 転送: splice でゼロコピー
                let remaining = content_length.saturating_sub(body_start_len);
                
                if remaining > 0 {
                    let transferred = splice_body_transfer(
                        backend_stream,
                        client_tcp,
                        pipe,
                        remaining,
                    ).await;
                    
                    total += transferred;
                    
                    if transferred < remaining as u64 {
                        backend_wants_keep_alive = false;
                    }
                }
            } else {
                // Content-Length も Chunked もない場合: 接続クローズまで読み取り
                // この場合は Keep-Alive 不可
                backend_wants_keep_alive = false;
                
                loop {
                    let n = match async_raw_read(backend_stream, &mut header_buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    
                    if let Err(_) = async_raw_write_all(client_tcp, &header_buf[..n]).await {
                        break;
                    }
                    total += n as u64;
                }
            }
            
            return (status_code, total, backend_wants_keep_alive);
        }
        
        // ヘッダーが大きすぎる場合は中止
        if accumulated.len() > MAX_HEADER_SIZE {
            warn!("Response header too large");
            return (502, 0, false);
        }
    }
}

// ====================
// HTTPS プロキシ（コネクションプール対応）
// ====================

async fn proxy_https_pooled(
    mut client_stream: ServerTls,
    target: &ProxyTarget,
    security: &SecurityConfig,
    pool_key: &str,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    // セキュリティ設定からタイムアウトを取得
    let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
    
    // プールから接続を取得、または新規作成
    let mut backend_stream = match HTTPS_POOL.with(|p| p.borrow_mut().get(pool_key)) {
        Some(stream) => stream,
        None => {
            // 新規TCP接続を作成
            let addr = format!("{}:{}", target.host, target.port);
            let connect_result = timeout(connect_timeout, TcpStream::connect(&addr)).await;
            
            let backend_tcp = match connect_result {
                Ok(Ok(stream)) => {
                    let _ = stream.set_nodelay(true);
                    stream
                }
                Ok(Err(e)) => {
                    error!("Proxy connect error to {}: {}", addr, e);
                    let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 502, 0, true));
                }
                Err(_) => {
                    error!("Proxy connect timeout to {}", addr);
                    let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 504, 0, true));
                }
            };
            
            // TLS接続（タイムアウト付き）
            let connector = TLS_CONNECTOR.with(|c| c.clone());
            let tls_result = timeout(connect_timeout, connector.connect(backend_tcp, &target.host)).await;
            
            match tls_result {
                Ok(Ok(stream)) => stream,
                Ok(Err(e)) => {
                    error!("TLS connect error to {}: {}", target.host, e);
                    let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 502, 0, true));
                }
                Err(_) => {
                    error!("TLS connect timeout to {}", target.host);
                    let err_buf = ERR_MSG_GATEWAY_TIMEOUT.to_vec();
                    let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
                    return Some((client_stream, 504, 0, true));
                }
            }
        }
    };

    // セキュリティ設定からchunked最大サイズを取得
    let max_chunked = security.max_chunked_body_size as u64;
    
    // リクエスト送信とレスポンス受信
    let result = proxy_https_request(
        &mut client_stream,
        &mut backend_stream,
        request,
        content_length,
        is_chunked,
        initial_body,
        max_chunked,
    ).await;

    match result {
        Some((status_code, total, backend_wants_keep_alive)) => {
            // バックエンドがKeep-Aliveを許可している場合、プールに返却
            if backend_wants_keep_alive {
                HTTPS_POOL.with(|p| p.borrow_mut().put(pool_key.to_string(), backend_stream));
            }
            Some((client_stream, status_code, total, client_wants_close))
        }
        None => {
            // エラー発生時は接続を破棄
            let err_buf = ERR_MSG_BAD_GATEWAY.to_vec();
            let _ = timeout(WRITE_TIMEOUT, client_stream.write_all(err_buf)).await;
            Some((client_stream, 502, 0, true))
        }
    }
}

/// HTTPSリクエストを送信してレスポンスを受信（内部関数）
/// 戻り値: Option<(status_code, response_size, backend_wants_keep_alive)>
async fn proxy_https_request(
    client_stream: &mut ServerTls,
    backend_stream: &mut ClientTls,
    request: Vec<u8>,
    content_length: usize,
    is_chunked: bool,
    initial_body: &[u8],
    max_chunked_body_size: u64,
) -> Option<(u16, u64, bool)> {
    // 1. リクエストヘッダー送信
    let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // 2. リクエストボディ送信
    if !initial_body.is_empty() {
        let body_buf = initial_body.to_vec();
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(body_buf)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            return None;
        }
    }

    // 3. 残りのリクエストボディを転送
    if is_chunked {
        // Chunked転送の場合（DoS対策: ルートごとのmax_chunked_body_sizeで制限）
        match transfer_chunked_body(client_stream, backend_stream, initial_body, max_chunked_body_size).await {
            ChunkedTransferResult::Complete => {}
            ChunkedTransferResult::Failed => return None,
            ChunkedTransferResult::SizeLimitExceeded => {
                // サイズ制限超過 - 接続を閉じて失敗として扱う
                return None;
            }
        }
    } else {
        let remaining = content_length.saturating_sub(initial_body.len());
        if remaining > 0 {
            let transferred = transfer_exact_bytes(client_stream, backend_stream, remaining).await;
            if transferred < remaining as u64 {
                return None;
            }
        }
    }

    // 4. レスポンスを受信して転送（Connectionヘッダーも取得）
    let (total, status_code, backend_wants_keep_alive) = transfer_response_with_keepalive(backend_stream, client_stream).await;

    Some((status_code, total, backend_wants_keep_alive))
}

// ====================
// 転送ヘルパー関数（ジェネリック版）
// ====================

/// 正確なバイト数を転送
async fn transfer_exact_bytes<R: AsyncReader, W: AsyncWriter>(
    reader: &mut R,
    writer: &mut W,
    mut remaining: usize,
) -> u64 {
    let mut total = 0u64;
    
    while remaining > 0 {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, reader.read_buf(buf)).await;
        
        let (res, returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return total,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        let mut write_buf = returned_buf;
        write_buf.truncate(n);
        
        let write_result = timeout(WRITE_TIMEOUT, writer.write_buf(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put(returned);
            }
            Ok((Err(_), returned)) => {
                buf_put(returned);
                break;
            }
            Err(_) => break,
        }
        
        total += n as u64;
        remaining -= n;
    }
    
    total
}

// ====================
// kTLS + splice(2) によるゼロコピー転送（Linux 固有）
// ====================
//
// kTLS が有効な場合、splice(2) を使用してカーネル空間で直接
// データを転送します。これにより、ボディ転送時にユーザー空間への
// コピーが完全に不要になります。
//
// ## 実装状況
//
// - **ファイル送信（sendfile）**: kTLS有効時にゼロコピー対応 ✅
// - **プロキシ転送（splice）**: kTLS有効時 + Content-Length で対応 ✅
//
// ## プロキシ転送でのsplice使用
//
// libc::read/write を直接使用し、monoio の所有権ベース I/O を回避。
// 非同期待機は TcpStream::readable()/writable() を使用。
//
// ### 対応状況
// - Content-Length 転送: splice(2) でゼロコピー ✅
// - Chunked 転送: 通常転送（終端検出が必要なため）
//
// ## splice(2) の転送フロー
//
// [リクエスト] クライアント(kTLS) → splice → バックエンド(TCP)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ: splice(2) でゼロコピー転送
//
// [レスポンス] バックエンド(TCP) → splice → クライアント(kTLS)
//   1. ヘッダー: raw_read で読み取り → パース → raw_write で送信
//   2. ボディ: splice(2) でゼロコピー転送
//
// 注意: splice(2) は少なくとも一方のFDがパイプである必要があります。
// ====================

/// Chunkedボディ転送の結果
#[derive(Debug, Clone, Copy, PartialEq)]
enum ChunkedTransferResult {
    /// 転送完了
    Complete,
    /// 転送失敗（I/Oエラー等）
    Failed,
    /// サイズ制限超過（DoS対策）
    SizeLimitExceeded,
}

/// Chunkedボディを転送（ステートマシンベース）
/// 
/// RFC 7230準拠のChunkedDecoderを使用して、トレーラーの有無に
/// かかわらず正確に終端を検出します。
/// 
/// DoS対策として、max_body_size を超えた場合は転送を中止します。
/// 
/// # Arguments
/// * `reader` - 読み取り元ストリーム
/// * `writer` - 書き込み先ストリーム
/// * `initial_body` - 初期ボディデータ（ヘッダー後に既に読み取り済みのデータ）
/// * `max_body_size` - 最大許容ボディサイズ（0の場合は制限なし）
async fn transfer_chunked_body<R: AsyncReader, W: AsyncWriter>(
    reader: &mut R,
    writer: &mut W,
    initial_body: &[u8],
    max_body_size: u64,
) -> ChunkedTransferResult {
    let mut decoder = ChunkedDecoder::new(max_body_size);
    
    // 初期ボディが既に終端を含んでいるかチェック
    if !initial_body.is_empty() {
        match decoder.feed(initial_body) {
            ChunkedFeedResult::Complete => return ChunkedTransferResult::Complete,
            ChunkedFeedResult::SizeLimitExceeded => return ChunkedTransferResult::SizeLimitExceeded,
            ChunkedFeedResult::Continue => {}
        }
    }
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, reader.read_buf(buf)).await;
        
        let (res, returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => return ChunkedTransferResult::Failed,
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                return ChunkedTransferResult::Failed;
            }
        };
        
        // ステートマシンにデータをフィード
        let feed_result = decoder.feed(&returned_buf[..n]);
        
        // サイズ制限超過チェック
        if feed_result == ChunkedFeedResult::SizeLimitExceeded {
            buf_put(returned_buf);
            return ChunkedTransferResult::SizeLimitExceeded;
        }
        
        // バックエンドに転送
        let mut write_buf = returned_buf;
        write_buf.truncate(n);
        
        let write_result = timeout(WRITE_TIMEOUT, writer.write_buf(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put(returned);
            }
            Ok((Err(_), returned)) => {
                buf_put(returned);
                return ChunkedTransferResult::Failed;
            }
            Err(_) => return ChunkedTransferResult::Failed,
        }
        
        // 終端チェック
        if feed_result == ChunkedFeedResult::Complete {
            return ChunkedTransferResult::Complete;
        }
    }
    
    ChunkedTransferResult::Failed
}

/// レスポンスを受信して転送（ジェネリック版）
/// 注: 現在は transfer_response_with_keepalive を使用
#[allow(dead_code)]
async fn transfer_response<R: AsyncReader, W: AsyncWriter>(
    backend: &mut R,
    client: &mut W,
) -> (u64, u16) {
    let mut total = 0u64;
    let mut status_code = 502u16;
    let mut header_parsed = false;
    let mut accumulated = Vec::with_capacity(4096);
    let mut is_chunked = false;
    let mut body_remaining: Option<usize> = None;
    // ステートマシンベースのChunkedデコーダを使用
    // レスポンス受信時は制限なし（バックエンドを信頼）
    let mut chunked_decoder = ChunkedDecoder::new_unlimited();
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend.read_buf(buf)).await;
        
        let (res, returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                // タイムアウト
                if !accumulated.is_empty() {
                    let data = std::mem::take(&mut accumulated);
                    let len = data.len();
                    let _ = timeout(WRITE_TIMEOUT, client.write_buf(data)).await;
                    total += len as u64;
                }
                break;
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                // EOFに達した
                if !accumulated.is_empty() {
                    let data = std::mem::take(&mut accumulated);
                    let len = data.len();
                    let _ = timeout(WRITE_TIMEOUT, client.write_buf(data)).await;
                    total += len as u64;
                }
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        if !header_parsed {
            accumulated.extend_from_slice(&returned_buf[..n]);
            buf_put(returned_buf);
            
            // httparseを使用してレスポンスヘッダーを解析
            if let Some(parsed) = parse_http_response(&accumulated) {
                header_parsed = true;
                status_code = parsed.status_code;
                is_chunked = parsed.is_chunked;
                
                let header_len = parsed.header_len;
                let header_with_body = std::mem::take(&mut accumulated);
                let data_len = header_with_body.len();
                
                // ボディ開始部分の長さを計算
                let body_start_len = data_len.saturating_sub(header_len);
                
                // Content-Lengthがある場合、残りのボディサイズを計算
                if let Some(cl) = parsed.content_length {
                    body_remaining = Some(cl.saturating_sub(body_start_len));
                }
                
                // Chunked の場合、初期ボディ部分をデコーダにフィード
                if is_chunked && body_start_len > 0 {
                    let _ = chunked_decoder.feed(&header_with_body[header_len..]);
                }
                
                let write_result = timeout(WRITE_TIMEOUT, client.write_buf(header_with_body)).await;
                match write_result {
                    Ok((Ok(_), returned)) => {
                        buf_put(returned);
                    }
                    Ok((Err(_), returned)) => {
                        buf_put(returned);
                        break;
                    }
                    Err(_) => break,
                }
                total += data_len as u64;
            }
        } else {
            // ヘッダー解析済み
            if is_chunked {
                // Chunked転送 - デコーダにデータをフィード
                let feed_result = chunked_decoder.feed(&returned_buf[..n]);
                
                let mut write_buf = returned_buf;
                write_buf.truncate(n);
                
                let write_result = timeout(WRITE_TIMEOUT, client.write_buf(write_buf)).await;
                match write_result {
                    Ok((Ok(_), returned)) => {
                        buf_put(returned);
                    }
                    Ok((Err(_), returned)) => {
                        buf_put(returned);
                        break;
                    }
                    Err(_) => break,
                }
                
                total += n as u64;
                
                // ステートマシンによる終端チェック
                if feed_result == ChunkedFeedResult::Complete {
                    break;
                }
            } else {
                // Content-Length転送
                let bytes_to_send = if let Some(remaining) = body_remaining {
                    let to_send = n.min(remaining);
                    body_remaining = Some(remaining - to_send);
                    to_send
                } else {
                    n
                };
                
                if bytes_to_send > 0 {
                    let mut write_buf = returned_buf;
                    write_buf.truncate(bytes_to_send);
                    
                    let write_result = timeout(WRITE_TIMEOUT, client.write_buf(write_buf)).await;
                    match write_result {
                        Ok((Ok(_), returned)) => {
                            buf_put(returned);
                        }
                        Ok((Err(_), returned)) => {
                            buf_put(returned);
                            break;
                        }
                        Err(_) => break,
                    }
                    
                    total += bytes_to_send as u64;
                } else {
                    buf_put(returned_buf);
                }
                
                if let Some(remaining) = body_remaining {
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
    }
    
    (total, status_code)
}

/// レスポンスを受信して転送（Keep-Aliveサポート版）
/// バックエンドがKeep-Aliveを許可しているかどうかも返す
async fn transfer_response_with_keepalive<R: AsyncReader, W: AsyncWriter>(
    backend: &mut R,
    client: &mut W,
) -> (u64, u16, bool) {
    let mut total = 0u64;
    let mut status_code = 502u16;
    let mut header_parsed = false;
    let mut accumulated = Vec::with_capacity(4096);
    let mut is_chunked = false;
    let mut body_remaining: Option<usize> = None;
    // ステートマシンベースのChunkedデコーダを使用
    // レスポンス受信時は制限なし（バックエンドを信頼）
    let mut chunked_decoder = ChunkedDecoder::new_unlimited();
    let mut backend_wants_keep_alive = false;  // デフォルトはfalse（安全側）
    
    loop {
        let buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend.read_buf(buf)).await;
        
        let (res, returned_buf) = match read_result {
            Ok(result) => result,
            Err(_) => {
                // タイムアウト
                if !accumulated.is_empty() {
                    let data = std::mem::take(&mut accumulated);
                    let len = data.len();
                    let _ = timeout(WRITE_TIMEOUT, client.write_buf(data)).await;
                    total += len as u64;
                }
                break;
            }
        };
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                // EOFに達した
                if !accumulated.is_empty() {
                    let data = std::mem::take(&mut accumulated);
                    let len = data.len();
                    let _ = timeout(WRITE_TIMEOUT, client.write_buf(data)).await;
                    total += len as u64;
                }
                break;
            }
            Ok(n) => n,
            Err(_) => {
                buf_put(returned_buf);
                break;
            }
        };
        
        if !header_parsed {
            accumulated.extend_from_slice(&returned_buf[..n]);
            buf_put(returned_buf);
            
            // httparseを使用してレスポンスヘッダーを解析
            if let Some(parsed) = parse_http_response(&accumulated) {
                header_parsed = true;
                status_code = parsed.status_code;
                is_chunked = parsed.is_chunked;
                
                // HTTP/1.1 ではデフォルトでkeep-alive、Connection: closeが明示されていなければOK
                backend_wants_keep_alive = !parsed.is_connection_close;
                
                let header_len = parsed.header_len;
                let header_with_body = std::mem::take(&mut accumulated);
                let data_len = header_with_body.len();
                
                // ボディ開始部分の長さを計算
                let body_start_len = data_len.saturating_sub(header_len);
                
                // Content-Lengthがある場合、残りのボディサイズを計算
                if let Some(cl) = parsed.content_length {
                    body_remaining = Some(cl.saturating_sub(body_start_len));
                }
                
                // Chunked の場合、初期ボディ部分をデコーダにフィード
                if is_chunked && body_start_len > 0 {
                    let _ = chunked_decoder.feed(&header_with_body[header_len..]);
                }
                
                let write_result = timeout(WRITE_TIMEOUT, client.write_buf(header_with_body)).await;
                match write_result {
                    Ok((Ok(_), returned)) => {
                        buf_put(returned);
                    }
                    Ok((Err(_), returned)) => {
                        buf_put(returned);
                        backend_wants_keep_alive = false;
                        break;
                    }
                    Err(_) => {
                        backend_wants_keep_alive = false;
                        break;
                    }
                }
                total += data_len as u64;
            }
        } else {
            // ヘッダー解析済み
            if is_chunked {
                // Chunked転送 - デコーダにデータをフィード
                let feed_result = chunked_decoder.feed(&returned_buf[..n]);
                
                let mut write_buf = returned_buf;
                write_buf.truncate(n);
                
                let write_result = timeout(WRITE_TIMEOUT, client.write_buf(write_buf)).await;
                match write_result {
                    Ok((Ok(_), returned)) => {
                        buf_put(returned);
                    }
                    Ok((Err(_), returned)) => {
                        buf_put(returned);
                        backend_wants_keep_alive = false;
                        break;
                    }
                    Err(_) => {
                        backend_wants_keep_alive = false;
                        break;
                    }
                }
                
                total += n as u64;
                
                // ステートマシンによる終端チェック
                if feed_result == ChunkedFeedResult::Complete {
                    break;
                }
            } else {
                // Content-Length転送
                let bytes_to_send = if let Some(remaining) = body_remaining {
                    let to_send = n.min(remaining);
                    body_remaining = Some(remaining - to_send);
                    to_send
                } else {
                    n
                };
                
                if bytes_to_send > 0 {
                    let mut write_buf = returned_buf;
                    write_buf.truncate(bytes_to_send);
                    
                    let write_result = timeout(WRITE_TIMEOUT, client.write_buf(write_buf)).await;
                    match write_result {
                        Ok((Ok(_), returned)) => {
                            buf_put(returned);
                        }
                        Ok((Err(_), returned)) => {
                            buf_put(returned);
                            backend_wants_keep_alive = false;
                            break;
                        }
                        Err(_) => {
                            backend_wants_keep_alive = false;
                            break;
                        }
                    }
                    
                    total += bytes_to_send as u64;
                } else {
                    buf_put(returned_buf);
                }
                
                if let Some(remaining) = body_remaining {
                    if remaining == 0 {
                        break;
                    }
                }
            }
        }
    }
    
    (total, status_code, backend_wants_keep_alive)
}

// ====================
// SendFile処理
// ====================
//
// kTLS + sendfile によるゼロコピー送信をサポートします。
//
// ## 通常の送信フロー（kTLS無効時）
//
// ファイル → ユーザー空間バッファ → TLS暗号化 → ネットワーク
// （2回のコピーが発生）
//
// ## ゼロコピー送信フロー（kTLS有効時）
//
// ファイル → カーネル空間でTLS暗号化 → NIC
// （ユーザー空間へのコピーなし）
//
// ### パフォーマンス効果
//
// - コンテキストスイッチの削減
// - メモリアクセスの削減（L3キャッシュミスの減少）
// - CPU使用率の低下（特に大きなファイル送信時）
//
// ### セキュリティ
//
// - ファイルの内容は変更されず、そのまま送信される
// - TLS暗号化はカーネル内で行われるため安全
// ====================

async fn handle_sendfile(
    mut tls_stream: ServerTls,
    base_path: &Path,
    is_dir: bool,
    index_filename: Option<&str>,
    req_path: &[u8],
    prefix: &[u8],
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    // --- パス解決ロジック（Nginx風） ---
    // 
    // 1. ファイル指定（is_dir=false）: 完全一致のみ
    //    例: prefix="/robots.txt", path="./www/robots.txt"
    //    - リクエスト "/robots.txt" → OK（ファイルを返す）
    //    - リクエスト "/robots.txt/extra" → 404（ファイルの下には入れない）
    //
    // 2. ディレクトリ指定（is_dir=true）: プレフィックス除去後のパスを結合
    //    例: prefix="/static/", path="./www/assets/"
    //    - リクエスト "/static/css/style.css" → "./www/assets/css/style.css"
    //    - リクエスト "/static/" → "./www/assets/{index_filename}" (デフォルト: index.html)
    
    let path_str = std::str::from_utf8(req_path).unwrap_or("/");
    let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
    
    // プレフィックスを除去して「残りパス」を取得
    let remainder = if !prefix_str.is_empty() && path_str.starts_with(prefix_str) {
        &path_str[prefix_str.len()..]
    } else {
        path_str
    };
    
    // パストラバーサル防止（簡易チェック）
    if remainder.contains("..") {
        let err_buf = ERR_MSG_FORBIDDEN.to_vec();
        let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
        return Some((tls_stream, 403, 0, true));
    }
    
    let full_path = if is_dir {
        // ケースA: ディレクトリへのルーティング（Alias動作）
        // config: path = "./www/static/"
        // req: /static/css/style.css → remainder: css/style.css
        // result: ./www/static/css/style.css
        let sub_path = remainder.trim_start_matches('/');
        let mut p = base_path.to_path_buf();
        if !sub_path.is_empty() {
            p.push(sub_path);
        }
        p
    } else {
        // ケースB: ファイルへの直接ルーティング（完全一致）
        // config: path = "./www/robots.txt"
        // req: /robots.txt → remainder: "" (OK)
        // req: /robots.txt/extra → remainder: "/extra" (NG → 404)
        
        let clean_remainder = remainder.trim_matches('/');
        if !clean_remainder.is_empty() {
            // ファイル指定なのにさらにパスが続いている場合は404
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));
        }
        base_path.to_path_buf()
    };

    let full_path_canonical = match full_path.canonicalize() {
        Ok(p) => p,
        Err(_) => {
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));  // エラー時は接続を閉じる
        }
    };

    if is_dir {
        if let Ok(base) = base_path.canonicalize() {
            if !full_path_canonical.starts_with(&base) {
                let err_buf = ERR_MSG_FORBIDDEN.to_vec();
                let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
                return Some((tls_stream, 403, 0, true));  // エラー時は接続を閉じる
            }
        }
    }

    // ディレクトリの場合はインデックスファイルを試す
    // 設定されたファイル名、なければデフォルトの "index.html" を使用
    let final_path = if full_path_canonical.is_dir() {
        let filename = index_filename.unwrap_or("index.html");
        let index_path = full_path_canonical.join(filename);
        if index_path.exists() {
            index_path
        } else {
            // インデックスファイルが存在しない場合は403 Forbidden（ディレクトリリスティング禁止）
            let err_buf = ERR_MSG_FORBIDDEN.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 403, 0, true));
        }
    } else {
        full_path_canonical
    };

    let file = match OpenOptions::new().read(true).open(&final_path).await {
        Ok(f) => f,
        Err(_) => {
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));  // エラー時は接続を閉じる
        }
    };

    let metadata = match file.metadata().await {
        Ok(m) => m,
        Err(_) => {
            let err_buf = ERR_MSG_NOT_FOUND.to_vec();
            let _ = timeout(WRITE_TIMEOUT, tls_stream.write_all(err_buf)).await;
            return Some((tls_stream, 404, 0, true));  // エラー時は接続を閉じる
        }
    };

    let file_size = metadata.len();
    let mime_type = mime_guess::from_path(&final_path).first_or_octet_stream();
    
    // ヘッダー構築（Keep-Alive対応）
    let mut header_buf = Vec::with_capacity(HEADER_BUF_CAPACITY);
    header_buf.extend_from_slice(HTTP_200_PREFIX);
    header_buf.extend_from_slice(mime_type.as_ref().as_bytes());
    header_buf.extend_from_slice(CONTENT_LENGTH_HEADER);
    
    let mut num_buf = itoa::Buffer::new();
    header_buf.extend_from_slice(num_buf.format(file_size).as_bytes());
    if client_wants_close {
        header_buf.extend_from_slice(CONNECTION_CLOSE);
    } else {
        header_buf.extend_from_slice(CONNECTION_KEEP_ALIVE);
    }

    // ヘッダー送信（タイムアウト付き）
    let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(header_buf)).await;
    if !matches!(write_result, Ok((Ok(_), _))) {
        return None;
    }

    // ファイル転送
    // kTLS が有効な場合は sendfile によるゼロコピー送信を使用
    #[cfg(feature = "ktls")]
    {
        if tls_stream.is_ktls_send_enabled() {
            return handle_sendfile_zerocopy(tls_stream, &file, file_size, client_wants_close).await;
        }
    }
    
    // kTLS が無効な場合は従来の read/write を使用
    handle_sendfile_userspace(tls_stream, &file, file_size, client_wants_close).await
}

/// kTLS + sendfile によるゼロコピーファイル送信
///
/// kTLS が有効な場合に使用されます。
/// ファイルの内容をカーネル空間で直接 TLS 暗号化して送信します。
#[cfg(feature = "ktls")]
async fn handle_sendfile_zerocopy(
    tls_stream: ServerTls,
    file: &monoio::fs::File,
    file_size: u64,
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    use std::os::unix::io::AsRawFd;
    
    let file_fd = file.as_raw_fd();
    let mut offset: i64 = 0;
    let mut total_sent = 0u64;
    
    // sendfile を使用してファイルをゼロコピー送信
    // sendfile はブロッキング呼び出しのため、大きなファイルはチャンク分割して送信
    const SENDFILE_CHUNK_SIZE: usize = 1024 * 1024; // 1MB チャンク
    
    while (offset as u64) < file_size {
        let remaining = file_size - (offset as u64);
        let chunk_size = (remaining as usize).min(SENDFILE_CHUNK_SIZE);
        
        // sendfile 実行
        match tls_stream.sendfile(file_fd, &mut offset, chunk_size) {
            Ok(0) => {
                // EOF
                break;
            }
            Ok(n) => {
                total_sent += n as u64;
            }
            Err(e) => {
                // EAGAIN/EWOULDBLOCK の場合は再試行（非同期ソケットの場合）
                if e.kind() == io::ErrorKind::WouldBlock {
                    // writable を待ってから再試行
                    if let Err(_) = tls_stream.get_ref().writable(false).await {
                        break;
                    }
                    continue;
                }
                error!("sendfile error: {}", e);
                break;
            }
        }
    }

    Some((tls_stream, 200, total_sent, client_wants_close))
}

/// 従来の read/write によるファイル送信（ユーザー空間経由）
///
/// kTLS が無効な場合、または rustls 使用時に使用されます。
async fn handle_sendfile_userspace(
    mut tls_stream: ServerTls,
    file: &monoio::fs::File,
    file_size: u64,
    client_wants_close: bool,
) -> Option<(ServerTls, u16, u64, bool)> {
    let mut total_sent = 0u64;
    let mut offset = 0u64;
    
    while offset < file_size {
        let read_buf = buf_get();
        let (res, returned_buf) = file.read_at(read_buf, offset).await;
        
        let n = match res {
            Ok(0) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n,
            Err(e) => {
                buf_put(returned_buf);
                error!("File read error: {}", e);
                break;
            }
        };
        
        let mut write_buf = returned_buf;
        write_buf.truncate(n);
        
        let write_result = timeout(WRITE_TIMEOUT, tls_stream.write_all(write_buf)).await;
        match write_result {
            Ok((Ok(_), returned)) => {
                buf_put(returned);
                total_sent += n as u64;
                offset += n as u64;
            }
            Ok((Err(_), returned)) => {
                buf_put(returned);
                break;
            }
            Err(_) => break,
        }
    }

    Some((tls_stream, 200, total_sent, client_wants_close))
}

// ====================
// ロギング
// ====================

fn log_access(path: &[u8], ua: &[u8], req_body_size: u64, status: u16, resp_body_size: u64, start_time: OffsetDateTime) {
    let end_time = OffsetDateTime::now_utc();
    let duration_ms = (end_time - start_time).whole_milliseconds();
    let path_str = std::str::from_utf8(path).unwrap_or("-");
    let ua_str = std::str::from_utf8(ua).unwrap_or("-");
    
    info!("Access: time={} duration={}ms path={} ua={} req_body_size={} status={} resp_body_size={}",
        start_time, duration_ms, path_str, ua_str, req_body_size, status, resp_body_size);
}

#[allow(dead_code)]
fn find_header<'a>(headers: &'a [Header<'a>], name: &str) -> Option<&'a [u8]> {
    headers.iter().find(|h| h.name.eq_ignore_ascii_case(name)).map(|h| h.value)
}
