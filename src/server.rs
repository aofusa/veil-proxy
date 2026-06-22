//! サーバーライフサイクル管理モジュール
//!
//! シグナルハンドラ、バックグラウンドスレッド、リスナーソケットの作成を担当します。

use crate::config::*;
use crate::runtime::io::{AsyncReadRent, AsyncWriteRent, AsyncWriteRentExt};
use crate::runtime::tcp::{TcpListener, TcpStream};
use crate::runtime::time::timeout;
use ftlog::{debug, error, info, warn};
use std::io;
use std::net::SocketAddr;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::http_utils::*;
use crate::metrics::*;
use crate::pool::*;
use crate::system::*;
use crate::upstream::*;

use crate::cache;
#[cfg(unix)]
use std::os::unix::io::AsRawFd;

// log_ktls_status は crate::logging モジュールに移動しました。

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
pub fn spawn_reload_thread() {
    thread::spawn(move || {
        info!("Configuration reload thread started");

        loop {
            thread::sleep(Duration::from_millis(500));

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
pub fn spawn_tls_reloader(mut reloader: crate::tls_reload::TlsCertReloader, interval_secs: u64) {
    thread::spawn(move || {
        info!("TLS certificate reload thread started");
        let mut elapsed: u64 = 0;
        loop {
            thread::sleep(Duration::from_millis(500));
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
    headers: Vec<(Box<[u8]>, Box<[u8]>)>,
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
            if path_str.starts_with(prefix_str) {
                std::borrow::Cow::Borrowed(&path_str[prefix_str.len()..])
            } else {
                std::borrow::Cow::Borrowed(path_str)
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
pub fn spawn_cache_cleanup_thread() {
    thread::spawn(move || {
        info!("Cache cleanup thread started (interval=60s)");

        loop {
            // 60秒ごとにクリーンアップを実行
            thread::sleep(Duration::from_secs(60));

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
            thread::sleep(tick_interval);

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

                    // Deliver response to WASM module
                    let _ = wasm_engine.on_http_call_response(
                        &pending.module_name,
                        pending.token,
                        response,
                    );
                }
            }
        }
    });
}

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

                        // 同期的な TCP 接続でヘルスチェック
                        let check_result = perform_health_check(
                            &addr,
                            &target.host,
                            &hc_config.path,
                            hc_config.use_tls,
                            hc_config.verify_cert,
                            Duration::from_secs(hc_config.timeout_secs),
                            &hc_config.healthy_statuses,
                        );

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
                thread::sleep(Duration::from_millis(500));
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
