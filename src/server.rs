//! サーバーライフサイクル管理モジュール
//!
//! シグナルハンドラ、バックグラウンドスレッド、リスナーソケットの作成を担当します。

use crate::config::*;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};
use crate::runtime::tcp::{TcpListener, TcpStream};
use crate::runtime::time::timeout;
use ftlog::{debug, error, info, warn};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
// Arc::clone は SIGHUP ハンドラ登録（Unix 専用）でのみ使う。Windows には SIGHUP が無い
// ため未使用 import 警告になる。
#[cfg(unix)]
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::http_utils::*;
use crate::metrics::*;
use crate::pool::*;
use crate::system::*;
use crate::upstream::*;

use crate::cache;
// AsRawFd は Linux（CBPF reuseport）・FreeBSD（capsicum rights 制限）の
// リスナー fd 取得に加え、F-164 の UDS リスナー（`OwnedListenerFd`）が
// `cfg(unix)` 全体で必要とする。Windows 等では未使用のため cfg でゲートする。
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

// log_ktls_status は crate::logging モジュールに移動しました。

/// capability mode（FreeBSD capsicum）セーフなスリープ。
///
/// FreeBSD の capability mode（`cap_enter(2)`）下では `std::thread::sleep` が内部で
/// 使う `clock_nanosleep(CLOCK_MONOTONIC)` が `ECAPMODE`（os error 94）で失敗し、
/// std 内部のアサーションで panic する（F-123 の VM 検証で発見）。設定/TLS リロード
/// 等の背景監視スレッドは capability mode 内でも周期スリープするため、capsicum セーフ
/// な `select(2)` タイムアウト（fd 集合なし = 権利不要）で代替する。他 OS では
/// `std::thread::sleep` に委譲し挙動を変えない。
// 理由付き allow: 非 FreeBSD 経路は従来どおり std スリープ（イベントループ外の
// 背景スレッド用ラッパ）。
#[allow(clippy::disallowed_methods)]
pub(crate) fn cap_safe_sleep(dur: Duration) {
    #[cfg(target_os = "freebsd")]
    {
        let mut tv = libc::timeval {
            tv_sec: dur.as_secs() as libc::time_t,
            tv_usec: dur.subsec_micros() as libc::suseconds_t,
        };
        loop {
            let ret = unsafe {
                libc::select(
                    0,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    &mut tv,
                )
            };
            if ret >= 0 {
                break;
            }
            if std::io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break;
            }
            // EINTR: FreeBSD の select は tv に残余時間を書き戻すため、そのまま継続。
        }
    }
    #[cfg(not(target_os = "freebsd"))]
    {
        std::thread::sleep(dur);
    }
}

/// シグナルハンドラのセットアップ
pub fn setup_signal_handler() {
    // SIGINT, SIGTERM をキャッチしてシャットダウンフラグを設定
    ctrlc::set_handler(move || {
        info!("Received shutdown signal, initiating graceful shutdown...");
        SHUTDOWN_FLAG.store(true, Ordering::SeqCst);
    })
    .expect("Failed to set signal handler");

    // SIGHUP をキャッチして設定リロードをトリガー（Linux/Unix）
    #[cfg(unix)]
    {
        use signal_hook::consts::SIGHUP;
        use signal_hook::flag as signal_flag;

        // SIGHUP で RELOAD_FLAG を true に設定
        // signal-hook はシグナルセーフな方法でフラグを更新
        if let Err(e) = signal_flag::register(SIGHUP, Arc::clone(&RELOAD_FLAG)) {
            warn!("Failed to register SIGHUP handler: {}", e);
        } else {
            info!("SIGHUP handler registered for configuration hot reload");
        }

        // F-03: SIGHUP で TLS_RELOAD_FLAG も立てる（証明書ホットリロード用）
        if let Err(e) = signal_flag::register(SIGHUP, Arc::clone(&TLS_RELOAD_FLAG)) {
            warn!("Failed to register SIGHUP handler for TLS reload: {}", e);
        }
    }
}

/// 設定リロードスレッドを起動
///
/// RELOAD_FLAG を監視し、シグナルを受け取ったら設定をリロードします。
/// ワーカースレッドは CURRENT_CONFIG を参照するため、
/// リロード後の新規リクエストは自動的に新しい設定を使用します。
// 理由付き allow: 専用リロード監視スレッド上の待機（イベントループ外）。
#[allow(clippy::disallowed_methods)]
pub fn spawn_reload_thread() {
    thread::spawn(move || {
        info!("Configuration reload thread started");

        loop {
            cap_safe_sleep(Duration::from_millis(500));

            // シャットダウン中はリロードしない
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                break;
            }

            // リロードフラグをチェック
            if RELOAD_FLAG.swap(false, Ordering::SeqCst) {
                info!("SIGHUP received, reloading configuration...");

                // グローバル変数から設定ファイルパスを取得
                let config_path = CONFIG_PATH.load();

                match reload_config(&config_path) {
                    Ok(()) => {
                        // アクセスログライタースレッドをホットリロード
                        // ファイルパスやフォーマットが変わった場合、旧スレッドを終了して新スレッドを起動する
                        #[cfg(feature = "access-log")]
                        {
                            let cfg = crate::config::CURRENT_CONFIG.load();
                            crate::access_log::reload_access_log_writer(&cfg.access_log_config);
                        }
                        info!("Configuration reloaded successfully");
                        info!("New requests will use updated routes");
                    }
                    Err(e) => {
                        error!("Failed to reload configuration: {}", e);
                        error!("Keeping previous configuration");
                    }
                }
            }
        }

        info!("Configuration reload thread stopped");
    });
}

/// TLS 証明書リロードスレッドを起動（F-03）
///
/// 以下のタイミングで証明書を再読み込みする:
/// - SIGHUP 受信時（TLS_RELOAD_FLAG）: 即座に reload_now()
/// - 定期チェック（interval_secs ごと）: mtime 変化を検知して reload
///
/// リロードはグローバル ArcSwap を差し替えるため、既存接続は影響を受けず、
/// 新規ハンドシェイクのみが新しい証明書を使用する。
// 理由付き allow: 専用 TLS リロードスレッド上の待機（イベントループ外）。
#[allow(clippy::disallowed_methods)]
pub fn spawn_tls_reloader(mut reloader: crate::tls_reload::TlsCertReloader, interval_secs: u64) {
    thread::spawn(move || {
        info!("TLS certificate reload thread started");
        let mut elapsed: u64 = 0;
        loop {
            cap_safe_sleep(Duration::from_millis(500));
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                break;
            }

            // SIGHUP による即時リロード
            if TLS_RELOAD_FLAG.swap(false, Ordering::SeqCst) {
                info!("SIGHUP received, reloading TLS certificate...");
                match reloader.reload_now() {
                    Ok(()) => info!("TLS certificate reloaded via SIGHUP"),
                    Err(e) => error!("TLS certificate reload (SIGHUP) failed: {}", e),
                }
                elapsed = 0;
                continue;
            }

            // 定期 mtime チェック
            elapsed += 500;
            if elapsed >= interval_secs * 1000 {
                elapsed = 0;
                reloader.check_and_reload();
            }
        }
        info!("TLS certificate reload thread stopped");
    });
}

/// stale-while-revalidate: バックグラウンドでキャッシュを更新
///
/// staleキャッシュを返した後、バックグラウンドでバックエンドに再リクエストし、
/// レスポンスでキャッシュを更新します。
///
/// ## Request Collapsing
///
/// 同一キャッシュキーに対して既に更新が進行中の場合、
/// 重複したリクエストをスキップしてバックエンド過負荷を防ぎます。
pub fn spawn_background_revalidation(
    cache_key: cache::CacheKey,
    upstream_group: UpstreamGroup,
    security: SecurityConfig,
    method: Vec<u8>,
    req_path: Vec<u8>,
    prefix: Vec<u8>,
    // F-168: proxy.rs 側のリクエストヘッダーが `Bytes`（accumulated からのゼロコピー
    // 切り出し）になったため、こちらもそれに合わせる（これはリクエストヘッダーの再送用で
    // レスポンス側とは無関係）。
    headers: Vec<(bytes::Bytes, bytes::Bytes)>,
) {
    let hash = cache_key.hash_value();

    // Request Collapsing: 同一キーに対して既に更新中であればスキップ
    if !cache::try_start_revalidation(hash) {
        debug!(
            "Background revalidation skipped (already in progress) for {:?}",
            cache_key.path()
        );
        return;
    }

    // パニック耐性のあるspawn (stale-while-revalidate のバックグラウンドタスク)
    spawn_with_panic_catch(async move {
        debug!("Background revalidation started for {:?}", cache_key.path());

        // 完了時に必ず更新フラグをクリアするためのスコープガード
        // このクロージャはパニック時でも実行される（Drop trait）
        struct RevalidationGuard(u64);
        impl Drop for RevalidationGuard {
            fn drop(&mut self) {
                cache::finish_revalidation(self.0);
            }
        }
        let _guard = RevalidationGuard(hash);

        // サーバーを選択
        let server = match upstream_group.select("revalidation") {
            Some(s) => s,
            None => {
                debug!("No healthy server for background revalidation");
                return;
            }
        };

        let target = &server.target;
        let addr = format!("{}:{}", target.host, target.port);

        // バックエンドに接続
        let connect_timeout = Duration::from_secs(security.backend_connect_timeout_secs);
        let connect_result = timeout(connect_timeout, TcpStream::connect_str(&addr)).await;

        let mut backend_stream = match connect_result {
            Ok(Ok(stream)) => {
                let _ = stream.set_nodelay(true);
                stream
            }
            _ => {
                debug!("Background revalidation: failed to connect to {}", addr);
                return;
            }
        };

        // リクエストを構築（Cow<str>で借用優先）
        let path_str = std::str::from_utf8(&req_path).unwrap_or("/");
        let sub_path: std::borrow::Cow<'_, str> = if prefix.is_empty() {
            std::borrow::Cow::Borrowed(path_str)
        } else {
            let prefix_str = std::str::from_utf8(&prefix).unwrap_or("");
            match path_str.strip_prefix(prefix_str) {
                Some(stripped) => std::borrow::Cow::Borrowed(stripped),
                None => std::borrow::Cow::Borrowed(path_str),
            }
        };

        // ホスト名を取得
        let host_header = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(b"host"))
            .map(|(_, v)| v.as_ref())
            .unwrap_or(target.host.as_bytes());

        let method_str = std::str::from_utf8(&method).unwrap_or("GET");

        // HTTPリクエストを構築
        let mut request = Vec::with_capacity(512);
        request.extend_from_slice(method_str.as_bytes());
        request.extend_from_slice(b" ");
        request.extend_from_slice(sub_path.as_bytes());
        request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
        request.extend_from_slice(host_header);
        request.extend_from_slice(b"\r\nConnection: close\r\n");

        // 元のヘッダーを追加（一部除外）
        for (name, value) in &headers {
            if name.eq_ignore_ascii_case(b"host")
                || name.eq_ignore_ascii_case(b"connection")
                || name.eq_ignore_ascii_case(b"content-length")
            {
                continue;
            }
            request.extend_from_slice(name);
            request.extend_from_slice(b": ");
            request.extend_from_slice(value);
            request.extend_from_slice(b"\r\n");
        }
        request.extend_from_slice(b"\r\n");

        // リクエスト送信
        let write_result = timeout(WRITE_TIMEOUT, backend_stream.write_all(request)).await;
        if !matches!(write_result, Ok((Ok(_), _))) {
            debug!("Background revalidation: failed to send request");
            return;
        }

        // レスポンス受信
        let mut accumulated = Vec::with_capacity(BUF_SIZE);
        let mut status_code = 0u16;

        loop {
            let read_buf = buf_get();
            let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

            let (res, mut returned_buf) = match read_result {
                Ok(result) => result,
                Err(_) => break,
            };

            let n = match res {
                Ok(0) | Err(_) => {
                    buf_put(returned_buf);
                    break;
                }
                Ok(n) => n,
            };

            returned_buf.set_valid_len(n);
            accumulated.extend_from_slice(returned_buf.as_valid_slice());
            buf_put(returned_buf);

            // ヘッダー解析
            if let Some(parsed) = parse_http_response(&accumulated) {
                status_code = parsed.status_code;
                let header_len = parsed.header_len;
                let body_start = accumulated[header_len..].to_vec();

                // ボディを読み込み（Content-Length または接続終了まで）
                let mut body = body_start;
                if let Some(cl) = parsed.content_length {
                    let remaining = cl.saturating_sub(body.len());
                    if remaining > 0 {
                        let additional =
                            buffer_exact_bytes_simple(&mut backend_stream, remaining).await;
                        body.extend(additional);
                    }
                } else if !parsed.is_chunked {
                    // 接続終了まで読む（最大10MB）
                    const MAX_SIZE: usize = 10 * 1024 * 1024;
                    loop {
                        if body.len() >= MAX_SIZE {
                            break;
                        }
                        let read_buf = buf_get();
                        let read_result =
                            timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

                        let (res, mut returned_buf) = match read_result {
                            Ok(result) => result,
                            Err(_) => break,
                        };

                        let n = match res {
                            Ok(0) | Err(_) => {
                                buf_put(returned_buf);
                                break;
                            }
                            Ok(n) => n,
                        };

                        returned_buf.set_valid_len(n);
                        body.extend_from_slice(returned_buf.as_valid_slice());
                        buf_put(returned_buf);
                    }
                }

                // ヘッダー抽出
                let headers_data = &accumulated[..header_len];
                let mut headers_storage = [httparse::EMPTY_HEADER; 64];
                let mut response = httparse::Response::new(&mut headers_storage);

                if response.parse(headers_data).is_ok() {
                    let response_headers: Vec<(Box<[u8]>, Box<[u8]>)> = response
                        .headers
                        .iter()
                        .map(|h| (h.name.as_bytes().into(), h.value.into()))
                        .collect();

                    // キャッシュを更新
                    if let Some(cache_manager) = cache::get_global_cache() {
                        if cache_manager.store(
                            cache_key.clone(),
                            status_code,
                            response_headers,
                            body,
                        ) {
                            info!(
                                "Background revalidation: cache updated for {:?}",
                                cache_key.path()
                            );
                        }
                    }
                }

                break;
            }

            // ヘッダーが大きすぎる
            if accumulated.len() > MAX_HEADER_SIZE {
                break;
            }
        }

        debug!("Background revalidation completed (status={})", status_code);
        // _guard がドロップされて finish_revalidation(hash) が呼ばれる
    });
}

/// バックグラウンド更新用の簡易バイト読み込み
pub async fn buffer_exact_bytes_simple(
    backend_stream: &mut TcpStream,
    mut remaining: usize,
) -> Vec<u8> {
    let mut result = Vec::with_capacity(remaining);

    while remaining > 0 {
        let read_buf = buf_get();
        let read_result = timeout(READ_TIMEOUT, backend_stream.read(read_buf)).await;

        let (res, mut returned_buf) = match read_result {
            Ok(r) => r,
            Err(_) => break,
        };

        let n = match res {
            Ok(0) | Err(_) => {
                buf_put(returned_buf);
                break;
            }
            Ok(n) => n.min(remaining),
        };

        returned_buf.set_valid_len(n);
        result.extend_from_slice(&returned_buf.as_valid_slice()[..n]);
        buf_put(returned_buf);
        remaining = remaining.saturating_sub(n);
    }

    result
}

/// キャッシュクリーンアップスレッドを起動
///
/// 定期的に以下の処理を実行:
/// - 期限切れエントリの削除
/// - LRU eviction（メモリ使用量が閾値を超えた場合）
/// - メトリクスの更新
// 理由付き allow: 専用キャッシュクリーンアップスレッド上の待機（イベントループ外）。
#[allow(clippy::disallowed_methods)]
pub fn spawn_cache_cleanup_thread() {
    thread::spawn(move || {
        info!("Cache cleanup thread started (interval=60s)");

        loop {
            // 60秒ごとにクリーンアップを実行
            cap_safe_sleep(Duration::from_secs(60));

            // シャットダウン中は終了
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                info!("Cache cleanup thread shutting down");
                break;
            }

            // グローバルキャッシュを取得
            if let Some(cache_manager) = cache::get_global_cache() {
                // 1. 期限切れエントリの削除
                let expired_count = cache_manager.evict_expired();
                if expired_count > 0 {
                    debug!("Cache cleanup: evicted {} expired entries", expired_count);
                    record_cache_eviction("expired", expired_count);
                }

                // 2. LRU eviction（メモリ使用量が閾値を超えた場合）
                let lru_count = cache_manager.evict_lru();
                if lru_count > 0 {
                    debug!("Cache cleanup: evicted {} LRU entries", lru_count);
                    record_cache_eviction("lru", lru_count);
                }

                // 3. ディスクキャッシュのクリーンアップ
                match cache_manager.evict_disk() {
                    Ok(disk_count) if disk_count > 0 => {
                        debug!("Cache cleanup: evicted {} disk entries", disk_count);
                        record_cache_eviction("disk", disk_count);
                    }
                    Err(e) => {
                        warn!("Cache disk cleanup error: {}", e);
                    }
                    _ => {}
                }

                // 4. メトリクスを更新
                let stats = cache_manager.stats();
                update_cache_size_metrics(&stats);
            }
        }
    });
}

/// WASMタイマースレッドを起動
///
/// WASM モジュールの `on_tick` コールバックを定期的に呼び出します。
/// tick period は各モジュールの `proxy_set_tick_period` 設定に基づきます。
#[cfg(feature = "wasm")]
// 理由付き allow: 専用 WASM tick スレッド上の待機（イベントループ外）。
#[allow(clippy::disallowed_methods)]
pub fn spawn_wasm_tick_thread() {
    thread::spawn(move || {
        info!("WASM tick thread started");

        // 最小tick間隔を取得（デフォルト: 100ms）
        let tick_interval = {
            let config = CURRENT_CONFIG.load();
            if config.wasm_filter_engine.is_some() {
                // get_min_tick_period() returns Option<Duration>
                crate::wasm::get_min_tick_period().unwrap_or(Duration::from_millis(100))
            } else {
                Duration::from_secs(1) // WASM未設定時は1秒
            }
        };

        debug!("WASM tick interval: {:?}", tick_interval);

        loop {
            cap_safe_sleep(tick_interval);

            // シャットダウン中は終了
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                info!("WASM tick thread shutting down");
                break;
            }

            // WASM tick処理
            let config = CURRENT_CONFIG.load();
            if let Some(ref wasm_engine) = config.wasm_filter_engine {
                crate::wasm::process_ticks(wasm_engine);

                // キュー通知処理（P4: Queue Notification Integration）
                crate::wasm::process_pending_notifications(wasm_engine);

                // P3: Pending HTTP call processing
                // Take all globally registered pending calls and execute them
                let pending_calls = crate::wasm::take_global_pending_calls();
                for pending in pending_calls {
                    let upstream_name = &pending.call.upstream;

                    debug!(
                        "[wasm:http_call] Processing pending call: module='{}' token={} upstream='{}' timeout={}ms",
                        pending.module_name,
                        pending.token,
                        upstream_name,
                        pending.call.timeout_ms
                    );

                    // Look up the upstream in config.upstream_groups
                    let upstream_groups = &config.upstream_groups;
                    let response = if let Some(group) = upstream_groups.get(upstream_name) {
                        // Select a backend server
                        if let Some(server) = group.select("0.0.0.0") {
                            // Get connection info
                            let host = server.host();
                            let port = server.port();
                            let use_tls = server.use_tls();

                            debug!(
                                "[wasm:http_call] Connecting to upstream: {}:{} (tls={})",
                                host, port, use_tls
                            );

                            // Execute HTTP call using http_executor
                            crate::wasm::http_executor::execute_http_call_safe(
                                &pending, host, port, use_tls,
                            )
                        } else {
                            warn!("[wasm:http_call] No healthy servers in upstream '{}' for module '{}'",
                                upstream_name, pending.module_name);
                            crate::wasm::HttpCallResponse {
                                status_code: 503,
                                headers: vec![(
                                    b"x-wasm-error".to_vec(),
                                    b"no_healthy_servers".to_vec(),
                                )],
                                body: b"No healthy upstream servers available".to_vec(),
                                trailers: vec![],
                            }
                        }
                    } else {
                        warn!(
                            "[wasm:http_call] Upstream '{}' not found for module '{}'",
                            upstream_name, pending.module_name
                        );
                        crate::wasm::HttpCallResponse {
                            status_code: 502,
                            headers: vec![(
                                b"x-wasm-error".to_vec(),
                                b"upstream_not_found".to_vec(),
                            )],
                            body: format!("Upstream '{}' not found", upstream_name).into_bytes(),
                            trailers: vec![],
                        }
                    };

                    // Deliver response to WASM module.
                    // tick スレッド（io_uring ワーカーではない背景スレッド）上で
                    // 非同期 WASM 実行を完走させる。ホットパスではないため block_on で良い。
                    let _ = futures::executor::block_on(wasm_engine.on_http_call_response(
                        &pending.module_name,
                        pending.token,
                        response,
                    ));
                }

                // F-139: pending gRPC 呼び出しの実行は専用スレッド
                // （`spawn_wasm_grpc_thread`）へ一本化した。tick スレッド
                // （100ms 固定周期）で状態機械を駆動すると 1 ステップ 100ms に
                // なり、F-134 時点の「1 呼び出し 1 ブロッキング完結」より
                // レイテンシが悪化してしまうため、条件変数 / poll(2) 駆動の
                // 別スレッドに分離した。詳細は
                // `docs/artifacts/f139_wasm_grpc_nonblocking_design.md`。
            }
        }
    });
}

/// F-139: 専用 gRPC 実行スレッド。
///
/// WASM tick スレッド（100ms 固定周期）とは別に 1 本立て、gRPC 呼び出しの状態機械
/// （`crate::wasm::host::grpc_executor::GrpcRunner`）を条件変数 / poll(2) 駆動で
/// 進める。アクティブな呼び出しが無ければ新規登録があるまで条件変数で待ち
/// （ビジースピン禁止）、アクティブな呼び出しがあれば poll(2) で
/// 「いずれかのソケットが読み書き可能 or 最短デッドライン」まで待つ
/// （`GrpcRunner::poll_all` 内部）。
///
/// **ホットパス絶対規則との関係**: この処理はデータプレーン（io_uring イベント
/// ループ）とは完全に別のバックグラウンド専用スレッド上でのみ実行される。
/// したがってブロッキング `poll(2)` / 同期 I/O を使ってよい。
// `wasm` と `grpc` の**両方**が必要（本体が `crate::wasm` のレジストリ・エンジンを参照するため）。
// `--features grpc` 単独ビルドでも壊れないよう、呼び出し側（`entry.rs`）と同じ条件で切る。
#[cfg(all(feature = "wasm", feature = "grpc"))]
// 理由付き allow: 専用 gRPC 実行スレッド上の待機（イベントループ外）。
#[allow(clippy::disallowed_methods)]
pub fn spawn_wasm_grpc_thread() {
    thread::spawn(move || {
        info!("WASM gRPC executor thread started");

        let mut runner = crate::wasm::host::grpc_executor::GrpcRunner::new();

        loop {
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                info!("WASM gRPC executor thread shutting down");
                break;
            }

            // 新規登録（呼び出し開始・逐次送出・キャンセル）を取り込む。
            let new_calls = crate::wasm::host::grpc_executor::take_global_pending_grpc_calls();
            let new_sends = crate::wasm::host::grpc_executor::take_global_pending_grpc_sends();
            let cancels = crate::wasm::host::grpc_executor::take_global_pending_grpc_cancels();

            let config = CURRENT_CONFIG.load();
            let Some(ref wasm_engine) = config.wasm_filter_engine else {
                // WASM 未設定なら本来登録も発生しないはずだが、念のため待機する。
                crate::wasm::host::grpc_executor::wait_for_grpc_work();
                continue;
            };

            let ingest_events =
                runner.ingest(new_calls, new_sends, cancels, &config.upstream_groups);
            for ev in ingest_events {
                crate::wasm::host::grpc_executor::deliver_event(wasm_engine, ev);
            }

            if !runner.has_active_calls() {
                // アクティブな呼び出しが無い間は条件変数で待つ（ビジースピン禁止）。
                crate::wasm::host::grpc_executor::wait_for_grpc_work();
                continue;
            }

            // アクティブな呼び出しがある間は poll(2) で駆動する。
            let events = runner.poll_all();
            for ev in events {
                crate::wasm::host::grpc_executor::deliver_event(wasm_engine, ev);
            }
        }
    });
}

// 理由付き allow: 専用ヘルスチェックスレッド上の待機（イベントループ外）。
#[allow(clippy::disallowed_methods)]
pub fn spawn_health_check_thread() {
    thread::spawn(move || {
        info!("Health check thread started");

        loop {
            // シャットダウン中はチェックしない
            if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                break;
            }

            // 設定を取得
            let config = CURRENT_CONFIG.load();

            // 各 Upstream グループをチェック
            for (name, group) in config.upstream_groups.iter() {
                if let Some(ref hc_config) = group.health_check {
                    // 各サーバーをチェック
                    for server in &group.servers {
                        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                            break;
                        }

                        let target = &server.target;
                        let addr = format!("{}:{}", target.host, target.port);

                        // チェック種別に応じてヘルスチェックを実行（F-22）
                        let timeout_dur = Duration::from_secs(hc_config.timeout_secs);
                        let check_result = match hc_config.check_type {
                            HealthCheckType::Tcp => perform_tcp_health_check(&addr, timeout_dur),
                            HealthCheckType::Grpc => perform_grpc_health_check(
                                &addr,
                                &hc_config.path,
                                hc_config.use_tls,
                                hc_config.verify_cert,
                                timeout_dur,
                            ),
                            HealthCheckType::Http => perform_health_check(
                                &addr,
                                &target.host,
                                &hc_config.path,
                                hc_config.use_tls,
                                hc_config.verify_cert,
                                timeout_dur,
                                &hc_config.healthy_statuses,
                            ),
                        };

                        // メトリクス: ヘルスチェック結果を更新
                        update_upstream_health(name, &addr, check_result);

                        if check_result {
                            server.record_success(hc_config.healthy_threshold);
                        } else {
                            server.record_failure(hc_config.unhealthy_threshold);
                            ftlog::debug!("Health check failed for {} (upstream: {})", addr, name);
                        }
                    }
                }
            }

            // 次のチェックまで待機（最短間隔を使用）
            // シャットダウン時に迅速に終了するため、短い間隔で分割してスリープ
            let min_interval = config
                .upstream_groups
                .values()
                .filter_map(|g| g.health_check.as_ref())
                .map(|hc| hc.interval_secs)
                .min()
                .unwrap_or(10);

            // 500ms間隔でシャットダウンフラグをチェック
            let sleep_iterations = (min_interval * 2) as usize; // 500ms × 2 = 1秒
            for _ in 0..sleep_iterations {
                if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                    break;
                }
                cap_safe_sleep(Duration::from_millis(500));
            }
        }

        info!("Health check thread stopped");
    });
}

// SO_REUSEPORT CBPF ロードバランシング
// ====================

// CBPF_ATTACHED, create_reuseport_cbpf_program, attach_reuseport_cbpf は
// crate::system モジュールに移動しました。

/// リスナーソケットを作成する（SO_REUSEPORT + オプションのCBPF振り分け）
///
/// # 引数
/// * `addr` - バインドするアドレス
/// * `balancing` - 振り分け方式
/// * `num_workers` - ワーカースレッド数（CBPF使用時に必要）
/// * `worker_id` - このワーカーのID（最初のワーカーがCBPFをアタッチ）
pub fn create_listener(
    addr: SocketAddr,
    #[allow(unused_variables)] balancing: ReuseportBalancing,
    #[allow(unused_variables)] num_workers: usize,
    #[allow(unused_variables)] worker_id: usize,
) -> io::Result<TcpListener> {
    // SO_REUSEPORT を有効にして listen する（カスタム io_uring 実装）
    let listener = TcpListener::bind_reuse_port(addr)?;

    // FreeBSD: capsicum が有効なら、このリスナー fd を最小権利へ制限する（F-120 Phase 4）。
    // 全リスナー作成経路（HTTP/H2C/L4）がこの関数を通るため、ここが単一の適用ポイント。
    #[cfg(target_os = "freebsd")]
    if CURRENT_CONFIG.load().global_security.enable_capsicum {
        if let Err(e) = crate::security::capsicum::limit_listener_rights(listener.as_raw_fd()) {
            warn!(
                "[Worker {}] capsicum: failed to limit listener rights for {}: {}",
                worker_id, addr, e
            );
        } else {
            debug!(
                "[Worker {}] capsicum: listener rights limited (CAP_ACCEPT|CAP_EVENT|...) for {}",
                worker_id, addr
            );
        }
    }

    // Linux環境でCBPF振り分けが有効な場合、最初のワーカーのみCBPFプログラムをアタッチ
    // 後続のワーカーはreuseportグループに参加し、自動的にBPFプログラムを継承する
    #[cfg(target_os = "linux")]
    if balancing == ReuseportBalancing::Cbpf {
        // CAS操作で最初の1回だけアタッチを実行
        let prev = CBPF_ATTACHED.compare_exchange(0, 1, Ordering::SeqCst, Ordering::SeqCst);

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
                    warn!(
                        "[Worker {}] CBPF attach failed, falling back to kernel default: {}",
                        worker_id, e
                    );
                    // フラグをリセットして他のワーカーも試行できるようにする（オプション）
                    // CBPF_ATTACHED.store(0, Ordering::SeqCst);
                }
            }
        }
    }

    Ok(listener)
}

// ====================
// Unix ドメインソケット（UDS）リスナー（F-164）
// ====================

/// AF_UNIX リスナー fd の所有権ラッパー。
///
/// `bind_unix_listener` が返す。TCP と異なり AF_UNIX には `SO_REUSEPORT` が無いため、
/// 各ワーカーが個別に bind するのではなく、この fd を 1 度だけ bind し、各ワーカーは
/// [`OwnedListenerFd::dup_listener`] で `dup(2)` した自分専用の `runtime::TcpListener`
/// を作る（カーネルが accept を分散する）。`Drop` はこの元 fd のみを close する
/// （dup 後の各ワーカーの fd は各自の `TcpListener::drop` で独立に close される）。
#[cfg(unix)]
pub struct OwnedListenerFd {
    fd: std::os::unix::io::RawFd,
    /// bind したソケットファイルのパス（グレースフルシャットダウン時の unlink 用）。
    path: std::path::PathBuf,
}

#[cfg(unix)]
impl OwnedListenerFd {
    /// この fd を `dup(2)` して、ワーカー専用の `runtime::TcpListener` を作る。
    pub fn dup_listener(&self) -> io::Result<TcpListener> {
        let new_fd = unsafe { libc::dup(self.fd) };
        if new_fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: dup(2) が返した新規 fd であり、この TcpListener が単独で所有する。
        Ok(unsafe { TcpListener::from_raw_fd_unix(new_fd) })
    }

    /// bind したソケットファイルのパス。
    pub fn path(&self) -> &std::path::Path {
        self.path.as_path()
    }
}

#[cfg(unix)]
impl AsRawFd for OwnedListenerFd {
    fn as_raw_fd(&self) -> std::os::unix::io::RawFd {
        self.fd
    }
}

#[cfg(unix)]
impl Drop for OwnedListenerFd {
    fn drop(&mut self) {
        unsafe { libc::close(self.fd) };
    }
}

/// AF_UNIX ソケットにリッスンする（F-164、`[server].listen` / `[server].h2c_listen` の
/// `unix:<path>` 形式向け）。
///
/// ワーカー spawn 前に **1 回だけ** 呼ぶ想定のコールドパス関数（起動時のみ）。
///
/// # 挙動
/// - 既存パスが**ソケットである場合に限り** `unlink` してから bind する
///   （stale socket 対応）。通常ファイル・ディレクトリの場合は安全側に倒し、
///   データを消さずにエラーを返す。
/// - bind 成功後、ソケットファイルへ `mode`（`unix_socket_permissions` 由来）を
///   `chmod(path, mode)` で適用する。Linux では AF_UNIX ソケット fd に対する
///   `fchmod(2)` はソケットのバインドされたパスのパーミッションを変更しない
///   （fd 経由では効果が無い/無視される）ため、パス経由の `chmod` が必須である
///   （bind 直後〜listen 前に行うため、TOCTOU ウィンドウは他プロセスがまだこの
///   パスを知らない起動シーケンス内に限られる）。
/// - FreeBSD capsicum が有効なら `create_listener` と同じ `limit_listener_rights`
///   をこのリスナー fd にも適用する。
///
/// # 引数
/// * `path` - ソケットファイルパス。
/// * `mode` - bind 直後に設定するパーミッション（例: `0o660`）。
// 理由付き allow: 起動時に 1 度だけ呼ばれるコールドパス（ワーカー spawn 前の bind）。
// `std::fs`/`libc` の同期呼び出しはホットパス（accept 以降のデータプレーン）には
// 一切含まれない。stat/unlink/bind/chmod/listen はすべて libc 直呼びで完結させ、
// clippy::disallowed_methods の対象（std::fs::metadata/remove_file 等）を使わない。
#[cfg(unix)]
pub fn bind_unix_listener(path: &std::path::Path, mode: u32) -> io::Result<OwnedListenerFd> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = CString::new(path.as_os_str().as_bytes())
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e.to_string()))?;

    // 既存パスがソケットである場合に限り unlink する（stale socket 対応）。
    // 通常ファイル・ディレクトリなら安全側に倒してエラーにする。
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let stat_ret = unsafe { libc::stat(c_path.as_ptr(), &mut st) };
    if stat_ret == 0 {
        if st.st_mode & libc::S_IFMT == libc::S_IFSOCK {
            if unsafe { libc::unlink(c_path.as_ptr()) } != 0 {
                return Err(io::Error::last_os_error());
            }
        } else {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to bind unix socket: existing path is not a socket: {}",
                    path.display()
                ),
            ));
        }
    } else {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::NotFound {
            return Err(e);
        }
    }

    let fd = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    addr.sun_family = libc::AF_UNIX as libc::sa_family_t;
    let path_bytes = path.as_os_str().as_bytes();
    if path_bytes.len() >= addr.sun_path.len() {
        unsafe { libc::close(fd) };
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("unix socket path too long: {}", path.display()),
        ));
    }
    for (i, &b) in path_bytes.iter().enumerate() {
        addr.sun_path[i] = b as libc::c_char;
    }
    let addr_len =
        (std::mem::size_of::<libc::sa_family_t>() + path_bytes.len() + 1) as libc::socklen_t;

    // umask を一時的に「mode の補集合」へ設定しておくことで、bind(2) が作る
    // ソケットファイルを **最初から** 目的のパーミッションで生成する
    // （bind → chmod の間に緩いパーミッションで見える窓を作らないため）。
    // ワーカー spawn 前の起動シーケンス内でのみ実行されるコールドパスであり、
    // 直後に元の umask へ戻す。
    let old_umask = unsafe { libc::umask((0o777 & !mode) as libc::mode_t) };
    let bind_ret = unsafe { libc::bind(fd, &addr as *const _ as *const libc::sockaddr, addr_len) };
    let bind_err = io::Error::last_os_error();
    unsafe { libc::umask(old_umask) };
    if bind_ret != 0 {
        unsafe { libc::close(fd) };
        return Err(bind_err);
    }

    // umask は「落とす」方向にしか効かないため、より緩いモード（例: 0666）を
    // 指定した場合に備えて chmod でも明示的に設定する。Linux では AF_UNIX ソケット
    // fd への fchmod(2) がパスのパーミッションに反映されないため、パス経由で行う。
    if unsafe { libc::chmod(c_path.as_ptr(), mode as libc::mode_t) } != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }

    if unsafe { libc::listen(fd, 1024) } != 0 {
        let e = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(e);
    }

    // FreeBSD: capsicum が有効なら、このリスナー fd も最小権利へ制限する
    // （`create_listener` の TCP 経路と同じ適用ポイント）。
    #[cfg(target_os = "freebsd")]
    if CURRENT_CONFIG.load().global_security.enable_capsicum {
        if let Err(e) = crate::security::capsicum::limit_listener_rights(fd) {
            warn!(
                "capsicum: failed to limit unix listener rights for {}: {}",
                path.display(),
                e
            );
        } else {
            debug!(
                "capsicum: unix listener rights limited (CAP_ACCEPT|CAP_EVENT|...) for {}",
                path.display()
            );
        }
    }

    Ok(OwnedListenerFd {
        fd,
        path: path.to_path_buf(),
    })
}

/// UDS ソケットファイルを unlink する（グレースフルシャットダウン用、F-164）。
///
/// 起動時に veil 自身が bind したソケットファイルのみを対象とする想定
/// （`entry.rs` がシャットダウン経路で呼ぶ）。失敗（既に存在しない等）は警告に留め、
/// シャットダウンを妨げない。
// 理由付き allow: シャットダウン時に高々数回呼ばれるコールドパス。
#[cfg(unix)]
pub fn unlink_unix_socket(path: &std::path::Path) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(c) => c,
        Err(_) => return,
    };
    if unsafe { libc::unlink(c_path.as_ptr()) } != 0 {
        let e = io::Error::last_os_error();
        if e.kind() != io::ErrorKind::NotFound {
            warn!(
                "failed to unlink unix socket {} during shutdown: {}",
                path.display(),
                e
            );
        }
    } else {
        debug!("unlinked unix socket {} during shutdown", path.display());
    }
}

#[cfg(test)]
#[cfg(unix)]
// 理由付き allow: テスト専用モジュール（起動/イベントループ外）。ソケットファイルの
// 後始末・パーミッション検証・stale ファイル準備に std::fs を使う。
#[allow(clippy::disallowed_methods)]
mod uds_tests {
    use super::*;

    /// F-164: `bind_unix_listener` で bind したソケットへ、各ワーカー相当の
    /// `dup_listener()` で作った `TcpListener` が accept できること。
    /// ランタイムドライバ（io_uring リング）が生成できるか。
    ///
    /// Docker のビルドサンドボックス・seccomp 制限下・古いカーネルでは
    /// `io_uring_setup(2)` が拒否され、`runtime::block_on` が panic する。
    /// 実 I/O を伴うテストはそのような環境ではスキップする
    /// （`runtime::uring::tcp` の `io_uring_available` と同じ方針。E2E で網羅する）。
    #[cfg(veil_rt_uring)]
    fn runtime_driver_available() -> bool {
        crate::runtime::IoUring::new(8, 0).is_ok()
    }

    /// reactor バックエンド（epoll/kqueue）は poller の生成に特別な権限を要さないため
    /// 常に利用可能。
    #[cfg(veil_rt_reactor)]
    fn runtime_driver_available() -> bool {
        true
    }

    #[test]
    fn test_bind_unix_listener_dup_and_accept() {
        if !runtime_driver_available() {
            eprintln!(
                "runtime driver unavailable; skipping test_bind_unix_listener_dup_and_accept"
            );
            return;
        }
        let mut path = std::env::temp_dir();
        path.push(format!("veil-f164-server-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let owned = bind_unix_listener(&path, 0o660).expect("bind_unix_listener");

        // パーミッションが反映されていること。
        let meta = std::fs::metadata(&path).expect("metadata");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(meta.permissions().mode() & 0o777, 0o660);

        let listener = owned.dup_listener().expect("dup_listener");

        let connect_path = path.clone();
        let client = std::thread::spawn(move || {
            std::os::unix::net::UnixStream::connect(&connect_path).expect("client connect")
        });

        let peer_addr = crate::runtime::block_on(async move {
            let (_stream, peer_addr) = listener.accept().await.expect("accept");
            peer_addr
        });

        client.join().expect("client thread join");
        assert_eq!(peer_addr, std::net::SocketAddr::from(([127, 0, 0, 1], 0)));

        drop(owned);
        unlink_unix_socket(&path);
        assert!(!path.exists());
    }

    /// stale socket（前回起動が残した既存のソケットファイル）は再 bind 時に
    /// 自動的に unlink されて再利用できること。
    #[test]
    fn test_bind_unix_listener_replaces_stale_socket() {
        let mut path = std::env::temp_dir();
        path.push(format!("veil-f164-stale-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let first = bind_unix_listener(&path, 0o660).expect("first bind");
        drop(first);
        // 最初の bind_unix_listener は自分の fd を close するのみでファイルは
        // unlink しない（プロセス再起動を模すため、意図的にファイルを残す）。
        assert!(path.exists());

        let second = bind_unix_listener(&path, 0o660).expect("stale socket must be replaced");
        drop(second);
        unlink_unix_socket(&path);
    }

    /// 既存パスが通常ファイルの場合は安全側に倒してエラーになること
    /// （データを破壊しない）。
    #[test]
    fn test_bind_unix_listener_refuses_regular_file() {
        let mut path = std::env::temp_dir();
        path.push(format!("veil-f164-regular-{}.sock", std::process::id()));
        std::fs::write(&path, b"not a socket").expect("write regular file");

        let result = bind_unix_listener(&path, 0o660);
        assert!(result.is_err());

        let _ = std::fs::remove_file(&path);
    }
}
