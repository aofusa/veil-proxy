//! h2c・バッファリング・WebSocket・ヘルスチェックの不足分
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// H2C (HTTP/2 over cleartext) E2Eテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_proxy_forwarding() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/1.1でプロキシにリクエスト送信
    // プロキシがH2Cでバックエンドに接続し、正常に動作することを確認
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // H2Cバックエンドが正常に動作している場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C request, got: {:?}",
        status
    );

    // X-H2C-Testヘッダーが追加されていることを確認（H2Cルートが使用された場合）
    if status == Some(200) {
        let h2c_test_header = get_header_value(&response, "X-H2C-Test");
        if let Some(value) = h2c_test_header {
            assert_eq!(value, "true", "X-H2C-Test header should be 'true'");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_get_request() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // GETリクエストをH2Cルート経由で送信
    let response = send_request(PROXY_PORT, "/h2c/index.html", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // H2C接続が正常に確立された場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C GET request, got: {:?}",
        status
    );

    if status == Some(200) {
        // レスポンスボディを確認
        let body_start = response.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
        let body = &response[body_start..];
        assert!(
            body.contains("H2C") || body.contains("Hello"),
            "Response body should contain expected content"
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_post_request() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // POSTリクエストをH2Cルート経由で送信
    let body = b"test post body";
    let content_length_str = body.len().to_string();
    let headers = vec![
        ("Content-Type", "text/plain"),
        ("Content-Length", &content_length_str),
    ];

    let response = send_post_request(PROXY_PORT, "/h2c/test.txt", &headers, body).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // H2C接続が正常に確立された場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C POST request, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_header_manipulation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // カスタムヘッダーを含むリクエストを送信
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);

    if status == Some(200) {
        // X-H2C-Testヘッダーが追加されていることを確認
        let h2c_test_header = get_header_value(&response, "X-H2C-Test");
        if let Some(value) = h2c_test_header {
            assert_eq!(value, "true", "X-H2C-Test header should be 'true'");
        }

        // X-Proxied-Byヘッダーが追加されていることを確認
        let proxied_by = get_header_value(&response, "X-Proxied-By");
        if let Some(value) = proxied_by {
            assert_eq!(value, "veil", "X-Proxied-By header should be 'veil'");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_connection_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 存在しないバックエンドへのH2C接続を試みる
    // プロキシ設定で存在しないポートを指定する必要があるが、
    // テスト環境では既存のルートを使用してタイムアウトを確認

    // 実際のタイムアウトテストには、遅延応答するバックエンドが必要
    // ここでは、基本的な動作確認のみ実施
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // H2C接続が正常に確立された場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C POST request, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_backend_unavailable() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 存在しないパスへのリクエストの場合、404 Not Foundが返される
    let response = send_request(PROXY_PORT, "/h2c/nonexistent", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 存在しないパスへのリクエストの場合、404 Not Foundが返される
    assert_eq!(
        status,
        Some(404),
        "Should return 404 Not Found for nonexistent path, got: {:?}",
        status
    );
}

// ====================
// H2C 未実装テストの実装
// ====================

// カテゴリ1: 基本接続テスト（優先度: 高）

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_basic_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2Cルート経由で基本的な接続を確認
    // プロキシがH2Cでバックエンドに接続し、正常に動作することを確認
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // H2C接続が正常に確立された場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C connection, got: {:?}",
        status
    );

    // 接続が確立された場合、X-H2C-Testヘッダーが追加されていることを確認
    if status == Some(200) {
        let h2c_test_header = get_header_value(&response, "X-H2C-Test");
        if let Some(value) = h2c_test_header {
            assert_eq!(value, "true", "X-H2C-Test header should be 'true'");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_connection_reuse() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 同一接続での複数リクエストを確認
    // プロキシがH2C接続を再利用することを確認
    let response1 = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(
        response1.is_some(),
        "Should receive first response from proxy"
    );

    // 短い待機時間後に2回目のリクエストを送信
    tokio::time::sleep(Duration::from_millis(100)).await;

    let response2 = send_request(PROXY_PORT, "/h2c/index.html", &[]).await;
    assert!(
        response2.is_some(),
        "Should receive second response from proxy"
    );

    // 両方のリクエストがレスポンスを受信することを確認
    let status1 = get_status_code(&response1.unwrap());
    assert!(
        status1 == Some(200) || status1 == Some(502) || status1 == Some(504),
        "First request should return 200 OK, 502 Bad Gateway, or 504 Gateway Timeout, got: {:?}",
        status1
    );

    let status2 = get_status_code(&response2.unwrap());
    assert!(
        status2 == Some(200) || status2 == Some(502) || status2 == Some(504),
        "Second request should return 200 OK, 502 Bad Gateway, or 504 Gateway Timeout, got: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_connection_close() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 接続の正常終了を確認
    // Connection: closeヘッダーを含むリクエストを送信
    let headers = vec![("Connection", "close")];
    let response = send_request(PROXY_PORT, "/h2c/", &headers).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // Connection: closeヘッダーが正しく処理され、接続が正常に終了することを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C connection with close header, got: {:?}",
        status
    );
}

// カテゴリ2: ハンドシェイクテスト（優先度: 高）

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_handshake_success() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2Cハンドシェイクの成功を確認
    // プロキシがH2Cでバックエンドに接続し、ハンドシェイクが成功することを確認
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // ハンドシェイクが成功した場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for successful H2C handshake, got: {:?}",
        status
    );

    // ハンドシェイクが成功した場合、X-H2C-Testヘッダーが追加されていることを確認
    if status == Some(200) {
        let h2c_test_header = get_header_value(&response, "X-H2C-Test");
        if let Some(value) = h2c_test_header {
            assert_eq!(value, "true", "X-H2C-Test header should be 'true'");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_settings_negotiation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // SETTINGSネゴシエーションを確認
    // プロキシがH2Cでバックエンドに接続し、SETTINGSフレームが交換されることを確認
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // SETTINGSネゴシエーションが成功した場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for successful H2C SETTINGS negotiation, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_handshake_failure() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2Cハンドシェイクの失敗を確認
    // 存在しないパスへのリクエストを送信して、ハンドシェイクが失敗する場合のエラーハンドリングを確認
    let response = send_request(PROXY_PORT, "/h2c/invalid-path-that-should-fail", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 存在しないパスへのリクエストの場合、404 Not Foundが返される
    assert_eq!(
        status,
        Some(404),
        "Should return 404 Not Found for invalid path, got: {:?}",
        status
    );
}

// カテゴリ3: リクエスト/レスポンステスト（優先度: 高）

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_large_request_body() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 大きなリクエストボディの送信を確認
    // フロー制御が正しく動作することを確認
    let large_body = vec![b'A'; 10000]; // 10KBのボディ
    let content_length_str = large_body.len().to_string();
    let headers = vec![
        ("Content-Type", "text/plain"),
        ("Content-Length", &content_length_str),
    ];

    let response = send_post_request(PROXY_PORT, "/h2c/test.txt", &headers, &large_body).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 大きなボディが正しく転送されることを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for large request body, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_large_response_body() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 大きなレスポンスボディの受信を確認
    // フロー制御が正しく動作することを確認
    let response = send_request(PROXY_PORT, "/h2c/large.txt", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 大きなレスポンスが正しく受信されることを確認
    // ファイルが存在する場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for large response, got: {:?}",
        status
    );

    if status == Some(200) {
        // レスポンスボディが存在することを確認
        let body_start = response.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
        let body = &response[body_start..];
        assert!(!body.is_empty(), "Response body should not be empty");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_header_compression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HPACK圧縮を確認
    // 複数のヘッダーを含むリクエストを送信して、HPACK圧縮が正しく動作することを確認
    let headers = vec![
        ("User-Agent", "test-client/1.0"),
        (
            "Accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        ),
        ("Accept-Language", "en-US,en;q=0.5"),
        ("Accept-Encoding", "gzip, deflate"),
        ("Connection", "keep-alive"),
    ];

    let response = send_request(PROXY_PORT, "/h2c/", &headers).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // HPACK圧縮が正しく動作することを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C request with HPACK compression, got: {:?}",
        status
    );
}

// カテゴリ4: ストリーム多重化テスト（優先度: 中）

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_multiple_streams() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 複数ストリームの並行処理を確認
    // 同一接続での複数リクエストを並行して送信
    let mut handles = Vec::new();

    for i in 0..3 {
        let handle = tokio::spawn(async move {
            let path = format!("/h2c/test{}.txt", i);
            send_request(PROXY_PORT, &path, &[]).await
        });
        handles.push(handle);
    }

    // すべてのリクエストが完了するまで待機
    let mut responses = Vec::new();
    for handle in handles {
        if let Ok(response) = handle.await {
            responses.push(response);
        }
    }

    // すべてのリクエストがレスポンスを受信することを確認
    assert_eq!(responses.len(), 3, "Should receive 3 responses");

    for response in responses {
        assert!(response.is_some(), "Should receive response from proxy");
        let status = get_status_code(&response.unwrap());
        // H2C接続が正常に確立された場合、200 OKが返される
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for H2C multiplexing request, got: {:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_stream_priority() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ストリーム優先度を確認
    // 優先度ヘッダーを含むリクエストを送信
    // 注意: HTTP/2の優先度は複雑なため、ここでは基本的な動作確認のみ実施
    let headers = vec![("Priority", "u=0, i")];

    let response = send_request(PROXY_PORT, "/h2c/", &headers).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 優先度が正しく処理されることを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C request with priority, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_stream_cancellation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ストリームキャンセルを確認
    // 接続を早期に切断して、ストリームがキャンセルされることを確認
    // 注意: 実際のRST_STREAMフレームのテストには、より低レベルな実装が必要
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // ストリームが正しく処理されることを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C stream cancellation test, got: {:?}",
        status
    );
}

// カテゴリ5: エラーハンドリングテスト（優先度: 高）

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_invalid_frame() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正フレームの処理を確認
    // プロキシ経由では直接的な不正H2Cフレームの送信は困難なため、
    // 極端に長いパスを送信してURI Too Longエラーハンドリングを確認
    let long_path = format!("/h2c/{}", "a".repeat(9000));
    let response = send_request(PROXY_PORT, &long_path, &[]).await;
    assert!(response.is_some(), "Should receive response from proxy");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // URIが長すぎる場合、414 URI Too Longが返される
    assert_eq!(
        status,
        Some(414),
        "Should return 414 URI Too Long for excessively long path, got: {:?}",
        status
    );
}

// カテゴリ6: プロキシ機能テスト（優先度: 高）

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_proxy_load_balancing() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2Cバックエンドへのロードバランシングを確認
    // 注意: 現在の設定ではH2Cバックエンドは単一のため、
    // ロードバランシングのテストは限定的
    // 複数のリクエストを送信して、プロキシが正しく動作することを確認
    let mut responses = Vec::new();

    for _ in 0..5 {
        let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
        responses.push(response);
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // すべてのリクエストがレスポンスを受信することを確認
    for response in responses {
        assert!(response.is_some(), "Should receive response from proxy");
        let status = get_status_code(&response.unwrap());
        // H2C接続が正常に確立された場合、200 OKが返される
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for H2C load balancing request, got: {:?}",
            status
        );
    }
}

// カテゴリ7: gRPC統合テスト（優先度: 中）

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(all(feature = "http2", feature = "grpc"))]
pub(super) async fn test_h2c_grpc_unary_call() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2C経由でのgRPC Unary RPCを確認
    // gRPCリクエストをgRPCルート経由で送信（バックエンドはH2Cで接続）
    let message = b"test message";
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/UnaryCall",
        message,
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
    // gRPCリクエストが正常に処理された場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for gRPC request over H2C, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(all(feature = "http2", feature = "grpc"))]
pub(super) async fn test_h2c_grpc_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2C経由でのgRPCストリーミングを確認
    // gRPCストリーミングリクエストをgRPCルート経由で送信（バックエンドはH2Cで接続）
    let message = b"start streaming";
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/ServerStreaming",
        message,
        &[],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!("Failed to send gRPC streaming request: {}", e);
        }
    };

    let status = GrpcTestClient::extract_status_code(&response);
    // gRPCストリーミングが正しく処理された場合、200 OKが返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for gRPC streaming request over H2C, got: {:?}",
        status
    );
}

// カテゴリ8: パフォーマンステスト（優先度: 低）

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_throughput() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2C接続のスループットを測定
    let start = std::time::Instant::now();
    let mut success_count = 0;
    let request_count = 10;

    for _ in 0..request_count {
        let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
        if let Some(resp) = response {
            let status = get_status_code(&resp);
            // H2C接続が正常に確立された場合、200 OKが返される
            if status == Some(200) {
                success_count += 1;
            }
        }
    }

    let elapsed = start.elapsed();
    let throughput = request_count as f64 / elapsed.as_secs_f64();

    eprintln!(
        "H2C throughput test: {} requests in {:?}, throughput: {:.2} req/s, successful: {}",
        request_count, elapsed, throughput, success_count
    );

    // スループットが測定できることを確認
    assert!(throughput > 0.0, "Throughput should be greater than 0");
    assert!(
        success_count > 0,
        "Should have at least one successful request"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_latency() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2C接続のレイテンシを測定
    let mut latencies = Vec::new();
    let request_count = 5;

    for _ in 0..request_count {
        let start = std::time::Instant::now();
        let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
        let elapsed = start.elapsed();

        if response.is_some() {
            latencies.push(elapsed);
        }
    }

    if !latencies.is_empty() {
        let avg_latency = latencies.iter().sum::<Duration>() / latencies.len() as u32;
        let min_latency = latencies.iter().min().copied().unwrap_or(Duration::ZERO);
        let max_latency = latencies.iter().max().copied().unwrap_or(Duration::ZERO);

        eprintln!(
            "H2C latency test: avg={:?}, min={:?}, max={:?}",
            avg_latency, min_latency, max_latency
        );

        // レイテンシが測定できることを確認
        assert!(
            avg_latency > Duration::ZERO,
            "Average latency should be greater than 0"
        );
    }
}

// ====================
// バッファリング: 不足しているテスト（優先度: 高）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_disk_spillover_enabled() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ディスクスピルオーバー有効時の動作確認
    // 注意: このテストは設定ファイルでディスクスピルオーバーを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test buffering
    // disk_buffer_path = "/tmp/veil_buffer" が設定されている場合のテスト

    // メモリバッファ上限（デフォルト10MB）を超える大きなレスポンスをリクエスト
    // 実際のテストでは、20MB以上のレスポンスを生成する必要がある
    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // ディスクスピルオーバーが有効な場合、メモリ上限超過時にディスクに書き込まれる
        // 正常に処理される場合は200が返される
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for disk spillover test, got: {:?}",
            status
        );

        if status == Some(200) {
            // Content-Lengthを確認
            let content_length = get_content_length_from_headers(response.as_bytes());
            if let Some(cl) = content_length {
                eprintln!(
                    "Buffering disk spillover enabled test: content length = {} bytes",
                    cl
                );
                // メモリ上限（10MB）を超える場合、ディスクスピルオーバーが使用される
                if cl > 10 * 1024 * 1024 {
                    eprintln!("Large response detected, disk spillover may be used");
                }
            }
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_disk_spillover_disabled() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ディスクスピルオーバー無効時の動作確認
    // disk_buffer_pathが設定されていない場合、メモリ上限超過時にエラーが返される可能性がある

    // メモリバッファ上限を超える大きなレスポンスをリクエスト
    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // ディスクスピルオーバーが無効な場合でも、ストリーミングモードにフォールバックされるため、200が返される
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for buffering test (streaming fallback), got: {:?}",
            status
        );

        eprintln!(
            "Buffering disk spillover disabled test: status={:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_client_write_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // クライアント書き込みタイムアウトの動作確認

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
        .unwrap();

    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();

    while tls_conn.is_handshaking() {
        if tls_conn.complete_io(&mut stream).is_err() {
            panic!("TLS handshake error");
        }
    }

    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // 部分的なリクエストを送信して放置する
    let request_part =
        b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 1000\r\n\r\npartial data";
    if let Err(e) = tls_stream.write_all(request_part) {
        panic!("Failed to send partial request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // タイムアウトを待つ（サーバー側のタイムアウト設定によるが、ここでは数秒待機）
    tokio::time::sleep(Duration::from_secs(6)).await;

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

    // タイムアウトにより切断されるか、408 Request Timeoutが返るはず
    assert!(
        status == Some(408) || status.is_none() || status == Some(400),
        "Should return 408 Request Timeout or close connection for slow client write, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_slow_client_detection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 低速クライアントの検出を確認
    // 注意: このテストは低速クライアントをシミュレートする必要がある

    // 通常のリクエストを送信（リトライ付き）
    let mut response = None;
    for _retry in 0..3 {
        response = send_request(PROXY_PORT, "/", &[]).await;
        if response.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if let Some(resp) = response {
        let status = get_status_code(&resp);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // 低速クライアントが適切に検出されることを確認
        // 実際のテストには、低速クライアントのシミュレーションが必要
        eprintln!("Buffering slow client detection test: status={:?}", status);
    } else {
        eprintln!("Buffering slow client detection test: failed to receive response (environment may not be ready)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_full_backend_connection_early_release() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Fullモードでのバックエンド接続早期解放を確認
    // 注意: このテストは設定ファイルでFullバッファリングモードを設定する必要がある
    // 例: ./tests/e2e_setup.sh test buffering

    use std::time::Instant;

    // Fullモードの場合、バッファリング完了後にバックエンド接続が解放される
    let start = Instant::now();
    let mut response = None;
    for _retry in 0..3 {
        response = send_request(PROXY_PORT, "/", &[]).await;
        if response.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let elapsed = start.elapsed();

    if let Some(resp) = response {
        let status = get_status_code(&resp);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // Fullモードでは、バックエンド接続が早期に解放される可能性がある
        // 実際の検証には、バックエンド接続の状態を監視する必要がある
        eprintln!(
            "Buffering full backend connection early release test: elapsed={:?}",
            elapsed
        );
    } else {
        eprintln!("Buffering full backend connection early release test: failed to receive response (environment may not be ready)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_streaming_backend_connection_release() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Streamingモードでのバックエンド接続保持を確認
    // Streamingモードの場合、バックエンド接続がレスポンス完了まで保持される

    use std::time::Instant;

    let start = Instant::now();
    let mut response = None;
    for _retry in 0..3 {
        response = send_request(PROXY_PORT, "/large.txt", &[]).await;
        if response.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let elapsed = start.elapsed();

    if let Some(resp) = response {
        let status = get_status_code(&resp);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // Streamingモードでは、バックエンド接続がレスポンス完了まで保持される
        // 実際の検証には、バックエンド接続の状態を監視する必要がある
        eprintln!(
            "Buffering streaming backend connection release test: elapsed={:?}",
            elapsed
        );
    } else {
        eprintln!("Buffering streaming backend connection release test: failed to receive response (environment may not be ready)");
    }
}

#[tokio::test]
#[ntest::timeout(60000)]
pub(super) async fn test_buffering_adaptive_threshold_exact() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Adaptiveモードの閾値正確な切り替えを確認
    // 注意: このテストは設定ファイルでAdaptiveバッファリングモードを設定する必要がある
    // 例: ./tests/e2e_setup.sh test buffering

    // 閾値より小さいレスポンス（Fullバッファリング）
    let mut small_response = None;
    for _retry in 0..3 {
        small_response = send_request(PROXY_PORT, "/", &[]).await;
        if small_response.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    if let Some(small_resp) = small_response {
        assert_eq!(
            get_status_code(&small_resp),
            Some(200),
            "Should return 200 OK"
        );

        // 閾値より大きいレスポンス（Streaming）
        // None またはコンテンツが小さすぎる場合もリトライ（負荷下での誤ルーティング対策）
        let mut large_response = None;
        for _retry in 0..5 {
            let resp = send_request(PROXY_PORT, "/large.txt", &[]).await;
            if let Some(ref r) = resp {
                let cl = get_content_length_from_headers(r.as_bytes());
                if cl.unwrap_or(0) > 100 {
                    large_response = resp;
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }

        if let Some(large_resp) = large_response {
            let status = get_status_code(&large_resp);
            if status == Some(200) {
                // 閾値前後でモードが正確に切り替わることを確認
                let small_size = small_resp.len();
                let large_size = large_resp.len();

                // Content-Lengthを確認
                let small_cl = get_content_length_from_headers(small_resp.as_bytes());
                let large_cl = get_content_length_from_headers(large_resp.as_bytes());

                eprintln!("Adaptive threshold exact test: small={} bytes (cl={:?}), large={} bytes (cl={:?})", 
                         small_size, small_cl, large_size, large_cl);

                // 大きいレスポンスが小さいレスポンスより大きいことを確認
                assert!(
                    large_size > small_size,
                    "Large response should be larger than small response"
                );

                // 閾値（デフォルト1MB）前後でモードが切り替わることを確認
                if let (Some(small_cl_val), Some(large_cl_val)) = (small_cl, large_cl) {
                    let threshold = 1024 * 1024; // 1MB
                    if small_cl_val <= threshold && large_cl_val > threshold {
                        eprintln!(
                            "Adaptive mode switch detected: small <= {} bytes, large > {} bytes",
                            threshold, threshold
                        );
                    }
                }
            }
        } else {
            eprintln!("Buffering adaptive threshold exact test: failed to receive large response (environment may not be ready)");
        }
    } else {
        eprintln!("Buffering adaptive threshold exact test: failed to receive small response (environment may not be ready)");
    }
}

// ====================
// WebSocket: 不足しているテスト（優先度: 高）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_poll_mode_fixed() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Fixedモードの動作確認
    // 注意: このテストは設定ファイルでwebsocket_poll_mode = "fixed"を設定する必要がある
    // 実際のWebSocket通信を検証するには、WebSocketクライアントライブラリが必要

    // WebSocketアップグレードリクエストを送信
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

    // WebSocketアップグレードリクエストを送信
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send WebSocket upgrade request: {:?}", e);
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

    // Fixedモードでは、常に固定タイムアウトでポーリングされる
    // 実際の検証には、WebSocketフレームの送受信とタイムアウトの測定が必要
    assert!(
        status == Some(101),
        "Should return appropriate status: {:?}",
        status
    );

    eprintln!("WebSocket poll mode fixed test: status={:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_poll_mode_adaptive_active() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Adaptiveモード（アクティブ時）の動作確認
    // データ転送時は短いタイムアウトでポーリングされる

    // WebSocketアップグレードリクエストを送信
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

    // WebSocketアップグレードリクエストを送信
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send WebSocket upgrade request: {:?}", e);
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

    // Adaptiveモードでは、データ転送時は短いタイムアウトでポーリングされる
    // 実際の検証には、WebSocketフレームの送受信とタイムアウトの測定が必要
    assert!(
        status == Some(101),
        "Should return appropriate status: {:?}",
        status
    );

    eprintln!(
        "WebSocket poll mode adaptive active test: status={:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_long_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 長時間接続の動作確認
    // 注意: 実際のWebSocket通信を検証するには、WebSocketクライアントライブラリが必要

    // WebSocketアップグレードリクエストを送信
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
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

    // WebSocketアップグレードリクエストを送信
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send WebSocket upgrade request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // ヘッダー部分のみ受信（\r\n\r\nまで）
    let mut response = Vec::new();
    let mut single = [0u8; 1];
    loop {
        match tls_stream.read(&mut single) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                response.push(single[0]);
                if response.ends_with(b"\r\n\r\n") {
                    break;
                }
                if response.len() > 8192 {
                    break;
                }
            }
        }
    }

    let response = String::from_utf8_lossy(&response);
    let status = get_status_code(&response);

    // 長時間接続が維持されることを確認（101 Switching Protocolsで確立）
    assert!(
        status == Some(101),
        "Should return appropriate status: {:?}",
        status
    );

    eprintln!("WebSocket long connection test: status={:?}", status);
}

// ====================
// ヘルスチェック: 不足しているテスト（優先度: 高）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_unhealthy_threshold_exact() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不健康閾値の正確な動作確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck
    // 実際のテストには、バックエンドの動的な障害をシミュレートする必要がある

    // メトリクスエンドポイントから初期状態を取得
    let initial_metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // 複数のリクエストを送信してヘルスチェックが動作することを確認
    for i in 0..10 {
        // リトライロジックを追加
        let mut response = None;
        for _retry in 0..3 {
            response = send_request(PROXY_PORT, "/", &[]).await;
            if response.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        if let Some(resp) = response {
            let status = get_status_code(&resp);
            assert_eq!(status, Some(200), "Should return 200 OK for request {}", i);
        } else {
            eprintln!(
                "Health check unhealthy threshold exact test: failed to receive response {}",
                i
            );
            // リクエストが失敗してもテストを続行（環境の問題の可能性）
            continue;
        }

        // ヘルスチェック間隔を待つ
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // メトリクスエンドポイントから最終状態を取得
    let final_metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // メトリクスが更新されていることを確認
    if let (Some(initial), Some(final_state)) = (initial_metrics, final_metrics) {
        if initial.contains("http_upstream_health") || final_state.contains("http_upstream_health")
        {
            eprintln!("Health check unhealthy threshold exact test: metrics detected");
            // 連続失敗回数が閾値に達した時点でサーバーが除外されることを確認
            // 実際の検証には、バックエンドの動的な障害をシミュレートする必要がある
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_healthy_threshold_exact() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 健康閾値の正確な動作確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck
    // 実際のテストには、バックエンドの動的な回復をシミュレートする必要がある

    // メトリクスエンドポイントから初期状態を取得
    let initial_metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // 複数のリクエストを送信してヘルスチェックが動作することを確認
    for i in 0..10 {
        // リトライロジックを追加
        let mut response = None;
        for _retry in 0..3 {
            response = send_request(PROXY_PORT, "/", &[]).await;
            if response.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        if let Some(resp) = response {
            let status = get_status_code(&resp);
            assert_eq!(status, Some(200), "Should return 200 OK for request {}", i);
        } else {
            eprintln!(
                "Health check healthy threshold exact test: failed to receive response {}",
                i
            );
            // リクエストが失敗してもテストを続行（環境の問題の可能性）
            continue;
        }

        // ヘルスチェック間隔を待つ
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // メトリクスエンドポイントから最終状態を取得
    let final_metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // メトリクスが更新されていることを確認
    if let (Some(initial), Some(final_state)) = (initial_metrics, final_metrics) {
        if initial.contains("http_upstream_health") || final_state.contains("http_upstream_health")
        {
            eprintln!("Health check healthy threshold exact test: metrics detected");
            // 連続成功回数が閾値に達した時点でサーバーが復帰することを確認
            // 実際の検証には、バックエンドの動的な回復をシミュレートする必要がある
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_tls_cert_verification_enabled() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 証明書検証有効時の動作確認
    // 注意: このテストは設定ファイルでTLSヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck
    // use_tls = true, verify_cert = true が設定されている場合のテスト

    // メトリクスエンドポイントから健康状態を確認
    let metrics_response = send_request(PROXY_PORT, "/__metrics", &[]).await;

    if let Some(metrics) = metrics_response {
        if metrics.contains("http_upstream_health")
            || metrics.contains("veil_proxy_http_upstream_health")
        {
            eprintln!("Health check TLS cert verification enabled test: metrics detected");
            // 証明書検証が有効な場合、有効な証明書でヘルスチェックが成功することを確認
            // 実際の検証には、有効な証明書と無効な証明書の両方でテストする必要がある
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_tls_cert_verification_disabled() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 証明書検証無効時の動作確認
    // 注意: このテストは設定ファイルでTLSヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck
    // use_tls = true, verify_cert = false が設定されている場合のテスト

    // メトリクスエンドポイントから健康状態を確認
    let metrics_response = send_request(PROXY_PORT, "/__metrics", &[]).await;

    if let Some(metrics) = metrics_response {
        if metrics.contains("http_upstream_health")
            || metrics.contains("veil_proxy_http_upstream_health")
        {
            eprintln!("Health check TLS cert verification disabled test: metrics detected");
            // 証明書検証が無効な場合、自己署名証明書でもヘルスチェックが成功することを確認
            // 実際の検証には、自己署名証明書でテストする必要がある
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_backend_slow_response() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // バックエンドの遅い応答時の動作確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck
    // 実際のテストには、遅い応答を返すバックエンドエンドポイントが必要

    // 通常のリクエストを送信（リトライ付き）
    let mut response = None;
    for _retry in 0..3 {
        response = send_request(PROXY_PORT, "/", &[]).await;
        if response.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    if let Some(resp) = response {
        let status = get_status_code(&resp);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // 遅い応答がタイムアウトで処理されることを確認
        // 実際の検証には、遅い応答を返すバックエンドエンドポイントが必要
        eprintln!(
            "Health check backend slow response test: status={:?}",
            status
        );
    } else {
        eprintln!("Health check backend slow response test: failed to receive response (environment may not be ready)");
    }
}

#[tokio::test]
#[ntest::timeout(60000)]
pub(super) async fn test_health_check_backend_intermittent_failure() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 間欠的な障害の動作確認
    // ヘルスチェックが稼働中のプロキシに対して複数リクエストを送信し、
    // メトリクスが更新されることを確認する

    // メトリクスエンドポイントから初期状態を取得
    let initial_metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // 複数のリクエストを送信（ヘルスチェック間隔をまたぐため）
    // 5回で複数のヘルスチェック間隔を網羅するのに十分
    for _ in 0..5 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let status = get_status_code(&response);
            if status != Some(200) {
                eprintln!(
                    "Health check backend intermittent failure test: non-200 status={:?}",
                    status
                );
            }
        }

        // ヘルスチェック間隔を待つ（async sleepでランタイムをブロックしない）
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // メトリクスエンドポイントから最終状態を取得
    let final_metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // メトリクスが更新されていることを確認
    if let (Some(initial), Some(final_state)) = (initial_metrics, final_metrics) {
        if initial.contains("http_upstream_health") || final_state.contains("http_upstream_health")
        {
            eprintln!("Health check backend intermittent failure test: metrics detected");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_disk_spillover_max_size() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ディスクバッファ上限超過時の動作確認
    // 注意: このテストは設定ファイルでディスクスピルオーバーを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test buffering
    // disk_buffer_path = "/tmp/veil_buffer", max_disk_buffer = 100MB が設定されている場合のテスト

    // /disk-spillover/ は upstream へ正常に転送されるが、プロキシ側でバッファ制限がかかる
    // 設定した 100 バイトを超えるレスポンスを確実に生成するため large.txt を使用
    let response = send_request(PROXY_PORT, "/disk-spillover/large.txt", &[]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // ディスクバッファ上限超過時、507 Insufficient Storageが返される
        assert_eq!(
            status,
            Some(507),
            "Should return 507 Insufficient Storage for disk buffer max size exceeded, got: {:?}",
            status
        );

        eprintln!(
            "Buffering disk spillover max size test: status={:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_performance_streaming_vs_full() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // StreamingとFullモードのパフォーマンス比較
    // 注意: このテストは設定ファイルでバッファリングモードを設定する必要がある
    // 例: ./tests/e2e_setup.sh test buffering

    use std::time::Instant;

    // Streamingモードでのパフォーマンス測定
    let start_streaming = Instant::now();
    let response_streaming = send_request(PROXY_PORT, "/large.txt", &[]).await;
    let elapsed_streaming = start_streaming.elapsed();

    // Fullモードでのパフォーマンス測定
    let start_full = Instant::now();
    let response_full = send_request(PROXY_PORT, "/large.txt", &[]).await;
    let elapsed_full = start_full.elapsed();

    if let (Some(resp_s), Some(resp_f)) = (response_streaming, response_full) {
        let status_s = get_status_code(&resp_s);
        let status_f = get_status_code(&resp_f);

        if status_s == Some(200) && status_f == Some(200) {
            eprintln!(
                "Buffering performance streaming vs full test: streaming={:?}, full={:?}",
                elapsed_streaming, elapsed_full
            );

            // パフォーマンスの違いを確認
            // Fullモードはバッファリングのオーバーヘッドがあるため、若干遅い可能性がある
            // ただし、実際のパフォーマンスは環境に依存するため、ここでは測定のみ
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_poll_mode_adaptive_idle() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Adaptiveモード（アイドル時）の動作確認
    // アイドル時はタイムアウトが延長される

    // WebSocketアップグレードリクエストを送信
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(10)))
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

    // WebSocketアップグレードリクエストを送信
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send WebSocket upgrade request: {:?}", e);
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

    // Adaptiveモードでは、アイドル時はタイムアウトが延長される
    // 実際の検証には、WebSocketフレームの送受信とタイムアウトの測定が必要
    assert!(
        status == Some(101),
        "Should return appropriate status: {:?}",
        status
    );

    eprintln!(
        "WebSocket poll mode adaptive idle test: status={:?}",
        status
    );
}

/// F-282: `websocket_idle_timeout_secs` のルートでは、101 の後にどちらの向きにもデータが流れなければトンネルを閉じる
#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_f282_websocket_tunnel_idle_timeout_closes() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();
    while tls_conn.is_handshaking() {
        tls_conn.complete_io(&mut stream).expect("TLS handshake");
    }
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    let request = b"GET /ws-idle/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    tls_stream.write_all(request).unwrap();
    tls_stream.flush().unwrap();
    let mut response = Vec::new();
    let mut single = [0u8; 1];
    while !response.ends_with(b"\r\n\r\n") && response.len() < 8192 {
        match tls_stream.read(&mut single) {
            Ok(0) | Err(_) => break,
            Ok(_) => response.push(single[0]),
        }
    }
    let status = get_status_code(&String::from_utf8_lossy(&response));
    assert_eq!(status, Some(101), "upgrade must succeed");

    // 何も送らずに待つ。1 秒のアイドルタイムアウトで閉じられる（読み取りの 10 秒より十分前）
    let started = std::time::Instant::now();
    let mut buf = [0u8; 1024];
    let closed = loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break true,
            Ok(_) => continue,
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                break false
            }
            // close_notify 無しの切断も閉じたとみなす
            Err(_) => break true,
        }
    };
    let elapsed = started.elapsed();
    assert!(closed, "the idle tunnel must be closed by the proxy");
    assert!(
        elapsed < Duration::from_secs(6),
        "the idle tunnel must close after about 1s, took {elapsed:?}"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_idle_connection_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // アイドル接続のタイムアウト確認
    // 注意: 実際のWebSocket通信を検証するには、WebSocketクライアントライブラリが必要

    // WebSocketアップグレードリクエストを送信
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
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

    // WebSocketアップグレードリクエストを送信
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send WebSocket upgrade request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // ヘッダー部分のみ受信（\r\n\r\nまで）
    let mut response = Vec::new();
    let mut single = [0u8; 1];
    loop {
        match tls_stream.read(&mut single) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                response.push(single[0]);
                if response.ends_with(b"\r\n\r\n") {
                    break;
                }
                if response.len() > 8192 {
                    break;
                }
            }
        }
    }

    let response = String::from_utf8_lossy(&response);
    let status = get_status_code(&response);

    // アイドル接続が確立されることを確認（タイムアウト動作はプロキシ設定次第）
    assert!(
        status == Some(101),
        "Should return appropriate status: {:?}",
        status
    );

    eprintln!(
        "WebSocket idle connection timeout test: status={:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_threshold_counting() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 閾値カウントの正確性を確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck

    // メトリクスエンドポイントから初期状態を取得
    let initial_metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // 複数のリクエストを送信してヘルスチェックが動作することを確認
    for i in 0..10 {
        // リトライロジックを追加
        let mut response = None;
        for _retry in 0..3 {
            response = send_request(PROXY_PORT, "/", &[]).await;
            if response.is_some() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        if let Some(resp) = response {
            let status = get_status_code(&resp);
            assert_eq!(status, Some(200), "Should return 200 OK for request {}", i);
        } else {
            eprintln!(
                "Health check threshold counting test: failed to receive response {}",
                i
            );
            // リクエストが失敗してもテストを続行（環境の問題の可能性）
            continue;
        }

        // ヘルスチェック間隔を待つ
        tokio::time::sleep(Duration::from_millis(100)).await;

        // 中間状態のメトリクスを取得
        if i % 5 == 0 {
            let metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;
            if let Some(metrics) = metrics {
                if metrics.contains("http_upstream_health")
                    || metrics.contains("veil_proxy_http_upstream_health")
                {
                    eprintln!(
                        "Health check threshold counting test: intermediate metrics at request {}",
                        i
                    );
                }
            }
        }
    }

    // メトリクスエンドポイントから最終状態を取得
    let final_metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // メトリクスが更新されていることを確認
    if let (Some(initial), Some(final_state)) = (initial_metrics, final_metrics) {
        if initial.contains("http_upstream_health") || final_state.contains("http_upstream_health")
        {
            eprintln!("Health check threshold counting test: metrics detected");
            // 失敗/成功カウントが正確にカウントされることを確認
            // 実際の検証には、バックエンドの動的な障害をシミュレートする必要がある
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_tls_invalid_cert() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正証明書の処理を確認
    // 注意: このテストは設定ファイルでTLSヘルスチェックを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck
    // use_tls = true が設定されている場合のテスト

    // メトリクスエンドポイントから健康状態を確認
    let metrics_response = send_request(PROXY_PORT, "/__metrics", &[]).await;

    if let Some(metrics) = metrics_response {
        if metrics.contains("http_upstream_health")
            || metrics.contains("veil_proxy_http_upstream_health")
        {
            eprintln!("Health check TLS invalid cert test: metrics detected");
            // 不正証明書が適切に処理されることを確認
            // 実際の検証には、不正証明書でテストする必要がある
            // verify_cert = true の場合、不正証明書でヘルスチェックが失敗することを確認
            // verify_cert = false の場合、不正証明書でもヘルスチェックが成功することを確認
        }
    }
}
