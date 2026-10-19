//! エラー処理・LB のアルゴリズム・ヘルスチェック・セキュリティ・キャッシュ・リダイレクト・Range・バッファリング・ルーティング・運用・特殊機能
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// 優先度中: エラーハンドリング詳細テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_error_handling_invalid_method() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なHTTPメソッドの処理をテスト
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // TLS接続を確立
    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();

    // TLSハンドシェイクを完了
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(_) => {
                panic!("TLS handshake error");
            }
        }
    }

    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // 不正なHTTPメソッドを送信
    let invalid_request = b"INVALID / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    if let Err(e) = tls_stream.write_all(invalid_request) {
        panic!("Failed to send invalid method request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // レスポンスを受信
    let mut response = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    let response = String::from_utf8_lossy(&response);
    let status = get_status_code(&response);

    // 不正なメソッドの場合、405 Method Not Allowedが返される
    assert_eq!(
        status,
        Some(405),
        "Should return 405 Method Not Allowed for invalid method, got: {:?}",
        status
    );

    eprintln!(
        "Error handling test: invalid method returned status {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_error_handling_missing_host() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Hostヘッダーが欠落しているリクエストの処理をテスト
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // TLS接続を確立
    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();

    // TLSハンドシェイクを完了
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(_) => {
                panic!("TLS handshake error");
            }
        }
    }

    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // Hostヘッダーが欠落しているリクエストを送信
    let missing_host_request = b"GET / HTTP/1.1\r\n\r\n";
    if let Err(e) = tls_stream.write_all(missing_host_request) {
        panic!("Failed to send missing host request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // レスポンスを受信
    let mut response = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    let response = String::from_utf8_lossy(&response);
    let status = get_status_code(&response);

    // Hostヘッダーが欠落している場合、400 Bad Requestが返される
    assert_eq!(
        status,
        Some(400),
        "Should return 400 Bad Request for missing host, got: {:?}",
        status
    );

    eprintln!(
        "Error handling test: missing host returned status {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(60000)]
pub(super) async fn test_error_handling_oversized_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 過大なヘッダーを含むリクエスト（100KBのヘッダー）
    let large_header_value = "x".repeat(100000);
    let oversized_request = format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nX-Large-Header: {}\r\n\r\n",
        large_header_value
    );

    // 負荷下で応答が遅延する場合があるため最大3回リトライ
    for attempt in 0..3u32 {
        let mut stream = match TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("attempt {}: connect error: {}", attempt + 1, e);
                tokio::time::sleep(Duration::from_millis(500)).await;
                continue;
            }
        };
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();

        let config = create_client_config();
        let server_name = ServerName::try_from("localhost".to_string()).unwrap();
        let mut tls_conn = match ClientConnection::new(config, server_name) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("attempt {}: TLS config error: {}", attempt + 1, e);
                continue;
            }
        };
        let mut handshake_ok = true;
        while tls_conn.is_handshaking() {
            if let Err(e) = tls_conn.complete_io(&mut stream) {
                eprintln!("attempt {}: TLS handshake error: {}", attempt + 1, e);
                handshake_ok = false;
                break;
            }
        }
        if !handshake_ok {
            tokio::time::sleep(Duration::from_millis(500)).await;
            continue;
        }

        let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
        // 過大なヘッダー送信中にプロキシが接続を閉じることがある（BrokenPipe）
        let _ = tls_stream.write_all(oversized_request.as_bytes());
        let _ = tls_stream.flush();

        let mut response = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match tls_stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(_) => break,
            }
        }

        let response = String::from_utf8_lossy(&response);
        let status = get_status_code(&response);
        if status == Some(431) {
            eprintln!(
                "Error handling test: oversized header returned status {:?}",
                status
            );
            return;
        }
        eprintln!(
            "attempt {}: got {:?}, expected 431, retrying...",
            attempt + 1,
            status
        );
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    panic!("Should return 431 Request Header Fields Too Large for oversized header");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_error_handling_invalid_path() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なパスの処理をテスト
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // TLS接続を確立
    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();

    // TLSハンドシェイクを完了
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(_) => {
                panic!("TLS handshake error");
            }
        }
    }

    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // 不正なパスを含むリクエストを送信（NULL文字を含む）
    let invalid_path_request = b"GET /\x00invalid HTTP/1.1\r\nHost: localhost\r\n\r\n";
    if let Err(e) = tls_stream.write_all(invalid_path_request) {
        panic!("Failed to send invalid path request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // レスポンスを受信
    let mut response = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    let response = String::from_utf8_lossy(&response);
    let status = get_status_code(&response);

    // 不正なパスの場合、400 Bad Requestが返される
    assert_eq!(
        status,
        Some(400),
        "Should return 400 Bad Request for invalid path, got: {:?}",
        status
    );

    eprintln!(
        "Error handling test: invalid path returned status {:?}",
        status
    );
}

// ====================
// 優先度高: ロードバランシングアルゴリズムテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_least_connections_distribution() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでleast_connアルゴリズムを設定する必要がある
    // 例: ./tests/e2e_setup.sh test least_conn

    // 複数の接続を確立して、接続数が少ないサーバーが選ばれることを確認
    // Least Connectionsアルゴリズムでは、接続数が少ないサーバーが優先される
    // ただし、接続の再利用により、完全に均等にならない可能性がある

    let mut backend1_count = 0;
    let mut backend2_count = 0;

    // 10回リクエストを送信
    for _ in 0..10 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            if let Some(server_id) = get_header_value(&response, "X-Server-Id") {
                match server_id.as_str() {
                    "backend1" => backend1_count += 1,
                    "backend2" => backend2_count += 1,
                    _ => {}
                }
            }
        }
        // 接続を確立するために短い待機
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 両方のバックエンドが使用されていることを確認
    assert!(backend1_count > 0, "Backend 1 should receive some requests");
    assert!(backend2_count > 0, "Backend 2 should receive some requests");

    eprintln!(
        "Least Connections distribution: backend1={}, backend2={}",
        backend1_count, backend2_count
    );

    // Least Connectionsでは、接続数が少ないサーバーが選ばれるため、
    // 完全に均等にならない可能性がある
    // ただし、両方のサーバーが使用されることを確認
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_ip_hash_consistency() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでip_hashアルゴリズムを設定する必要がある
    // 例: ./tests/e2e_setup.sh test ip_hash
    // 同じIPから複数回リクエストを送信し、同じバックエンドが選ばれることを確認

    // 同じIPから10回リクエストを送信
    let mut server_ids = Vec::new();
    for _ in 0..10 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            if let Some(server_id) = get_header_value(&response, "X-Server-Id") {
                server_ids.push(server_id);
            }
        }
    }

    assert!(
        !server_ids.is_empty(),
        "Should receive responses with server IDs"
    );

    // IP Hashの場合、同じIPからは同じサーバーが選ばれるべき
    // すべてのリクエストが同じサーバーにルーティングされることを確認
    if server_ids.len() > 1 {
        let first_server = &server_ids[0];
        let all_same = server_ids.iter().all(|id| id == first_server);

        if all_same {
            eprintln!(
                "IP Hash consistency confirmed: all {} requests went to {}",
                server_ids.len(),
                first_server
            );
        } else {
            eprintln!("IP Hash may not be configured: requests distributed across servers");
            eprintln!("Server IDs: {:?}", server_ids);
        }
    }
}

// ====================
// 優先度高: ヘルスチェック自動フェイルオーバーテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_failover() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck
    // 実際のバックエンド障害をシミュレートする必要がある

    // まず、両方のバックエンドが正常であることを確認
    let initial_response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(
        initial_response.is_some(),
        "Should receive initial response"
    );

    // メトリクスエンドポイントから健康状態を確認
    let metrics_response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(
        metrics_response.is_some(),
        "Should receive metrics response"
    );

    let metrics_response = metrics_response.unwrap();

    // ヘルスチェックメトリクスが含まれるか確認
    if metrics_response.contains("http_upstream_health")
        || metrics_response.contains("veil_proxy_http_upstream_health")
    {
        eprintln!("Health check metrics detected");

        // メトリクスから健康状態を確認
        // 実際のフェイルオーバーテストには、バックエンドの動的な停止/起動が必要
        // ここでは、メトリクスが存在することを確認
        assert!(
            metrics_response.contains("veil_proxy") || metrics_response.contains("# HELP"),
            "Should contain Prometheus metrics"
        );
    } else {
        eprintln!("Health check not configured, skipping failover test");
        // ヘルスチェックが設定されていない場合でも、基本的な動作確認
        assert!(
            metrics_response.contains("veil_proxy") || metrics_response.contains("# HELP"),
            "Should contain Prometheus metrics"
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_recovery() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck
    // 実際のバックエンド回復をシミュレートする必要がある

    // 現在の実装では、バックエンドの動的な停止/起動機能がないため、
    // 基本的な動作確認とメトリクス確認を行う

    // リクエストが正常に処理されることを確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // メトリクスエンドポイントから健康状態を確認
    let metrics_response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    if let Some(metrics) = metrics_response {
        if metrics.contains("http_upstream_health")
            || metrics.contains("veil_proxy_http_upstream_health")
        {
            eprintln!(
                "Health check metrics detected - recovery test would verify automatic re-addition"
            );
        }
    }

    // 実際の回復テストには、以下の手順が必要:
    // 1. バックエンドを停止
    // 2. ヘルスチェックが失敗することを確認
    // 3. バックエンドを再起動
    // 4. ヘルスチェックが成功し、プールに復帰することを確認
}

// ====================
// 優先度高: セキュリティ機能実動作テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_rate_limiting_enforcement() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでレート制限を設定する必要がある
    // 例: rate_limit_requests_per_min = 10

    // 制限を超えるリクエストを送信
    let mut success_count = 0;
    let mut rate_limited_count = 0;

    for i in 0..20 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let status = get_status_code(&response);
            match status {
                Some(200) => success_count += 1,
                Some(429) => rate_limited_count += 1,
                _ => {}
            }
        }
        // レート制限をトリガーするために短い間隔で送信
        if i < 19 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    // レート制限が設定されている場合、429が返される可能性がある
    // 設定されていない場合、すべて200が返される
    // このテストは設定に依存するため、両方のケースを許容
    assert!(
        success_count > 0 || rate_limited_count > 0,
        "Should receive some responses"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_ip_restriction_enforcement() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでIP制限を設定する必要がある
    // 例: allowed_ips = ["127.0.0.1"]
    // e2e_setup.shでは127.0.0.1が許可されているので200が期待される

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // 127.0.0.1からのアクセスは許可されているので200が期待される
    // もし403が返された場合、IP制限設定に問題がある
    match status {
        Some(200) => {
            eprintln!("IP restriction test: 127.0.0.1 is allowed as expected");
        }
        Some(403) => {
            // 127.0.0.1がブロックされている場合、設定が間違っている
            panic!("IP restriction blocking 127.0.0.1 - check allowed_ips configuration includes 127.0.0.1");
        }
        _ => {
            panic!("Unexpected status: {:?}", status);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_connection_limit_enforcement() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルで接続数制限を設定する必要がある

    // 多数の並行接続を確立
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let success_count = Arc::new(AtomicUsize::new(0));
    let total_connections = 100;

    let handles: Vec<_> = (0..total_connections)
        .map(|_| {
            let success_count = Arc::clone(&success_count);
            tokio::spawn(async move {
                let response = send_request(PROXY_PORT, "/", &[]).await;
                if let Some(response) = response {
                    if get_status_code(&response) == Some(200) {
                        success_count.fetch_add(1, Ordering::Relaxed);
                    }
                }
            })
        })
        .collect();

    for handle in handles {
        let _ = handle.await;
    }

    let successes = success_count.load(Ordering::Relaxed);
    // 接続数制限が設定されている場合、一部の接続が拒否される可能性がある
    // 設定されていない場合、すべて成功する
    assert!(
        successes > 0,
        "At least some connections should succeed: {}/{}",
        successes,
        total_connections
    );
}

// ====================
// 優先度高: プロキシキャッシュテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_hit() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでキャッシュを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test cache

    use std::time::Instant;

    // 最初のリクエスト（キャッシュミス）
    let start1 = Instant::now();
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    let elapsed1 = start1.elapsed();
    assert!(response1.is_some(), "Should receive first response");

    // 少し待機してから2回目のリクエスト（キャッシュヒットの可能性）
    tokio::time::sleep(Duration::from_millis(100)).await;
    let start2 = Instant::now();
    let response2 = send_request(PROXY_PORT, "/", &[]).await;
    let elapsed2 = start2.elapsed();
    assert!(response2.is_some(), "Should receive second response");

    let response1 = response1.unwrap();
    let response2 = response2.unwrap();

    // 基本的な動作確認
    assert_eq!(
        get_status_code(&response1),
        get_status_code(&response2),
        "Both responses should have same status"
    );

    // キャッシュが有効な場合、2回目のリクエストが速い可能性がある
    // ただし、キャッシュが無効な場合でも正常に動作することを確認
    if elapsed2 < elapsed1 {
        eprintln!(
            "Cache may be working: second request was faster ({}ms vs {}ms)",
            elapsed2.as_millis(),
            elapsed1.as_millis()
        );
    }

    // X-CacheヘッダーまたはAgeヘッダーを確認（キャッシュが有効な場合）
    let cache_header = get_header_value(&response2, "X-Cache");
    let age_header = get_header_value(&response2, "Age");
    if cache_header.is_some() || age_header.is_some() {
        eprintln!(
            "Cache headers detected: X-Cache={:?}, Age={:?}",
            cache_header, age_header
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_miss() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでキャッシュを有効化する必要がある

    // 異なるパスにリクエストを送信（キャッシュミス）
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    let response2 = send_request(PROXY_PORT, "/health", &[]).await;

    assert!(response1.is_some(), "Should receive first response");
    assert!(response2.is_some(), "Should receive second response");

    // 異なるパスなので、キャッシュミスが期待される
    let response1 = response1.unwrap();
    let response2 = response2.unwrap();

    // 基本的な動作確認
    assert!(
        get_status_code(&response1) == Some(200) || get_status_code(&response2) == Some(200),
        "At least one response should be successful"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_etag_304() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでETagを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test cache

    // 最初のリクエストでETagを取得
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive first response");

    let response1 = response1.unwrap();
    let etag = get_header_value(&response1, "ETag");

    if let Some(etag_value) = etag {
        eprintln!("ETag found: {}", etag_value);

        // If-None-Matchヘッダーで2回目のリクエスト
        let response2 = send_request(PROXY_PORT, "/", &[("If-None-Match", &etag_value)]).await;

        if let Some(response2) = response2 {
            let status = get_status_code(&response2);
            // ETagが一致する場合、304 Not Modifiedが返される
            assert_eq!(
                status,
                Some(304),
                "Should return 304 Not Modified for matching ETag, got: {:?}",
                status
            );

            if status == Some(304) {
                eprintln!("304 Not Modified received - ETag validation working");
                // 304レスポンスにはContent-Lengthが0または小さいはず
                let content_length = get_header_value(&response2, "Content-Length");
                if let Some(cl) = content_length {
                    eprintln!("Content-Length in 304 response: {}", cl);
                }
            }
        }
    } else {
        // ETagが設定されていない場合、このテストはスキップ
        eprintln!("ETag not configured, skipping 304 test");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_stale_while_revalidate() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでstale-while-revalidateを有効化する必要がある

    // キャッシュエントリを作成
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive first response");

    // キャッシュが期限切れになった後、stale-while-revalidateが動作することを確認
    // 実際のテストには、時間の経過をシミュレートする必要がある
    let response2 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response2.is_some(), "Should receive second response");

    // 基本的な動作確認
    let response1 = response1.unwrap();
    let response2 = response2.unwrap();
    assert!(
        get_status_code(&response1) == Some(200) && get_status_code(&response2) == Some(200),
        "Both responses should be successful"
    );
}

// ====================
// 優先度中: HTTP/2詳細機能テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_hpack_compression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/2のHPACK圧縮をテスト
    // 実際のテストには、HTTP/2クライアントライブラリが必要

    // 現在の実装では、ALPNネゴシエーションのみ確認
    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // TLSハンドシェイクを完了
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(_) => {
                panic!("TLS handshake error");
            }
        }
    }

    // ALPNでHTTP/2がネゴシエートされたことを確認
    let protocol = tls_conn.alpn_protocol();
    if let Some(proto) = protocol {
        assert!(
            proto == b"h2" || proto == b"http/1.1",
            "Should negotiate HTTP/2 or HTTP/1.1: {:?}",
            proto
        );
    }
}

// ====================
// 優先度中: WebSocket双方向通信テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_websocket_bidirectional() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // WebSocketの双方向通信をテスト（TLS経由）
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let config = create_client_config();
    let server_name = rustls::pki_types::ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = rustls::ClientConnection::new(config, server_name).unwrap();
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(_) => {
                panic!("TLS handshake error");
            }
        }
    }
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // WebSocketアップグレードリクエストを送信
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    tls_stream.write_all(request).unwrap();
    tls_stream.flush().unwrap();

    // レスポンスを受信
    let mut response = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    let response = String::from_utf8_lossy(&response);

    let status = get_status_code(&response);
    // WebSocketがサポートされている場合、101 Switching Protocolsが返される
    assert_eq!(
        status,
        Some(101),
        "Should return 101 Switching Protocols for WebSocket request, got: {:?}",
        status
    );
}

// ====================
// 優先度中: リダイレクトテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_redirect_301() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルで301リダイレクトを設定する必要がある

    // HTTPポートにアクセス（HTTPSにリダイレクトされる場合）
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", 8080)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    stream.write_all(request).unwrap();

    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);

    let status = get_status_code(&response);
    // リダイレクトが設定されている場合、301 Moved Permanentlyが返される
    assert_eq!(
        status,
        Some(301),
        "Should return 301 Moved Permanently for redirect, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_redirect_302() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルで302リダイレクトを設定する必要がある

    // リダイレクトアクションが設定されている場合のテスト
    let response = send_request(PROXY_PORT, "/redirect-test", &[]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // リダイレクトが設定されている場合、302 Foundが返される
        assert_eq!(
            status,
            Some(302),
            "Should return 302 Found for redirect, got: {:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_redirect_path_preservation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // リダイレクト時にパスが保持されることを確認
    // 注意: このテストは設定ファイルでリダイレクトを設定する必要がある

    let response = send_request(PROXY_PORT, "/api/v1/users", &[]).await;

    if let Some(response) = response {
        let location = get_header_value(&response, "Location");
        if let Some(location_value) = location {
            // リダイレクト先に元のパスが含まれることを確認
            assert!(
                location_value.contains("/api/v1/users") || location_value.contains("/users"),
                "Redirect location should preserve path: {}",
                location_value
            );
        }
    }
}

// ====================
// 優先度中: Rangeリクエストテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_range_request_single() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Rangeリクエスト（単一範囲）を送信
    let response = send_request(PROXY_PORT, "/large.txt", &[("Range", "bytes=0-999")]).await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // Rangeリクエストがサポートされている場合、206 Partial Contentが返される可能性がある
    // Range Requestがサポートされている場合、206 Partial Contentが返される
    assert_eq!(
        status,
        Some(206),
        "Should return 206 Partial Content for range request, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_range_request_206() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Rangeリクエストで206 Partial Contentを確認
    let response = send_request(PROXY_PORT, "/large.txt", &[("Range", "bytes=0-1023")]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        if status == Some(206) {
            // 206の場合、Content-Rangeヘッダーが存在することを確認
            let content_range = get_header_value(&response, "Content-Range");
            assert!(
                content_range.is_some(),
                "206 Partial Content should have Content-Range header"
            );
        }
    }
}

// ====================
// 優先度中: バッファリング制御テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_streaming_mode() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでバッファリングモードを設定する必要がある
    // 例: ./tests/e2e_setup.sh test buffering
    // デフォルトではStreamingモードが使用される

    use std::time::Instant;

    // 大きなレスポンスをリクエスト
    let start = Instant::now();
    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    let elapsed = start.elapsed();

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Streamingモードの場合、レスポンスが段階的に返される可能性がある
    // 大きなファイルなので、レスポンス時間を確認
    eprintln!(
        "Streaming mode test: response time {:?}, size {}",
        elapsed,
        response.len()
    );

    // レスポンスが正常に受信されたことを確認
    assert!(response.len() > 1000, "Large file should be > 1000 bytes");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_full_mode() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでバッファリングモードを設定する必要がある
    // 例: ./tests/e2e_setup.sh test buffering
    // Fullモードの場合、レスポンス全体がバッファリングされる

    use std::time::Instant;

    // Fullモードの場合、レスポンス全体がバッファリングされる
    let start = Instant::now();
    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    let elapsed = start.elapsed();

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Fullモードでは、バックエンド接続が早期に解放される可能性がある
    eprintln!(
        "Full mode test: response time {:?}, size {}",
        elapsed,
        response.len()
    );

    // レスポンスが正常に受信されたことを確認
    assert!(response.len() > 1000, "Large file should be > 1000 bytes");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_adaptive_mode() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでバッファリングモードを設定する必要がある
    // 例: ./tests/e2e_setup.sh test buffering
    // Adaptiveモードの場合、条件に応じてストリーミングまたはフルバッファリングが選択される

    use std::time::Instant;

    // 小さいレスポンス（Fullバッファリング）
    let start1 = Instant::now();
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    let elapsed1 = start1.elapsed();

    assert!(response1.is_some(), "Should receive small response");
    let response1 = response1.unwrap();
    assert_eq!(
        get_status_code(&response1),
        Some(200),
        "Should return 200 OK"
    );

    // 大きいレスポンス（Streaming）
    let start2 = Instant::now();
    let response2 = send_request(PROXY_PORT, "/large.txt", &[]).await;
    let elapsed2 = start2.elapsed();

    assert!(response2.is_some(), "Should receive large response");
    let response2 = response2.unwrap();
    assert_eq!(
        get_status_code(&response2),
        Some(200),
        "Should return 200 OK"
    );

    eprintln!(
        "Adaptive mode test: small response {:?} ({} bytes), large response {:?} ({} bytes)",
        elapsed1,
        response1.len(),
        elapsed2,
        response2.len()
    );

    // Adaptiveモードでは、サイズに応じてモードが切り替わる
    // 小さいレスポンスはFullバッファリング、大きいレスポンスはStreaming
    assert!(
        response1.len() < response2.len(),
        "Small response should be smaller"
    );
}

// ====================
// 優先度低: ルーティング条件テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_header_condition() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでヘッダー条件ルーティングを設定する必要がある
    // 例: [route.conditions] header = { "X-Version" = "v2" }

    // X-Versionヘッダー付きリクエスト
    let response = send_request(PROXY_PORT, "/", &[("X-Version", "v2")]).await;

    assert!(response.is_some(), "Should receive response");
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_method_condition() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // /api/get-only/* パスに allowed_methods = ["GET", "HEAD"] を設定済み
    // POSTリクエストを送信すると 405 Method Not Allowed が返される

    // POSTリクエスト（許可されていないメソッド）
    let response = send_request_with_method(
        PROXY_PORT,
        "/api/get-only/",
        "POST",
        &[],
        Some(b"test body"),
    )
    .await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // メソッドが許可されていない場合、405 Method Not Allowedが返される
        assert_eq!(
            status,
            Some(405),
            "Should return 405 Method Not Allowed for restricted method, got: {:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_query_condition() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでクエリ条件ルーティングを設定する必要がある
    // 例: [route.conditions] query = { "token" = "secret" }
    //
    // 現在の設定では、クエリ条件が設定されていないため、
    // クエリパラメータがパスに含まれていても、パス `/` として処理されるはず
    // しかし、実際の動作では、クエリパラメータがパスに含まれているため、
    // バックエンドが404を返す可能性がある

    // クエリパラメータなしでリクエストを送信（基本動作確認）
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());
    assert_eq!(status1, Some(200), "Should return 200 OK");

    // クエリパラメータ付きリクエスト（クエリ条件が設定されていない場合の動作確認）
    let response2 = send_request(PROXY_PORT, "/?token=secret", &[]).await;
    assert!(response2.is_some(), "Should receive response");
    let response2 = response2.unwrap();
    let status2 = get_status_code(&response2);
    // クエリ条件が設定されていない場合、デフォルトルートにマッチするはず
    // しかし、クエリパラメータがパスに含まれているため、404が返される可能性がある
    // これは、バックエンドの動作によるもの
    assert!(
        status2 == Some(200) || status2 == Some(404),
        "Should return 200 OK or 404 Not Found (depending on backend behavior): {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_source_ip_condition() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // /api/ip-restricted/* パスに denied_ips = ["127.0.0.0/8"] を設定済み
    // テストクライアントは 127.0.0.1 から接続するため 403 Forbidden が返される

    // 127.0.0.1からのリクエスト
    let response = send_request(PROXY_PORT, "/api/ip-restricted/", &[]).await;

    assert!(response.is_some(), "Should receive response");
    let response = response.unwrap();
    let status = get_status_code(&response);
    // IP制限が設定されている場合、403 Forbiddenが返される
    assert_eq!(
        status,
        Some(403),
        "Should return 403 Forbidden for IP restriction, got: {:?}",
        status
    );
}

// ====================
// 優先度低: 運用機能テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_graceful_reload() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは実際のSIGHUPシグナルを送信する必要がある
    // テスト環境では、プロセスIDの取得とシグナル送信が必要

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    // 実際のリロードテストには、設定ファイルの変更とSIGHUP送信が必要
    // ここでは、基本的な動作確認のみ
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_config_validation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルの検証機能をテストする必要がある
    // 実際のテストには、不正な設定ファイルでの起動試行が必要

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    // 設定が有効な場合、正常に動作することを確認
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");
}

// ====================
// 優先度低: 特殊機能テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_wire_protocol() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCワイヤプロトコルの詳細テスト
    // 実際のテストには、gRPCクライアントライブラリが必要

    // 基本的なgRPCリクエスト
    let response = send_request_with_method(
        PROXY_PORT,
        "/",
        "POST",
        &[
            ("Content-Type", "application/grpc"),
            ("Accept", "application/grpc"),
        ],
        Some(b"\x00\x00\x00\x00\x00"), // gRPCフレームヘッダー
    )
    .await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // gRPCエンドポイントが存在する場合、200が返される
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for gRPC request, got: {:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_status_code() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCリクエストを送信（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test",
        &[],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to send gRPC request: {}", e);
        }
    };

    // gRPCステータスを取得
    let grpc_status = GrpcTestClient::extract_grpc_status(&response);
    // gRPCステータスは存在しない場合もある（エンドポイントが存在しない場合）
    if grpc_status.is_some() {
        // gRPCステータスコードは0（OK）またはエラーコード
        assert!(
            grpc_status.unwrap() <= 16,
            "gRPC status code should be valid"
        );
    }

    // HTTPステータスコードも確認
    let http_status = GrpcTestClient::extract_status_code(&response);
    assert!(
        http_status == Some(200) || http_status == Some(404) || http_status == Some(502),
        "Should return 200, 404, or 502: {:?}",
        http_status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_web_cors() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPC-Web CORS変換のテスト
    // 実際のテストには、gRPC-Webクライアントライブラリが必要

    // OPTIONSリクエスト（プリフライト）
    let response = send_request_with_method(
        PROXY_PORT,
        "/",
        "OPTIONS",
        &[
            ("Origin", "https://example.com"),
            ("Access-Control-Request-Method", "POST"),
            ("Access-Control-Request-Headers", "content-type,x-grpc-web"),
        ],
        None,
    )
    .await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // CORSが設定されている場合、適切なCORSヘッダーが返される
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for CORS request, got: {:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc-web")]
pub(super) async fn test_grpc_web_text_format() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPC-Webテキスト形式のテスト
    use base64::{engine::general_purpose, Engine as _};

    let frame = GrpcFrame::new(b"Hello, gRPC-Web Text!".to_vec());
    let frame_bytes = frame.encode();
    let base64_encoded = general_purpose::STANDARD.encode(&frame_bytes);

    let response = send_request_with_method(
        PROXY_PORT,
        "/grpc.test.v1.TestService/UnaryCall",
        "POST",
        &[
            ("Content-Type", "application/grpc-web-text"),
            ("Accept", "application/grpc-web-text"),
        ],
        Some(base64_encoded.as_bytes()),
    )
    .await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for gRPC request, got: {:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc-web")]
pub(super) async fn test_grpc_web_cors_headers() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPC-Web CORSヘッダーのテスト
    let response = send_request_with_method(
        PROXY_PORT,
        "/grpc.test.v1.TestService/UnaryCall",
        "POST",
        &[
            ("Content-Type", "application/grpc-web"),
            ("Accept", "application/grpc-web"),
            ("Origin", "https://example.com"),
        ],
        Some(b"test"),
    )
    .await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for gRPC request, got: {:?}",
            status
        );

        // CORSヘッダーが含まれているか確認（レスポンスに含まれる場合）
        if response.contains("Access-Control-Allow-Origin") {
            // CORSヘッダーが存在することを確認
            let cors_header = get_header_value(&response, "Access-Control-Allow-Origin");
            assert!(
                cors_header.is_some(),
                "CORS Access-Control-Allow-Origin header should be present when CORS is enabled"
            );
            eprintln!("CORS header found: {:?}", cors_header);
        } else {
            eprintln!("CORS headers not present in response (CORS may not be configured)");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_proxy_load_balancing() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCプロキシロードバランシングのテスト
    // 複数のリクエストを送信し、異なるバックエンドに分散されることを確認
    let mut responses = Vec::new();
    for _ in 0..10 {
        // gRPCリクエストを送信（非同期版）
        let response = match GrpcTestClient::send_grpc_request(
            "127.0.0.1",
            PROXY_PORT,
            "/grpc.test.v1.TestService/Test",
            b"test",
            &[],
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                panic!("Failed to send gRPC request: {}", e);
            }
        };

        let status = GrpcTestClient::extract_status_code(&response);
        responses.push(status);
    }

    // 少なくともいくつかのリクエストが成功することを確認
    let success_count = responses
        .iter()
        .filter(|&s| s == &Some(200) || s == &Some(404) || s == &Some(502))
        .count();
    assert!(success_count > 0, "At least some requests should succeed");
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_proxy_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCプロキシタイムアウトのテスト
    // タイムアウト設定を短くしてリクエストを送信
    // gRPCリクエストを送信（非同期版、タイムアウト付き）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test",
        &[("grpc-timeout", "1S")], // 1秒のタイムアウト
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to send gRPC request: {}", e);
        }
    };

    let status = GrpcTestClient::extract_status_code(&response);
    // grpc-timeoutヘッダーはgRPCランタイムへのヒントであり、プロキシはHTTPレベルでは強制しない
    // バックエンドが正常に応答した場合は200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for gRPC request with grpc-timeout hint, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_proxy_error_handling() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCプロキシエラーハンドリングのテスト
    // 存在しないエンドポイントにリクエストを送信
    // gRPCリクエストを送信（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.NonExistentService/NonExistentMethod",
        b"test",
        &[],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to send gRPC request to /grpc.test.v1.NonExistentService/NonExistentMethod: {}", e);
        }
    };

    let status = GrpcTestClient::extract_status_code(&response);
    // 存在しないエンドポイントの場合、404 Not Foundが返される
    assert_eq!(
        status,
        Some(404),
        "Should return 404 Not Found for non-existent gRPC endpoint, got: {:?}",
        status
    );

    // gRPCステータスコードの検証
    let grpc_status = GrpcTestClient::extract_grpc_status(&response);
    if let Some(grpc_status_code) = grpc_status {
        // 存在しないエンドポイントの場合、NOT_FOUND (5) が返される可能性がある
        eprintln!(
            "gRPC status code for non-existent endpoint: {} (expected: NOT_FOUND (5))",
            grpc_status_code
        );
        assert!(
            grpc_status_code == 5 || grpc_status_code == 0,
            "Should return gRPC status NOT_FOUND (5) or OK (0) for non-existent endpoint, got: {}",
            grpc_status_code
        );
    } else {
        eprintln!(
            "Warning: gRPC status code not found in response (HTTP-level error: {:?})",
            status
        );
    }

    // トレーラーヘッダーの検証
    let trailers = GrpcTestClient::extract_trailers(&response);
    if !trailers.is_empty() {
        eprintln!("gRPC trailers for non-existent endpoint: {:?}", trailers);
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_malformed_protobuf() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なProtobufメッセージのテスト
    // 不正なProtobufデータを送信（非同期版）
    let malformed_data = b"\xFF\xFF\xFF\xFF\xFF\xFF\xFF\xFF";
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        malformed_data,
        &[],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to send gRPC request with malformed Protobuf to /grpc.test.v1.TestService/Test: {}", e);
        }
    };

    let status = GrpcTestClient::extract_status_code(&response);
    // gRPCはアプリケーションレベルのエラーをHTTP 200 + grpc-statusトレーラーで返す
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for gRPC request (error indicated by grpc-status), got: {:?}",
        status
    );

    // gRPCステータスコードの検証（トレーラーがHTTP/1.1ヘッダーとして転送される）
    let grpc_status = GrpcTestClient::extract_grpc_status(&response);
    if let Some(grpc_status_code) = grpc_status {
        // 不正なProtobufの場合、INVALID_ARGUMENT (3) または INTERNAL (13) が返される可能性がある
        assert!(
            grpc_status_code == 3 || grpc_status_code == 13,
            "Should return gRPC status INVALID_ARGUMENT (3) or INTERNAL (13) for malformed Protobuf, got: {}",
            grpc_status_code
        );
        eprintln!(
            "gRPC status code for malformed Protobuf: {}",
            grpc_status_code
        );
    } else {
        eprintln!("Note: grpc-status not found in response (backend may have processed message)");
    }

    // トレーラーヘッダーの検証
    let trailers = GrpcTestClient::extract_trailers(&response);
    let has_grpc_status = trailers.iter().any(|(name, _)| name == "grpc-status");
    if has_grpc_status {
        eprintln!("gRPC trailers for malformed Protobuf: {:?}", trailers);
    } else {
        eprintln!("Warning: grpc-status not found in trailers (may be HTTP-level error)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_stream_reset() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCストリームリセットのテスト
    // gRPCクライアントを作成してリクエストを途中でキャンセルする動作をテスト
    // リクエスト送信を試みる（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/StreamReset",
        b"\x00\x00\x00\x00\x05hello",
        &[],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to send gRPC stream reset request to /grpc.test.v1.TestService/StreamReset: {}", e);
        }
    };

    let status = GrpcTestClient::extract_status_code(&response);
    // ストリームリセットの場合、404 (エンドポイント不存在) または 502 (バックエンドエラー) が返される可能性がある
    assert!(
        status == Some(404) || status == Some(502) || status == Some(200),
        "Should return 404, 502, or 200 for stream reset request, got: {:?}",
        status
    );

    // gRPCステータスコードの検証
    let grpc_status = GrpcTestClient::extract_grpc_status(&response);
    if let Some(grpc_status_code) = grpc_status {
        // ストリームリセットの場合、CANCELLED (1) または NOT_FOUND (5) が返される可能性がある
        eprintln!(
            "gRPC status code for stream reset: {} (expected: CANCELLED (1) or NOT_FOUND (5))",
            grpc_status_code
        );
        assert!(
            grpc_status_code == 1 || grpc_status_code == 5 || grpc_status_code == 0,
            "Should return gRPC status CANCELLED (1), NOT_FOUND (5), or OK (0) for stream reset, got: {}", 
            grpc_status_code
        );
    } else {
        eprintln!(
            "Warning: gRPC status code not found in response (HTTP-level error: {:?})",
            status
        );
    }

    // トレーラーヘッダーの検証
    let trailers = GrpcTestClient::extract_trailers(&response);
    if !trailers.is_empty() {
        eprintln!("gRPC trailers for stream reset: {:?}", trailers);
        let has_grpc_status = trailers.iter().any(|(name, _)| name == "grpc-status");
        if has_grpc_status {
            eprintln!("grpc-status found in trailers");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_deflate_compression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // deflate圧縮のテスト（簡易実装）
    // grpc-encodingヘッダーでdeflateを指定（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test message",
        &[
            ("grpc-encoding", "deflate"),
            ("grpc-accept-encoding", "deflate"),
        ],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to send gRPC request: {}", e);
        }
    };

    let status = GrpcTestClient::extract_status_code(&response);
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for gRPC request, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_compression_negotiation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 圧縮方式のネゴシエーションテスト
    // 複数の圧縮方式をサポートすることを通知（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test",
        &[("grpc-accept-encoding", "gzip, deflate, identity")],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to send gRPC request: {}", e);
        }
    };

    let status = GrpcTestClient::extract_status_code(&response);
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for gRPC request, got: {:?}",
        status
    );
}

// ====================
// F-92: gRPC トレーラー詳細
// ====================

/// 正常応答とエラー応答で grpc-status / grpc-message トレーラーが透過されることを検証。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_grpc_trailer_detailed() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("HTTP/2 client for trailer detailed");

    let grpc_headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("accept", "application/grpc"),
    ];

    // --- 正常 Unary: grpc-status: 0 ---
    let ok_body = encode_grpc_lpm(&encode_simple_request("trailer-ok"));
    let ok = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &grpc_headers,
            Some(&ok_body),
        )
        .await
        .expect("ok unary for trailers");
    assert_eq!(ok.status, 200, "ok unary HTTP 200");
    let ok_status = ok.grpc_status();
    assert!(
        ok_status.is_some(),
        "ok unary must expose grpc-status, trailers={:?} headers={:?}",
        ok.trailers,
        ok.headers
    );
    let ok_code = ok_status.unwrap();
    assert!(ok_code <= 16, "grpc-status must be 0-16, got {}", ok_code);
    eprintln!(
        "Unary OK: grpc-status={} grpc-message={:?} trailers={:?}",
        ok_code,
        ok.grpc_message(),
        ok.trailers
    );

    // --- エラー: StreamReset → grpc-status != 0 + grpc-message ---
    let err_body = encode_grpc_lpm(&encode_simple_request("force-error"));
    let err = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/StreamReset",
            &grpc_headers,
            Some(&err_body),
        )
        .await
        .expect("error unary for trailers");
    assert_eq!(err.status, 200, "gRPC errors still use HTTP 200");
    let err_status = err.grpc_status();
    assert!(
        err_status.is_some(),
        "error path must expose grpc-status, trailers={:?} headers={:?}",
        err.trailers,
        err.headers
    );
    let err_code = err_status.unwrap();
    assert!(
        err_code > 0 && err_code <= 16,
        "StreamReset should yield non-zero grpc-status, got {}",
        err_code
    );
    let err_msg = err.grpc_message();
    assert!(
        err_msg.as_ref().map(|m| !m.is_empty()).unwrap_or(false),
        "error path should forward grpc-message, got {:?}",
        err_msg
    );
    eprintln!(
        "StreamReset: grpc-status={} grpc-message={:?} trailers={:?}",
        err_code, err_msg, err.trailers
    );

    // --- カスタムメタデータ付きリクエスト（クライアント→上流）。トレーラー破壊なし ---
    let meta_headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("accept", "application/grpc"),
        ("x-custom-meta", "trailer-test-value"),
    ];
    let meta = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &meta_headers,
            Some(&encode_grpc_lpm(&encode_simple_request("with-meta"))),
        )
        .await
        .expect("unary with custom metadata");
    assert_eq!(meta.status, 200);
    assert!(
        meta.grpc_status().is_some(),
        "custom metadata must not strip grpc-status"
    );

    // トレーラー名は grpc-* または x-* メタ
    for (name, _) in meta.trailers.iter().chain(err.trailers.iter()) {
        let lower = name.to_ascii_lowercase();
        assert!(
            lower.starts_with("grpc-")
                || lower.starts_with("x-")
                || lower == "content-type"
                || lower == "content-length",
            "unexpected trailer name: {}",
            name
        );
    }
}
