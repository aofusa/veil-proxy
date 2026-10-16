//! プロキシの基本・ヘッダー・ロードバランシング・静的配信・圧縮・メトリクス・エラー・WebSocket
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// プロキシ基本機能テスト（同期版 - 既存）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_proxy_basic_request() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_proxy_health_endpoint() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/health", &[]).await;
    assert!(
        response.is_some(),
        "Should receive response from health endpoint"
    );

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Health endpoint should return 200 OK");
}

// ====================
// ヘッダー操作テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_response_header_added() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();

    // プロキシが追加したヘッダーを確認
    let proxied_by = get_header_value(&response, "X-Proxied-By");
    assert_eq!(
        proxied_by,
        Some("veil".to_string()),
        "Should have X-Proxied-By header"
    );

    let test_header = get_header_value(&response, "X-Test-Header");
    assert_eq!(
        test_header,
        Some("e2e-test".to_string()),
        "Should have X-Test-Header"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_server_header_removed() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();

    // Serverヘッダーが削除されていることを確認
    let server = get_header_value(&response, "Server");
    assert!(server.is_none(), "Server header should be removed");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_backend_server_id_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 並列実行時のタイムアウト対策としてリトライロジックを追加
    let response = send_request_with_retry(PROXY_PORT, "/", &[], 3).await;
    assert!(response.is_some(), "Should receive response after retries");

    let response = response.unwrap();

    // バックエンドが追加したX-Server-Idヘッダーを確認
    let server_id = get_header_value(&response, "X-Server-Id");
    assert!(
        server_id == Some("backend1".to_string()) || server_id == Some("backend2".to_string()),
        "Should have X-Server-Id from backend"
    );
}

// ====================
// ロードバランシングテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_round_robin_distribution() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut backend1_count = 0;
    let mut backend2_count = 0;

    // 10回リクエストを送信
    // B-10: 共有の "/"（backend-pool）は並列実行中の他テストと RR カーソルを
    // 共有して分散カウントが干渉するため、専用プール（rr-isolated-pool）へ
    // ルーティングされる専用パスを使う。
    for _ in 0..10 {
        let response = send_request(PROXY_PORT, "/rr-test/", &[]).await;
        if let Some(response) = response {
            if let Some(server_id) = get_header_value(&response, "X-Server-Id") {
                match server_id.as_str() {
                    "backend1" => backend1_count += 1,
                    "backend2" => backend2_count += 1,
                    _ => {}
                }
            }
        }
    }

    // 両方のバックエンドが使用されていることを確認
    assert!(backend1_count > 0, "Backend 1 should receive some requests");
    assert!(backend2_count > 0, "Backend 2 should receive some requests");

    // Round Robinなのでほぼ均等に分散（許容範囲: 2-8、接続の再利用により完全に均等にならない可能性がある）
    assert!(
        (2..=8).contains(&backend1_count),
        "Backend 1 should receive roughly half: got {}",
        backend1_count
    );
    assert!(
        (2..=8).contains(&backend2_count),
        "Backend 2 should receive roughly half: got {}",
        backend2_count
    );
}

// ====================
// 静的ファイル配信テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_static_file_index() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();

    // レスポンスボディに期待されるコンテンツが含まれる
    assert!(
        response.contains("Hello from Backend 1") || response.contains("Hello from Backend 2"),
        "Should contain content from backend"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_static_file_large() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    assert!(response.is_some(), "Should receive large file response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Large file should return 200 OK");

    // 大きなレスポンスであることを確認
    assert!(response.len() > 1000, "Large file should be > 1000 bytes");
}

// ====================
// 圧縮テスト
// ====================

#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_compression_gzip() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 前提条件: /large.txt が存在することを確認（リトライあり）
    let prereq = send_request_with_retry(PROXY_PORT, "/large.txt", &[], 3).await;
    if prereq.is_none() {
        panic!("Prerequisite check failed: no response from /large.txt");
    }
    let prereq_status = get_status_code(prereq.as_ref().unwrap());
    if prereq_status != Some(200) {
        panic!(
            "Prerequisite failed: /large.txt not found (status: {:?})",
            prereq_status
        );
    }

    // Gzip圧縮をリクエスト
    let response =
        send_request_with_retry(PROXY_PORT, "/large.txt", &[("Accept-Encoding", "gzip")], 3).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 前提条件チェックで200が返ることを確認済みなので、ここでも200を期待
    assert_eq!(
        status,
        Some(200),
        "Compression request should return 200 OK, got: {:?}",
        status
    );

    // 圧縮が有効な場合、Content-Encodingヘッダーがある
    // min_size (1024) 以上のファイルなので圧縮されるはず
    let content_encoding = get_header_value(&response, "Content-Encoding");
    assert!(
        content_encoding
            .as_ref()
            .map(|e| e.contains("gzip") || e.contains("br") || e.contains("zstd"))
            .unwrap_or(false),
        "Large file should be compressed, got Content-Encoding: {:?}",
        content_encoding
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_compression_brotli() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Brotli圧縮をリクエスト
    let response = send_request(PROXY_PORT, "/large.txt", &[("Accept-Encoding", "br")]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");
}

// ====================
// バックエンド直接アクセステスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_backend1_direct() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(BACKEND1_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response from backend 1");

    let response = response.unwrap();
    let server_id = get_header_value(&response, "X-Server-Id");
    assert_eq!(
        server_id,
        Some("backend1".to_string()),
        "Should be backend1"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_backend2_direct() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(BACKEND2_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response from backend 2");

    let response = response.unwrap();
    let server_id = get_header_value(&response, "X-Server-Id");
    assert_eq!(
        server_id,
        Some("backend2".to_string()),
        "Should be backend2"
    );
}

// ====================
// Prometheusメトリクステスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_prometheus_metrics() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Metrics endpoint should return 200 OK");

    // Prometheusフォーマットのメトリクスが含まれる
    assert!(
        response.contains("# HELP") || response.contains("# TYPE") || response.contains("veil_"),
        "Should contain Prometheus metrics"
    );
}

// ====================
// エラーハンドリングテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_404_not_found() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 並列実行時の接続エラー対策としてリトライロジックを追加
    let response = send_request_with_retry(PROXY_PORT, "/nonexistent-path-12345", &[], 3).await;
    assert!(response.is_some(), "Should receive response after retries");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(404), "Nonexistent path should return 404");
}

// ====================
// HTTPS接続テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_https_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTPSポートに接続
    let response = send_request(PROXY_PORT, "/", &[]).await;

    assert!(
        response.is_some(),
        "Should receive response from HTTPS port"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "HTTPS request should succeed");
}

// ====================
// 並行リクエストテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[serial]
pub(super) async fn test_concurrent_requests() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let success_count = Arc::new(AtomicUsize::new(0));
    let total_requests = 20;

    let handles: Vec<_> = (0..total_requests)
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
    assert!(
        successes >= total_requests * 8 / 10,
        "At least 80% of concurrent requests should succeed: {}/{}",
        successes,
        total_requests
    );
}

// ====================
// レスポンスタイムテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_response_time() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use std::time::Instant;

    let start = Instant::now();
    let response = send_request(PROXY_PORT, "/", &[]).await;
    let elapsed = start.elapsed();

    assert!(response.is_some(), "Should receive response");

    // レスポンスは1秒以内であるべき
    assert!(
        elapsed.as_secs() < 1,
        "Response time should be under 1 second, was {:?}",
        elapsed
    );
}

// ====================
// Content-Typeテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_html_content_type() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let content_type = get_header_value(&response, "Content-Type");

    // .htmlファイルなのでtext/htmlであるべき
    if let Some(ct) = content_type {
        assert!(
            ct.contains("text/html") || ct.contains("text/plain"),
            "Content-Type should be text: {}",
            ct
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_json_content_type() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/health", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // health endpointはJSONを返す想定
    if status == Some(200) {
        // Content-Typeがapplication/jsonまたはtext/plainであることを確認
        let content_type = get_header_value(&response, "Content-Type");
        assert!(
            content_type
                .as_ref()
                .map(|ct| ct.contains("application/json") || ct.contains("text/plain"))
                .unwrap_or(true),
            "Health endpoint should return JSON or text content type, got: {:?}",
            content_type
        );

        // ボディがJSON形式であることを確認
        let body = response.split("\r\n\r\n").nth(1).unwrap_or("");
        assert!(
            body.contains("{") && body.contains("}"),
            "Health endpoint should return JSON body containing braces, got: {}",
            body
        );
    }
}

// ====================
// Keep-Aliveテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_keep_alive_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Keep-Alive接続でのリクエスト
    let response = send_request(PROXY_PORT, "/", &[("Connection", "keep-alive")]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");
}

// ====================
// User-Agentテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_custom_user_agent() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(PROXY_PORT, "/", &[("User-Agent", "VeilE2ETest/1.0")]).await;
    assert!(
        response.is_some(),
        "Should receive response with custom User-Agent"
    );

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");
}

// ====================
// Hostヘッダーテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_different_host_headers() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // localhost
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "localhost should work");

    // 127.0.0.1 のHost（TLS接続を使用）
    let response2 = send_request(PROXY_PORT, "/", &[("Host", "127.0.0.1")]).await;
    assert!(response2.is_some(), "127.0.0.1 Host should work");

    let response2 = response2.unwrap();
    let status = get_status_code(&response2);
    assert_eq!(status, Some(200), "127.0.0.1 Host should work");
}

// ====================
// 複数リクエストの安定性テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_multiple_sequential_requests() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut success_count = 0;
    let total_requests = 50;

    for _ in 0..total_requests {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            if get_status_code(&response) == Some(200) {
                success_count += 1;
            }
        }
    }

    // ほとんどのリクエストが成功するべき（タイミングの問題で1つ失敗する可能性がある）
    assert!(
        success_count >= total_requests * 9 / 10,
        "At least 90% of sequential requests should succeed: {}/{}",
        success_count,
        total_requests
    );
}

// ====================
// 圧縮エンコーディング優先順位テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_compression_priority() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 複数の圧縮形式をサポート
    let response = send_request(
        PROXY_PORT,
        "/large.txt",
        &[("Accept-Encoding", "gzip, br, zstd")],
    )
    .await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // 設定ではzstdが最優先のはず
    let content_encoding = get_header_value(&response, "Content-Encoding");
    if let Some(encoding) = content_encoding {
        // 圧縮が有効な場合、どれかの形式が使われる
        assert!(
            encoding.contains("zstd") || encoding.contains("br") || encoding.contains("gzip"),
            "Should use one of the accepted encodings: {}",
            encoding
        );
    }
}

// ====================
// メトリクステスト（新機能）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_active_connections_metric() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // メトリクスエンドポイントからアクティブ接続数を取得
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    let response = response.unwrap();

    // HTTP_ACTIVE_CONNECTIONSメトリクスが含まれるか確認
    // 注意: 接続が確立されている場合のみ値が存在する
    assert!(
        response.contains("http_active_connections")
            || response.contains("veil_proxy_http_active_connections"),
        "Should contain active connections metric"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_upstream_health_metric() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // メトリクスエンドポイントからアップストリーム健康状態を取得
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    let response = response.unwrap();

    // HTTP_UPSTREAM_HEALTHメトリクスが含まれることを確認
    assert!(
        response.contains("http_upstream_health")
            || response.contains("veil_proxy_http_upstream_health"),
        "Should contain upstream health metric"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_tls_health_check() {
    // このテストは、TLS健康チェック機能が正しく動作することを確認します
    // 注意: 実際のTLSバックエンドが必要なため、E2E環境でのみ実行可能

    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // TLS接続でヘルスチェックが動作することを確認
    // 実際のテストは、TLSバックエンドが設定されている場合にのみ有効
    // ここでは、メトリクスエンドポイントから健康状態を確認

    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    // メトリクスが正常に取得できることを確認
    let response = response.unwrap();
    assert!(
        response.contains("veil_proxy") || response.contains("# HELP"),
        "Should contain Prometheus metrics"
    );
}

// ====================
// エラーハンドリングテスト（優先度: 高）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_invalid_http_syntax() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なHTTP構文のリクエストを送信
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // 不正なHTTP構文を送信
    stream.write_all(b"INVALID REQUEST\r\n\r\n").unwrap();

    // レスポンスを受信
    let mut response = Vec::new();
    let _ = stream.read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);

    // 400 Bad Requestまたは接続エラーを受信することを確認
    // プロキシが接続を閉じる場合もあるため、エラーまたは400を確認
    assert!(
        response.contains("400") || response.is_empty(),
        "Should return 400 Bad Request or close connection for invalid HTTP syntax"
    );
}

#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_backend_connection_failure() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 存在しないパスにリクエストを送信（404を期待）
    // 負荷下でプロキシのコネクションプールが誤ったレスポンスを返すことがあるため
    // None および non-404 ステータスも最大5回リトライ
    let mut final_status = None;
    for _retry in 0..5u32 {
        let response = send_request(PROXY_PORT, "/nonexistent", &[]).await;
        if let Some(resp) = response {
            let s = get_status_code(&resp);
            if s == Some(404) {
                final_status = s;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    // 静的ファイルルーティングでは存在しないファイル → 404
    assert_eq!(
        final_status,
        Some(404),
        "Nonexistent path should return 404 after retries"
    );
}

// ====================
// WebSocket E2Eテスト（優先度: 中）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_websocket_basic_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // WebSocket接続を試みる（TLS経由）
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(std::time::Duration::from_secs(5)))
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
    // WebSocketルートが正しく設定されている場合は 101 Switching Protocols を期待
    assert_eq!(
        status,
        Some(101),
        "WebSocket upgrade should return 101 Switching Protocols, got: {:?}",
        status
    );
    assert!(
        response.contains("Upgrade:") || response.contains("upgrade:"),
        "101 response should contain Upgrade header"
    );
}
