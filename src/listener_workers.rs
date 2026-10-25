//! 待ち受け（リスナー）1 個のワーカー群を起動する（F-260）
//!
//! `entry.rs` は正規化したリスナーの列（`config::ListenerSpec`）を順に渡すだけにし、TCP のリスナー（TLS・平文）と
//! HTTP/3 のリスナーのワーカーの起動はここにまとめる。リスナーごとに `threads` 本のスレッドを持つ作り（今までの
//! `[server] listen`・`listen_cleartext` と同じ）で、暗黙のリスナー 2 個の設定は今までと同じスレッド数・同じ経路になる。

use crate::metrics::*;
use crate::system::*;

use crate::config::{
    drain_connections, ListenerSpec, ReuseportBalancing, CURRENT_CONNECTIONS, SHUTDOWN_FLAG,
};
use crate::runtime::tcp::{TcpListener, TcpStream};
use crate::runtime::time::timeout;
use crate::server::create_listener;
use ftlog::{error, info, warn};
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

/// TLS のアクセプタ（kTLS の有無でビルドごとに 1 つ）
#[cfg(veil_ktls)]
pub(crate) type TlsAcceptor = crate::ktls_rustls::RustlsAcceptor;
/// TLS のアクセプタ（kTLS の有無でビルドごとに 1 つ）
#[cfg(not(veil_ktls))]
pub(crate) type TlsAcceptor = crate::simple_tls::SimpleTlsAcceptor;

/// I/O ワーカースレッド（per-core io_uring executor）を 8MB スタックで起動する。
///
/// `std::thread::spawn` の既定スタックは 2MB と小さく、接続ハンドラの async future は
/// 深いネスト（TLS read/write → HTTP/2 フレーム処理 → ストリーミング転送 → 応答リレー）で
/// 大きくなり得る。spawn 時に future はスタック上で構築されてからボックス化されるため、
/// 既定 2MB では大きな接続でスタックオーバーフローし得る。十分なスタックを確保して防ぐ。
fn spawn_worker_thread<F>(f: F) -> thread::JoinHandle<()>
where
    F: FnOnce() + Send + 'static,
{
    thread::Builder::new()
        .stack_size(8 * 1024 * 1024)
        .spawn(f)
        .expect("failed to spawn I/O worker thread")
}

// ====================
// F-164: TCP / UDS 共通のワーカーリスナー取得
// ====================

/// リスナーのワーカーがそれぞれ自分専用の `runtime::TcpListener` を得る手段（F-164）。
///
/// - TCP: 各ワーカーが `SO_REUSEPORT` で個別に bind する（従来どおり）。
/// - UDS（`cfg(unix)` のみ）: AF_UNIX に `SO_REUSEPORT` は無いため、ワーカー spawn 前に
///   `server::bind_unix_listener` で 1 度だけ bind し、各ワーカーはその fd を
///   `dup(2)` して使う（カーネルが accept を分散する）。
#[derive(Clone)]
enum WorkerListenerSource {
    Tcp(SocketAddr),
    #[cfg(unix)]
    Unix(Arc<crate::server::OwnedListenerFd>),
}

impl WorkerListenerSource {
    /// `ListenAddr` から、ワーカー spawn 前の準備（UDS の場合は bind）まで済ませて作る。
    /// `unix_socket_permissions` はリスナーの UDS のパーミッション（8 進数表記の文字列）。
    fn prepare(
        addr: &crate::listen_addr::ListenAddr,
        #[cfg_attr(not(unix), allow(unused_variables))] unix_socket_permissions: &str,
    ) -> std::io::Result<Self> {
        match addr {
            crate::listen_addr::ListenAddr::Tcp(a) => Ok(Self::Tcp(*a)),
            #[cfg(unix)]
            crate::listen_addr::ListenAddr::Unix(path) => {
                let mode = crate::config::parse_unix_socket_permissions(unix_socket_permissions)
                    .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
                let fd = crate::server::bind_unix_listener(path, mode)?;
                Ok(Self::Unix(Arc::new(fd)))
            }
        }
    }

    /// UDS の場合、bind したソケットファイルのパスを返す（グレースフルシャットダウン時の
    /// unlink 用）。
    #[cfg(unix)]
    fn unix_socket_path(&self) -> Option<&std::path::Path> {
        match self {
            Self::Tcp(_) => None,
            Self::Unix(fd) => Some(fd.path()),
        }
    }

    /// このワーカー専用の `runtime::TcpListener` を取得する。
    fn obtain(
        &self,
        balancing: ReuseportBalancing,
        workers: usize,
        thread_id: usize,
    ) -> std::io::Result<TcpListener> {
        match self {
            Self::Tcp(addr) => create_listener(*addr, balancing, workers, thread_id),
            #[cfg(unix)]
            Self::Unix(fd) => fd.dup_listener(),
        }
    }
}

/// 全リスナーのワーカーで共有する起動時の値
#[derive(Clone)]
pub(crate) struct WorkerEnv {
    /// CPU アフィニティのコア（リスナーごとに先頭のコアから割り当てる。今までと同じ）
    pub core_ids: Option<Vec<core_affinity::CoreId>>,
    pub balancing: ReuseportBalancing,
    /// 同時接続数の上限（プロセス全体。0 = 無制限）
    pub max_conn: usize,
    /// capability mode・pledge・sandbox_init のバリア（bind を終えたワーカーの数。B-163）
    #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
    pub listeners_ready: Arc<std::sync::atomic::AtomicUsize>,
}

impl WorkerEnv {
    fn core_for(&self, thread_id: usize) -> Option<core_affinity::CoreId> {
        // コア数よりスレッド数が多い場合はモジュロ演算でラップアラウンド
        self.core_ids
            .as_ref()
            .filter(|ids| !ids.is_empty())
            .map(|ids| ids[thread_id % ids.len()])
    }
}

/// スレッド開始直後にCPUアフィニティを設定する。
/// これによりL1/L2キャッシュミスを削減し、レイテンシのジッターを安定化
fn pin_to_core(label: &str, thread_id: usize, core: Option<core_affinity::CoreId>) {
    if let Some(core_id) = core {
        if core_affinity::set_for_current(core_id) {
            info!("[{} {}] Pinned to CPU core {:?}", label, thread_id, core_id);
        } else {
            warn!(
                "[{} {}] Failed to pin to CPU core {:?}, running unpinned",
                label, thread_id, core_id
            );
        }
    }
}

/// TCP のリスナー 1 個（TLS か平文）の `workers` 本のワーカーを起動する。
///
/// `id` はリスナーの id（`RuntimeConfig::cleartext` の添字）。`acceptor` は TLS のリスナーのとき
/// （そのリスナーの ALPN のプロファイルのもの）。UDS を bind したらそのパスを返す（終了時に消す）。
pub(crate) fn spawn_tcp_listener(
    id: usize,
    spec: &ListenerSpec,
    workers: usize,
    acceptor: Option<TlsAcceptor>,
    env: &WorkerEnv,
    handles: &mut Vec<thread::JoinHandle<()>>,
) -> std::io::Result<Option<std::path::PathBuf>> {
    // F-164: UDS の場合はワーカー spawn 前に 1 度だけ bind する
    // （AF_UNIX には SO_REUSEPORT が無いため）。
    let source = WorkerListenerSource::prepare(&spec.addr, &spec.unix_socket_permissions)?;
    #[cfg(unix)]
    let unix_path = source.unix_socket_path().map(|p| p.to_path_buf());
    #[cfg(not(unix))]
    let unix_path = None;

    info!("{}", spec.describe(workers));
    let label: Arc<str> = Arc::from(spec.name.as_str());
    match acceptor {
        Some(acceptor) => spawn_workers(
            &label,
            workers,
            &source,
            env,
            handles,
            move |stream: TcpStream, peer_addr: SocketAddr| {
                // handle_connection 内で CURRENT_CONFIG から最新の設定を取得
                // これによりホットリロード時に新しい設定が即座に反映される
                crate::proxy::handle_connection(stream, acceptor.clone(), peer_addr)
            },
        ),
        None => {
            #[cfg(feature = "http2")]
            spawn_workers(
                &label,
                workers,
                &source,
                env,
                handles,
                move |stream: TcpStream, peer_addr: SocketAddr| {
                    serve_cleartext(stream, peer_addr, id)
                },
            );
            // 平文のリスナーは http2 feature が要る（設定の検証で止めている）
            #[cfg(not(feature = "http2"))]
            {
                let _ = id;
                return Err(std::io::Error::other(
                    "plaintext listeners need a build with the http2 feature",
                ));
            }
        }
    }
    Ok(unix_path)
}

/// `workers` 本のワーカーを起動する。`serve` は受理した 1 接続を処理する Future を作る
/// （リスナーの種類ごとに単相化されるので、TLS と平文の経路はそれぞれ今までと同じ型の Future になる）。
fn spawn_workers<S, Fut>(
    label: &Arc<str>,
    workers: usize,
    source: &WorkerListenerSource,
    env: &WorkerEnv,
    handles: &mut Vec<thread::JoinHandle<()>>,
    serve: S,
) where
    S: Fn(TcpStream, SocketAddr) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + 'static,
{
    for thread_id in 0..workers {
        let source = source.clone();
        let serve = serve.clone();
        let label = label.clone();
        let balancing = env.balancing;
        let max_conn = env.max_conn;
        let assigned_core = env.core_for(thread_id);
        #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
        let listeners_ready = Arc::clone(&env.listeners_ready);

        let handle = spawn_worker_thread(move || {
            pin_to_core(&label, thread_id, assigned_core);

            crate::runtime::block_on(async move {
                let listener = match source.obtain(balancing, workers, thread_id) {
                    Ok(l) => l,
                    Err(e) => {
                        error!("[{} {}] Bind error: {}", label, thread_id, e);
                        return;
                    }
                };
                // capability mode・pledge・sandbox_init のバリア: bind 完了を通知（コールドパス。B-163）
                #[cfg(any(target_os = "freebsd", target_os = "openbsd", target_os = "macos"))]
                listeners_ready.fetch_add(1, std::sync::atomic::Ordering::Release);

                info!("[{} {}] Worker started", label, thread_id);

                // F-46: 接続ハンドラの型付きタスクプール（spawn ごとの Box 確保を排除）
                let conn_pool = crate::runtime::TaskPool::new();

                loop {
                    // Shutdown チェック
                    if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        info!("[{} {}] Shutting down...", label, thread_id);
                        break;
                    }

                    // タイムアウト付きaccept（Graceful Shutdown対応）
                    let accept_result = timeout(Duration::from_secs(1), listener.accept()).await;

                    let (stream, peer_addr) = match accept_result {
                        Ok(Ok(s)) => s,
                        Ok(Err(e)) => {
                            error!("[{} {}] Accept error: {}", label, thread_id, e);
                            continue;
                        }
                        Err(_) => {
                            // タイムアウト - ループを継続してshutdownチェック。
                            // F-183: アイドル点でアロケータに空き領域を OS へ返させる。
                            crate::alloc_tune::idle_tick();
                            continue;
                        }
                    };

                    // F-155: 1 周回で受理した接続数のローカルカウンタ。
                    // バッチ内で spawn したタスクはまだ `ConnectionGuard::new()` を
                    // 実行していない可能性があるため（spawn 直後はまだ poll されて
                    // いない）、`CURRENT_CONNECTIONS` のロード値だけでは同一バッチ内で
                    // 受理済みの分を勘定できない。`CURRENT_CONNECTIONS.load(..) +
                    // batch_accepted` で判定することで、バッチ受理中に同時接続数
                    // 上限を一時的に超えるのを防ぐ。
                    let mut batch_accepted: usize = 0;

                    // 1 件受理した接続の検証・処理ロジック（1 件目・バッチ内の残りで
                    // 共通のため、ローカルクロージャへまとめる）。
                    let mut handle_accepted = |stream: TcpStream, peer_addr: SocketAddr| {
                        // F-35: 最前線 IP ブロックリスト。ブロック対象 IP は TLS
                        // ハンドシェイク・プロトコル検出の前に切断する（stream は drop で閉じられる）。
                        if crate::config::is_ip_blocked(peer_addr.ip()) {
                            return;
                        }

                        // 同時接続数制限チェック（上の doc コメント参照）。
                        if max_conn > 0 {
                            let current =
                                CURRENT_CONNECTIONS.load(Ordering::Relaxed) + batch_accepted;
                            if current >= max_conn {
                                warn!(
                                    "[{} {}] Connection limit reached ({}/{}), rejecting connection from {}",
                                    label, thread_id, current, max_conn, peer_addr
                                );
                                drop(stream);
                                return;
                            }
                        }

                        let _ = stream.set_nodelay(true);
                        batch_accepted += 1;

                        let fut = serve(stream, peer_addr);
                        // パニックキャッチ + 型付きプール（F-46）でスレッド生存とゼロ確保を両立
                        spawn_pooled_with_panic_catch(&conn_pool, async move {
                            // ConnectionGuard がスコープ内で生存している間、接続がカウントされる
                            // パニック時も Drop が呼ばれるため、カウンターの整合性が保証される
                            let _guard = ConnectionGuard::new();
                            fut.await;
                        });
                    };

                    handle_accepted(stream, peer_addr);

                    // F-155/F-156: nginx の `multi_accept` 相当。1 件目に続けて、
                    // バックログに滞留している接続を合計最大 32 件（1 件目 + 31 件）まで
                    // 一気に受理・spawn する。io_uring バックエンド（`veil_rt_uring`）の
                    // `TcpListener` には `accept_batch` が存在しない（io_uring パスの
                    // ロジックは変更しない方針のため）ので、reactor バックエンド
                    // （`veil_rt_reactor`: epoll/kqueue）限定で有効にする。
                    //
                    // **効果は FreeBSD の既存ハーネスでは測定できない**（平文の h2c で測定を試みた記録）:
                    // `h2c_file_plain` は h2load が keep-alive で 64 接続を張りっぱなしにするため、
                    // accept は計測開始時の 64 回しか実行されず、15 秒間のスループットにはほぼ寄与しない。
                    // 有無を変えた交互 A/B を 2 回（計 8 ラウンド）取ったところ、結果は逆転し、
                    // 8 サンプルの平均はほぼ同一だった（＝差は測定ノイズ）。本最適化が効くのは
                    // 短命接続が支配的なワークロードで、評価するにはそれ用の負荷（接続確立レート）が要る。
                    #[cfg(veil_rt_reactor)]
                    {
                        if let Err(e) = listener.accept_batch(31, &mut handle_accepted) {
                            error!("[{} {}] Accept error: {}", label, thread_id, e);
                        }
                    }
                }

                // グレースフルシャットダウン: 既存接続の完了を待機
                drain_connections(&label, thread_id).await;

                info!("[{} {}] Worker stopped", label, thread_id);
            });
            // B-118: 待ちが切れて残った接続タスクをスレッドローカルが生きているうちに破棄する
            crate::runtime::shutdown_thread();
        });
        handles.push(handle);
    }
}

/// 平文のリスナーで受けた 1 接続（F-200）。HTTP/1.1 と h2c をプロトコル検出で判別し、
/// そのリスナーの方針（`allow_http`・`allow_h2c`・`secure_redirect`。接続ごとに CURRENT_CONFIG を
/// リスナーの id で引く。SIGHUP で変えられる）に従う。
#[cfg(feature = "http2")]
async fn serve_cleartext(mut stream: TcpStream, peer_addr: SocketAddr, id: usize) {
    use crate::config::CURRENT_CONFIG;
    use crate::proxy::{detect_protocol_with_buffer, handle_h2c_connection, ProtocolType};
    use ftlog::debug;
    // H7: `peer_addr.ip().to_string()` のヒープ確保を排除し、TLS のワーカー
    // （`handle_connection`）と同じスタック固定長バッファ（`IpStr`）を使う。
    let client_ip = crate::http_utils::IpStr::new(peer_addr.ip());
    // H2C専用リスナーでも、プロトコル検出を実行して初期データを取得
    // これにより、クライアントがまだプリフェースを送信していない場合でも
    // 正しく処理できる
    let (protocol_type, initial_data) = detect_protocol_with_buffer(&mut stream).await;

    // F-260: 方針はリスナーの id で引く（表はリスナーの列と同じ長さ。範囲外は起こらないが既定で受ける）
    let policy = CURRENT_CONFIG
        .load()
        .cleartext
        .get(id)
        .copied()
        .unwrap_or_default();

    match protocol_type {
        ProtocolType::H2C if policy.allow_h2c => {
            handle_h2c_connection(stream, client_ip.as_str(), initial_data, policy.redirect())
                .await;
        }
        ProtocolType::H2C => {
            // allow_h2c = false: GOAWAY(PROTOCOL_ERROR) で閉じる（B-102 の作法）
            debug!(
                "[Cleartext] h2c is not allowed (allow_h2c = false), rejecting {}",
                peer_addr
            );
            crate::proxy::reject_h2c_invalid_preface(stream).await;
        }
        ProtocolType::Http11 if policy.allow_http => {
            if policy.secure_redirect {
                crate::config::handle_http_redirect(stream, initial_data, policy.redirect_port)
                    .await;
            } else {
                crate::proxy::handle_plain_http1_connection(stream, initial_data, peer_addr).await;
            }
        }
        ProtocolType::Http11 => {
            // allow_http = false: 426 を返して閉じる
            debug!(
                "[Cleartext] plaintext HTTP/1.1 is not allowed (allow_http = false), rejecting {}",
                peer_addr
            );
            crate::proxy::reject_plain_http1(stream).await;
        }
        ProtocolType::TLS => {
            // 平文リスナーに TLS が来た（ポートの取り違え）
            debug!(
                "[Cleartext] TLS handshake on the cleartext listener from {}, closing",
                peer_addr
            );
        }
        ProtocolType::Closed => {
            // 何も送らずに閉じた・アイドルのまま満了（TCP のヘルスチェック等。B-122）
            debug!(
                "[Cleartext] connection from {} closed before sending any data",
                peer_addr
            );
        }
        ProtocolType::Unknown => {
            // 接続ごとのログなので debug（相手が任意に発生させられる。B-122）
            debug!(
                "[Cleartext] Unknown protocol from {}, closing connection",
                peer_addr
            );
            // RFC 9113 §3.4: GOAWAY(PROTOCOL_ERROR) を返して閉じる。
            crate::proxy::reject_h2c_invalid_preface(stream).await;
        }
    }
}

/// HTTP/3（QUIC/UDP）のリスナー 1 個の `workers` 本のワーカーを起動する。
///
/// TCP 側と同様に thread-per-core: 各ワーカーが SO_REUSEPORT UDP ソケットを持ち、カーネルがフロー（5-tuple）
/// 単位でパケットを分散する。QUIC 状態はワーカー内で完結する（スレッドローカルの `H3_SEND_SCRATCH` 等は
/// 「1 スレッド 1 HTTP/3 サーバ」を前提にするので、リスナーごとに別スレッドにする）。
///
/// 証明書は memfd 経由で quiche に渡す（Landlock 下でもファイルを開かない）。各ワーカーは PEM を写したら
/// `copied` を 1 進めて `Arc` を手放す（呼び出し側は全員が写し終えたのを数えてから元の PEM を消す）。
#[cfg(feature = "http3")]
#[allow(clippy::too_many_arguments)] // 理由: 起動時に 1 回だけ呼ぶ配線（リスナー・証明書・設定・共有の値）
pub(crate) fn spawn_http3_listener(
    spec: &ListenerSpec,
    addr: SocketAddr,
    workers: usize,
    base: &crate::http3_server::Http3ServerConfig,
    cert_pem: &Arc<Vec<u8>>,
    key_pem: &Arc<Vec<u8>>,
    copied: &Arc<std::sync::atomic::AtomicUsize>,
    env: &WorkerEnv,
    handles: &mut Vec<thread::JoinHandle<()>>,
) {
    info!(
        "listener \"{}\" HTTP/3 {} (UDP) workers={}",
        spec.name, addr, workers
    );
    let label = format!("{} HTTP/3", spec.name);
    for thread_id in 0..workers {
        let cert_pem = cert_pem.clone();
        let key_pem = key_pem.clone();
        let copied = copied.clone();
        let mut worker_h3_config = base.clone();
        let assigned_core = env.core_for(thread_id);
        let label = label.clone();

        let handle = spawn_worker_thread(move || {
            pin_to_core(&label, thread_id, assigned_core);

            // memfd経由で証明書をロード（Landlock対応）
            // 事前読み込み済みのPEMデータをmemfdに書き込み、
            // /proc/self/fd/<fd>パス経由でquicheに渡す
            //
            // セキュリティ: Vec をクローンした後、Arc を即座にドロップして
            // メインスレッドでのゼロ化を可能にする
            let cert_data = (*cert_pem).clone();
            let key_data = (*key_pem).clone();

            // Arc 参照を即座にドロップ（参照カウントを減らす）してから写し終えたことを知らせる
            drop(cert_pem);
            drop(key_pem);
            copied.fetch_add(1, Ordering::Release);

            worker_h3_config.cert_path = String::new(); // memfd使用時は不要
            worker_h3_config.key_path = String::new(); // memfd使用時は不要
            worker_h3_config.cert_pem = Some(cert_data);
            worker_h3_config.key_pem = Some(key_data);

            info!("[{} {}] Starting...", label, thread_id);

            if let Err(e) = crate::http3_server::run_http3_server(addr, worker_h3_config) {
                error!("[{} {}] Server error: {}", label, thread_id, e);
            }

            info!("[{} {}] Stopped", label, thread_id);
        });
        handles.push(handle);
    }
}
