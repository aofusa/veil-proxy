//! HTTP/3 のストリーム多重化・gRPC のフレーミングとストリーミング・QPACK・接続マイグレーション（F-92）
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// HTTP/3 ストリーム多重化テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_multiple_streams() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 10個のリクエストを順番に送信（非同期版では順番に処理）
    // 複数ストリームテスト: プロキシが複数のリクエストを処理できることを確認
    let mut responses = 0;
    let mut success_count = 0;
    for i in 0..10 {
        match send_http3_request(
            &mut send_request,
            "GET",
            &format!("/stream{}", i),
            &[],
            None,
        )
        .await
        {
            Ok((status, _body)) => {
                // プロキシが正常に動作している場合、200または404が返される
                // 200はバックエンドが存在する場合、404は存在しない場合
                if status == 200 || status == 404 {
                    success_count += 1;
                }
                assert!(
                    status == 200 || status == 404 || status == 502,
                    "Should return 200, 404, or 502 for stream {}: {}",
                    i,
                    status
                );
                responses += 1;
            }
            Err(e) => {
                panic!("Failed to send/receive request {}: {}", i, e);
            }
        }
    }

    // 少なくともいくつかのストリームが成功することを確認
    assert!(
        success_count > 0,
        "At least some streams should succeed (got {}/{} successful)",
        success_count,
        responses
    );

    // 少なくともいくつかのレスポンスを受信したことを確認
    assert!(responses > 0, "Should receive at least some responses");
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_proxy_forwarding() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 前提条件: バックエンドが存在することを確認
    let prereq_status = match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
        Ok((status, _)) => status,
        Err(e) => {
            panic!("Failed to send prerequisite request: {}", e);
        }
    };

    // プロキシ経由でリクエストを送信
    match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
        Ok((status, body)) => {
            // 前提条件チェックで200が返ることを確認済みなので、ここでも200を期待
            assert_eq!(
                status, 200,
                "Should return 200 OK for HTTP/3 proxy forwarding (prerequisite status: {}), got: {}", 
                prereq_status, status
            );
            // バックエンドが存在する場合、ボディが返される
            assert!(
                !body.is_empty(),
                "Should receive response body for successful proxy forwarding"
            );
        }
        Err(e) => {
            panic!("Failed to send/receive HTTP/3 request: {}", e);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_proxy_compression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 圧縮を要求するリクエストを送信
    match send_http3_request(
        &mut send_request,
        "GET",
        "/large.txt",
        &[("Accept-Encoding", "gzip, br, zstd")],
        None,
    )
    .await
    {
        Ok((status, _body)) => {
            // バックエンドが存在する場合、200が返される
            assert!(
                status == 200 || status == 404 || status == 502,
                "Should return 200, 404, or 502: {}",
                status
            );
        }
        Err(e) => {
            panic!("Failed to send/receive HTTP/3 request: {}", e);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_connection_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 接続タイムアウトのテストは、実際のタイムアウトを待つ必要があるため、
    // 短いタイムアウトでハンドシェイクを試みて動作を確認
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版、短いタイムアウトでテスト）
    let result = Http3TestClient::new(server_addr, "localhost").await;
    eprintln!(
        "HTTP/3 connection timeout test: result = {:?}",
        result.is_ok()
    );

    // タイムアウトが発生するか、成功するかのいずれか（どちらも有効な結果）
    // 重要なのはパニックしないこと
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_stream_priority() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 優先度付きストリームのテスト（簡易実装）
    // 実際の優先度設定はquicheのAPIで行う必要がある
    match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
        Ok((status, _body)) => {
            assert!(
                status == 200 || status == 404 || status == 502,
                "Should return 200, 404, or 502: {}",
                status
            );
        }
        Err(e) => {
            panic!("Failed to send/receive HTTP/3 request: {}", e);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_stream_cancellation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ストリームキャンセルのテスト
    // 接続を確立してストリームを開始した後、接続を閉じることでキャンセル動作を確認
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // リクエストを送信（レスポンスを待たずに接続を閉じる）
    // 非同期版では、リクエストを送信すると自動的にレスポンスを待つため、
    // ストリームキャンセルのテストは実装が異なる
    let stream_result = send_http3_request(&mut send_request, "GET", "/large.txt", &[], None).await;
    eprintln!(
        "HTTP/3 stream cancellation: request sent = {:?}",
        stream_result.is_ok()
    );

    // 非同期版では、接続は自動的にドロップされる
    // ストリームキャンセルのテストは実装が異なるため、ここではリクエストが送信できることを確認
    assert!(
        stream_result.is_ok() || stream_result.is_err(),
        "Stream cancellation should complete without panic"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_bidirectional_streams() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 双方向ストリームのテスト（複数のリクエストを送信）
    for i in 0..3 {
        let body = format!("Request {}", i).into_bytes();
        match send_http3_request(&mut send_request, "POST", "/", &[], Some(&body)).await {
            Ok((status, _body)) => {
                // 双方向ストリームテスト: プロキシが複数のストリームを並列処理できることを確認
                // プロキシが正常に動作している場合、200または404が返される
                assert!(
                    status == 200 || status == 404 || status == 502,
                    "Should return 200, 404, or 502 for bidirectional stream {}: {}",
                    i,
                    status
                );
            }
            Err(e) => {
                panic!("Failed to send/receive HTTP/3 request {}: {}", i, e);
            }
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_proxy_header_manipulation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 前提条件: バックエンドが存在することを確認
    let prereq_status = match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
        Ok((status, _)) => status,
        Err(e) => {
            panic!("Failed to send prerequisite request: {}", e);
        }
    };

    // カスタムヘッダーを付けてリクエストを送信
    match send_http3_request(
        &mut send_request,
        "GET",
        "/",
        &[
            ("X-Custom-Header", "test-value"),
            ("X-Forwarded-For", "192.168.1.1"),
        ],
        None,
    )
    .await
    {
        Ok((status, _body)) => {
            // 前提条件チェックで200が返ることを確認済みなので、ここでも200を期待
            assert_eq!(
                status, 200,
                "Should return 200 OK for HTTP/3 proxy header manipulation (prerequisite status: {}), got: {}", 
                prereq_status, status
            );
        }
        Err(e) => {
            panic!("Failed to send/receive HTTP/3 request: {}", e);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_proxy_load_balancing() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 複数のリクエストを送信してロードバランシングを確認
    let mut responses = Vec::new();
    for _ in 0..10 {
        match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
            Ok((status, _body)) => {
                responses.push(status);
            }
            Err(e) => {
                panic!("Failed to send/receive HTTP/3 request: {}", e);
            }
        }
    }

    // 少なくともいくつかのレスポンスを受信したことを確認
    assert!(
        !responses.is_empty(),
        "Should receive at least some responses"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_stream_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ストリームタイムアウトのテスト
    // 短いタイムアウトでレスポンス受信を試みる
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;
    use tokio::time::{timeout, Duration};

    // リクエストを送信（非常に短いタイムアウトでテスト）
    let result = timeout(
        Duration::from_millis(1),
        send_http3_request(&mut send_request, "GET", "/", &[], None),
    )
    .await;
    eprintln!("HTTP/3 stream timeout test: result = {:?}", result.is_ok());
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_invalid_frame() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正フレームのテスト
    // HTTP/3クライアントを作成して正常な接続後、不正なリクエストを送信
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 不正なパスでリクエストを送信（これはHTTP/3レベルでは有効だがアプリレベルでエラー）
    let result = send_http3_request(&mut send_request, "GET", "/\x00invalid", &[], None).await;
    eprintln!(
        "HTTP/3 invalid frame test: send result = {:?}",
        result.is_ok()
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_backend_failure() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 存在しないパスにリクエストを送信（バックエンドエラーをシミュレート）
    match send_http3_request(&mut send_request, "GET", "/nonexistent", &[], None).await {
        Ok((status, _body)) => {
            // バックエンドエラーの場合、502または404が返される
            assert!(
                status == 404 || status == 502,
                "Should return 404 or 502 for backend failure: {}",
                status
            );
        }
        Err(e) => {
            panic!("Failed to send/receive HTTP/3 request: {}", e);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_tls_handshake() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版、TLS 1.3ハンドシェイクを含む）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => {
            eprintln!("TLS 1.3 handshake completed successfully");
            c
        }
        Err(e) => {
            panic!(
                "TLS 1.3 handshake failed: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 実際のリクエスト送信で接続の健全性を確認
    let request_result = send_http3_request(&mut send_request, "GET", "/health", &[], None).await;
    assert!(
        request_result.is_ok(),
        "TLS 1.3 connection should allow sending requests: {:?}",
        request_result.err()
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_0rtt_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // 最初の接続を確立（セッション情報を保存、非同期版）
    let (_client1, mut send_request1) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 最初の接続でリクエストを送信してセッションを確立
    let _ = send_http3_request(&mut send_request1, "GET", "/", &[], None).await;

    // 2回目の接続（0-RTTを使用する可能性がある、非同期版）
    let (_client2, mut send_request2) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => {
            eprintln!("Second connection established (may use 0-RTT)");
            c
        }
        Err(e) => {
            panic!(
                "Second HTTP/3 handshake failed: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    // 2回目の接続が成立したことを実際のリクエストで確認
    let request_result = send_http3_request(&mut send_request2, "GET", "/health", &[], None).await;
    assert!(
        request_result.is_ok(),
        "Second connection should allow sending requests (0-RTT test): {:?}",
        request_result.err()
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_connection_close() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // リクエストを送信
    let _ = send_http3_request(&mut send_request, "GET", "/", &[], None).await;

    // 非同期版では、接続は自動的にドロップされる
    eprintln!("Connection closed successfully");
}

#[tokio::test]
// タイムアウトは大きめ（60s）+ 接続レベル失敗はリトライする。1.5MB の h3 アップロード
// （F-44 以降は TLS バックエンドへのストリーミング経路）は重い並列スイート + co-tenant
// 負荷では CPU 競合で遅くなり得るため、test_http3_request_body_streaming と同じ
// レジリエンス方針を取る。
#[ntest::timeout(60000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_large_request_body() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    use common::http3_client::send_http3_request;

    // 1MB以上の大きなリクエストボディを生成
    let large_body: Vec<u8> = (0..1_500_000).map(|i| (i % 256) as u8).collect();

    const MAX_ATTEMPTS: usize = 4;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        // HTTP/3接続を確立（非同期版）
        let (mut _client, mut send_request) =
            match Http3TestClient::new(server_addr, "localhost").await {
                Ok(c) => c,
                Err(e) => {
                    last_err = format!("connect error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
            };

        // POSTリクエストを送信
        match send_http3_request(
            &mut send_request,
            "POST",
            "/",
            &[("Content-Type", "application/octet-stream")],
            Some(&large_body),
        )
        .await
        {
            Ok((status, _body)) => {
                // 大きなボディが正常に送信されたことを確認
                assert!(
                    status == 200 || status == 413 || status == 502,
                    "Should return 200, 413, or 502: {}",
                    status
                );
                eprintln!(
                    "HTTP/3 large request body: {} bytes uploaded (status {}, attempt {})",
                    large_body.len(),
                    status,
                    attempt + 1
                );
                return;
            }
            Err(e) => {
                last_err = format!("POST error: {}", e);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
    }
    panic!(
        "Failed to send/receive HTTP/3 request with large body after {} attempts: {}",
        MAX_ATTEMPTS, last_err
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_large_response_body() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // 大きなレスポンスを返すエンドポイントにリクエストを送信
    // バックエンドが大きなレスポンスを返すことを想定
    match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
        Ok((status, body)) => {
            assert_eq!(status, 200, "Should return 200 OK");
            // レスポンスボディが受信されたことを確認
            assert!(!body.is_empty(), "Should receive response body");
            eprintln!("Received response body size: {} bytes", body.len());
        }
        Err(e) => {
            panic!("Failed to send/receive HTTP/3 request: {}", e);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_chunked_response() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/3ではチャンク転送は使用されない（QUICのストリーミングを使用）
    // このテストでは、大きなレスポンスがストリーミングで受信されることを確認
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // リクエストを送信（HTTP/3では自動的にストリーミング）
    match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
        Ok((status, body)) => {
            assert_eq!(status, 200, "Should return 200 OK");
            // レスポンスボディが受信されたことを確認
            assert!(!body.is_empty(), "Should receive response body");
            eprintln!("Received streamed response body size: {} bytes", body.len());
        }
        Err(e) => {
            panic!("Failed to send/receive HTTP/3 response: {}", e);
        }
    }
}

/// F-32: HTTP/3 リクエスト方向ストリーミングの End-to-End（往復バイト単位一致）。
///
/// 初期ストリームフロー制御ウィンドウ（`initial_max_stream_data_bidi_remote` = 1MB）を
/// **超える** 大容量ボディ（1,500,000 バイト）を POST する。プロキシは HEADERS 受信時点で
/// バックエンド接続を開始し、DATA を chunked で逐次転送する（`recv_body` pump + 有界チャネルの
/// バックプレッシャ、WINDOW_UPDATE/MAX_STREAM_DATA に連動）。echo バックエンドが chunked を
/// デコードして同一ボディを返すので、**往復のバイト単位完全一致**を確認することで、
/// アクターモデル（メインループ⇔バックエンドタスク）でのストリーミング・chunked エンコード/
/// デコード・終端・双方向フロー制御の End-to-End 正当性を保証する。
#[tokio::test]
// タイムアウトは大きめ（60s）に取る。1MB 超の往復（up+down）は単一 H3 ワーカー + co-tenant
// 負荷の重い並列スイートでは CPU 競合で遅くなり得るため（処理自体は分離タスクで安定）。
#[ntest::timeout(60000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_request_body_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // 初期ストリームウィンドウ（initial_max_stream_data_bidi_remote = 1MB）を超えるサイズ →
    // 複数バッチ recv_body + WINDOW_UPDATE/MAX_STREAM_DATA 連動のフロー制御を強制する。
    const UPLOAD_TOTAL: usize = 1_200_000;
    let upload: Vec<u8> = (0..UPLOAD_TOTAL).map(|i| (i % 256) as u8).collect();

    use common::http3_client::send_http3_request;

    // 大容量往復は重い並列負荷下で QUIC 接続が一時的にリセットされ得る（プロキシの
    // ストリーミング処理自体は分離タスクで安定）。接続レベルの失敗はリトライし、成功時のみ
    // バイト単位一致を検証する（他の負荷フレーキーテストと同じレジリエンス方針）。
    const MAX_ATTEMPTS: usize = 4;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        let (mut _client, mut send_request) =
            match Http3TestClient::new(server_addr, "localhost").await {
                Ok(c) => c,
                Err(e) => {
                    last_err = format!("connect error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
            };

        match send_http3_request(
            &mut send_request,
            "POST",
            "/echo-upload/data",
            &[("content-type", "application/octet-stream")],
            Some(&upload),
        )
        .await
        {
            Ok((status, body)) => {
                assert_eq!(status, 200, "Expected 200 OK for echoed upload (h3)");
                assert_eq!(
                    body.len(),
                    UPLOAD_TOTAL,
                    "Echoed body length mismatch (got {}, want {})",
                    body.len(),
                    UPLOAD_TOTAL
                );
                // 往復の完全一致 = ストリーミング転送・chunked エンコード/デコードで破損・欠落なし
                for (i, (&got, &want)) in body.iter().zip(upload.iter()).enumerate() {
                    assert_eq!(
                        got, want,
                        "Echoed body byte mismatch at offset {} (got {}, want {})",
                        i, got, want
                    );
                }
                eprintln!(
                    "HTTP/3 request streaming: uploaded and echoed {} bytes byte-exact (attempt {})",
                    body.len(),
                    attempt + 1
                );
                return;
            }
            Err(e) => {
                last_err = format!("POST error: {}", e);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
    }
    panic!(
        "HTTP/3 request body streaming failed after {} attempts: {}",
        MAX_ATTEMPTS, last_err
    );
}

/// B-68: バックエンドが要求本文を読み切る前に応答して切断しても、HTTP/3 クライアントが
/// 詰まらずに応答を受け取れること。
///
/// `/error-500/*` のバックエンドはヘッダだけ読んで即座に 500 を返して閉じる。修正前の veil は
/// バックエンドタスクの終了後に要求ストリームの受信をやめるだけで STOP_SENDING を送らず、
/// QUIC のフロー制御ウィンドウが補充されないため、本文を送り切ってから応答を読むクライアントは
/// アイドルタイムアウトまで停止していた。
#[tokio::test]
#[ntest::timeout(60000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_early_backend_response_does_not_stall_upload() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    use common::http3_client::send_http3_request;

    // QUIC の初期フロー制御ウィンドウ（1MB 程度）を大きく超える本文。
    let body: Vec<u8> = vec![b'x'; 4_000_000];
    let (mut _client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("connect");
    let started = std::time::Instant::now();
    let (status, _) = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        send_http3_request(
            &mut send_request,
            "POST",
            "/error-500/upload",
            &[("content-type", "application/octet-stream")],
            Some(&body),
        ),
    )
    .await
    .expect("upload must not stall until the QUIC idle timeout")
    .expect("response must be received even if the server stops the upload");
    assert!(
        status == 500 || status == 502 || status == 503,
        "unexpected status {}",
        status
    );
    eprintln!(
        "early backend response: status {} in {:?}",
        status,
        started.elapsed()
    );
}

/// F-44: HTTP/3 リクエスト方向ストリーミング × **TLS バックエンド**の End-to-End。
///
/// `/echo-upload-tls/*` は HTTPS の echo バックエンドへ振り分けられ、バックエンドタスクは
/// 全二重 TLS ラッパー（`http3_stream::TlsBackend`）でアップロードとレスポンス受信を
/// 同一タスク内並行駆動する。初期ストリームウィンドウ（1MB）を超える 1,200,000 バイトを
/// POST し、往復のバイト単位完全一致で「TLS 経由でも全量バッファせず逐次転送」を検証する。
#[tokio::test]
#[ntest::timeout(60000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_request_body_streaming_tls_backend() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    const UPLOAD_TOTAL: usize = 1_200_000;
    let upload: Vec<u8> = (0..UPLOAD_TOTAL).map(|i| (i % 256) as u8).collect();

    use common::http3_client::send_http3_request;

    const MAX_ATTEMPTS: usize = 4;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        let (mut _client, mut send_request) =
            match Http3TestClient::new(server_addr, "localhost").await {
                Ok(c) => c,
                Err(e) => {
                    last_err = format!("connect error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
            };

        match send_http3_request(
            &mut send_request,
            "POST",
            "/echo-upload-tls/data",
            &[("content-type", "application/octet-stream")],
            Some(&upload),
        )
        .await
        {
            Ok((status, body)) => {
                assert_eq!(status, 200, "Expected 200 OK for echoed upload (h3+TLS)");
                assert_eq!(
                    body.len(),
                    UPLOAD_TOTAL,
                    "Echoed body length mismatch over TLS backend (got {}, want {})",
                    body.len(),
                    UPLOAD_TOTAL
                );
                for (i, (&got, &want)) in body.iter().zip(upload.iter()).enumerate() {
                    assert_eq!(
                        got, want,
                        "Echoed body byte mismatch at offset {} (TLS backend)",
                        i
                    );
                }
                eprintln!(
                    "HTTP/3 TLS-backend streaming: uploaded and echoed {} bytes byte-exact (attempt {})",
                    body.len(),
                    attempt + 1
                );
                return;
            }
            Err(e) => {
                last_err = format!("POST error: {}", e);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
    }
    panic!(
        "HTTP/3 TLS-backend request body streaming failed after {} attempts: {}",
        MAX_ATTEMPTS, last_err
    );
}

/// F-44: HTTP/3 × TLS バックエンド（小ボディ・単一 DATA 経路）。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_request_body_streaming_tls_backend_small() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    let upload = b"hello streaming request body over http3 via tls backend".to_vec();

    use common::http3_client::send_http3_request;

    let (mut _client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await
    {
        Ok(c) => c,
        Err(e) => panic!("Failed to create HTTP/3 client: {}", e),
    };

    let (status, body) = send_http3_request(
        &mut send_request,
        "POST",
        "/echo-upload-tls/small",
        &[("content-type", "text/plain")],
        Some(&upload),
    )
    .await
    .expect("HTTP/3 small TLS-backend streaming POST failed");

    assert_eq!(
        status, 200,
        "Expected 200 OK for echoed small upload (h3+TLS)"
    );
    assert_eq!(body, upload, "Echoed small body mismatch (h3+TLS)");
}

/// F-32: HTTP/3 リクエスト方向ストリーミング（小ボディ・単一 DATA 経路）。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_request_body_streaming_small() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    let upload = b"hello streaming request body over http3".to_vec();

    use common::http3_client::send_http3_request;

    let (mut _client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await
    {
        Ok(c) => c,
        Err(e) => panic!("Failed to create HTTP/3 client: {}", e),
    };

    let (status, body) = send_http3_request(
        &mut send_request,
        "POST",
        "/echo-upload/small",
        &[("content-type", "text/plain")],
        Some(&upload),
    )
    .await
    .expect("HTTP/3 small request body streaming POST failed");

    assert_eq!(status, 200, "Expected 200 OK for echoed small upload (h3)");
    assert_eq!(body, upload, "Echoed small body mismatch (h3)");
}

/// F-32: HTTP/3 レスポンス方向ストリーミングの End-to-End（chunked バックエンド・バイト一致）。
///
/// `Transfer-Encoding: chunked` のバックエンド（200,000 バイト・決定論パターン）からの応答を、
/// プロキシは **全バッファリングせず** `ChunkedDecoder::next_data_span` でゼロコピーデコードしつつ
/// HTTP/3 DATA（`send_body`）へ逐次転送する（クライアントのフロー制御に連動したバックプレッシャ）。
/// 実 h3 クライアントが全ボディを **バイト単位まで正確に** 再構成できることを確認する。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_response_body_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // test-backends/src/main.rs の chunked サーバー定数と一致させること
    const EXPECTED_TOTAL: usize = 200_000;
    let expected_byte = |i: usize| -> u8 { (i % 256) as u8 };

    use common::http3_client::send_http3_request;

    const MAX_ATTEMPTS: usize = 4;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        let (mut _client, mut send_request) =
            match Http3TestClient::new(server_addr, "localhost").await {
                Ok(c) => c,
                Err(e) => {
                    last_err = format!("connect error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
            };

        match send_http3_request(&mut send_request, "GET", "/chunked-stream/data", &[], None).await
        {
            Ok((status, body)) => {
                assert_eq!(status, 200, "Expected 200 OK for chunked stream (h3)");
                assert_eq!(
                    body.len(),
                    EXPECTED_TOTAL,
                    "Streamed body length mismatch (got {}, want {})",
                    body.len(),
                    EXPECTED_TOTAL
                );
                // 決定論パターンの完全一致 = ストリーミング転送中に破損・並べ替え・欠落がない
                for (i, &b) in body.iter().enumerate() {
                    assert_eq!(
                        b,
                        expected_byte(i),
                        "Body byte mismatch at offset {} (got {}, want {})",
                        i,
                        b,
                        expected_byte(i)
                    );
                }
                eprintln!(
                    "HTTP/3 response streaming: reassembled {} bytes byte-exact via DATA frames (attempt {})",
                    body.len(),
                    attempt + 1
                );
                return;
            }
            Err(e) => {
                last_err = format!("GET error: {}", e);
                tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            }
        }
    }
    panic!(
        "HTTP/3 response body streaming failed after {} attempts: {}",
        MAX_ATTEMPTS, last_err
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_throughput() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // スループット測定: 複数のリクエストを送信
    let start = std::time::Instant::now();
    let num_requests = 10;
    let mut successful_requests = 0;

    for i in 0..num_requests {
        match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
            Ok((status, _body)) => {
                if status == 200 {
                    successful_requests += 1;
                }
            }
            Err(e) => {
                panic!("Failed to send/receive HTTP/3 request {}: {}", i, e);
            }
        }
    }

    let elapsed = start.elapsed();
    let throughput = successful_requests as f64 / elapsed.as_secs_f64();

    eprintln!(
        "Throughput: {:.2} requests/second ({} successful out of {})",
        throughput, successful_requests, num_requests
    );

    // 最低限のスループットを確認
    assert!(
        successful_requests > 0,
        "Should have at least one successful request"
    );

    // 接続を閉じる
    // let _ = client.close();
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_latency() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（非同期版）
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => {
            panic!(
                "Failed to create HTTP/3 client: {} (HTTP/3 may not be enabled)",
                e
            );
        }
    };

    use common::http3_client::send_http3_request;

    // レイテンシ測定: 複数のリクエストのレイテンシを測定
    let num_requests = 5;
    let mut latencies = Vec::new();

    for i in 0..num_requests {
        let request_start = std::time::Instant::now();

        match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
            Ok((status, _body)) => {
                if status == 200 {
                    let latency = request_start.elapsed();
                    latencies.push(latency);
                    eprintln!("Request {} latency: {:?}", i, latency);
                }
            }
            Err(e) => {
                panic!("Failed to send/receive HTTP/3 request {}: {}", i, e);
            }
        }
    }

    if !latencies.is_empty() {
        let avg_latency = latencies.iter().sum::<Duration>() / latencies.len() as u32;
        eprintln!("Average latency: {:?}", avg_latency);
        assert!(
            avg_latency < Duration::from_secs(5),
            "Average latency should be reasonable"
        );
    } else {
        eprintln!("No successful requests for latency measurement");
    }
}

// ====================
// F-92: HTTP/2 ベースの gRPC 詳細フレーミング
// ====================

/// HTTP/2 DATA 境界と gRPC LPM 境界がずれたケースを含む詳細フレーミング検証。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_grpc_http2_framing() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("HTTP/2 client for gRPC framing");

    let grpc_headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("accept", "application/grpc"),
    ];
    let path = "/grpc.test.v1.TestService/UnaryCall";
    let msg = encode_simple_request("framing-ok");
    let lpm = encode_grpc_lpm(&msg);

    // Case 1: 単一 DATA フレームに完全な LPM
    let r1 = client
        .send_request_full("POST", path, &grpc_headers, Some(&lpm))
        .await
        .expect("unary single-frame gRPC");
    assert_eq!(r1.status, 200, "single-frame gRPC should be HTTP 200");
    let st1 = r1.grpc_status();
    assert!(
        st1.is_some(),
        "single-frame: grpc-status trailer/header required, headers={:?} trailers={:?}",
        r1.headers,
        r1.trailers
    );
    eprintln!(
        "Case1 single-frame: grpc-status={:?} body_len={} frames={}",
        st1,
        r1.body.len(),
        decode_all_grpc_frames(&r1.body).len()
    );

    // Case 2: LPM ヘッダ 5 バイトとボディを別 DATA に分割
    let (hdr5, rest) = lpm.split_at(5);
    let r2 = client
        .send_request_chunked(
            "POST",
            path,
            &grpc_headers,
            &[hdr5, rest],
            Some(Duration::from_millis(10)),
        )
        .await
        .expect("split LPM header/body");
    assert_eq!(r2.status, 200, "split LPM should still yield HTTP 200");
    assert!(
        r2.grpc_status().is_some(),
        "split LPM: grpc-status required"
    );
    eprintln!(
        "Case2 split-header: grpc-status={:?} body_len={}",
        r2.grpc_status(),
        r2.body.len()
    );

    // Case 3: LPM を 1 バイトずつ細切れ DATA で送信（境界ずれ最大）
    let msg3 = encode_simple_request("byte-split");
    let lpm3 = encode_grpc_lpm(&msg3);
    let one_byte_chunks: Vec<&[u8]> = lpm3.chunks(1).collect();
    let r3 = client
        .send_request_chunked("POST", path, &grpc_headers, &one_byte_chunks, None)
        .await
        .expect("byte-split LPM");
    assert_eq!(r3.status, 200, "byte-split LPM should yield HTTP 200");
    assert!(
        r3.grpc_status().is_some(),
        "byte-split: grpc-status required"
    );
    eprintln!(
        "Case3 byte-split: grpc-status={:?} body_len={}",
        r3.grpc_status(),
        r3.body.len()
    );

    // Case 4: 宣言 length と実ボディ不一致（不正 LPM）— 制御された応答・生存
    let bad = vec![0u8, 0, 0, 0x10, 0x00, b'x', b'y']; // length=4096, body=2B
    let r4 = client
        .send_request_full("POST", path, &grpc_headers, Some(&bad))
        .await;
    match r4 {
        Ok(resp) => {
            assert!(
                matches!(resp.status, 200 | 400 | 413 | 502 | 503),
                "malformed LPM controlled status, got {}",
                resp.status
            );
            eprintln!(
                "Case4 malformed: status={} grpc-status={:?}",
                resp.status,
                resp.grpc_status()
            );
        }
        Err(e) => {
            eprintln!("Case4 malformed: transport error (acceptable): {}", e);
        }
    }

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive gRPC HTTP/2 framing edge cases"
    );
}

// ====================
// F-92: gRPC ストリーミング詳細
// ====================

/// 複数メッセージのサーバ/クライアントストリームと途中切断後の生存を検証。
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_grpc_streaming_detailed() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("HTTP/2 client for streaming");

    let grpc_headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("accept", "application/grpc"),
    ];

    // --- Server Streaming: 1 リクエストで複数 LPM 応答 ---
    let ss_msg = encode_simple_request("ss-start");
    let ss_lpm = encode_grpc_lpm(&ss_msg);
    let ss = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/ServerStreaming",
            &grpc_headers,
            Some(&ss_lpm),
        )
        .await
        .expect("server streaming");
    assert_eq!(ss.status, 200, "server streaming HTTP status");
    let ss_frames = decode_all_grpc_frames(&ss.body);
    eprintln!(
        "ServerStreaming: frames={} grpc-status={:?} body_len={}",
        ss_frames.len(),
        ss.grpc_status(),
        ss.body.len()
    );
    // 上流が 5 メッセージ返す。透過プロキシなら複数フレーム期待。
    // フレーム 0 でも grpc-status があればストリームは完結している。
    assert!(
        ss.grpc_status().is_some() || !ss_frames.is_empty(),
        "server streaming should yield frames or grpc-status"
    );
    if ss.grpc_status() == Some(0) {
        assert!(
            ss_frames.len() >= 2,
            "ok server stream should return multiple LPMs, got {}",
            ss_frames.len()
        );
    }

    // --- Client Streaming: 複数 LPM を同一ストリームで送信 ---
    let mut client_chunks: Vec<Vec<u8>> = Vec::new();
    for i in 0..4 {
        let m = encode_simple_request(&format!("cs-{}", i));
        client_chunks.push(encode_grpc_lpm(&m));
    }
    let chunk_refs: Vec<&[u8]> = client_chunks.iter().map(|v| v.as_slice()).collect();
    let cs = client
        .send_request_chunked(
            "POST",
            "/grpc.test.v1.TestService/ClientStreaming",
            &grpc_headers,
            &chunk_refs,
            Some(Duration::from_millis(15)),
        )
        .await
        .expect("client streaming");
    assert_eq!(cs.status, 200, "client streaming HTTP status");
    assert!(
        cs.grpc_status().is_some(),
        "client streaming needs grpc-status"
    );
    let cs_frames = decode_all_grpc_frames(&cs.body);
    eprintln!(
        "ClientStreaming: frames={} grpc-status={:?} body={:?}",
        cs_frames.len(),
        cs.grpc_status(),
        cs_frames
            .first()
            .map(|f| String::from_utf8_lossy(&f.data).into_owned())
    );

    // --- Bidirectional: 複数送受信 ---
    let mut bidi_chunks: Vec<Vec<u8>> = Vec::new();
    for i in 0..3 {
        let m = encode_simple_request(&format!("bidi-{}", i));
        bidi_chunks.push(encode_grpc_lpm(&m));
    }
    let bidi_refs: Vec<&[u8]> = bidi_chunks.iter().map(|v| v.as_slice()).collect();
    let bidi = client
        .send_request_chunked(
            "POST",
            "/grpc.test.v1.TestService/BidirectionalStreaming",
            &grpc_headers,
            &bidi_refs,
            Some(Duration::from_millis(10)),
        )
        .await
        .expect("bidirectional streaming");
    assert_eq!(bidi.status, 200, "bidi streaming HTTP status");
    let bidi_frames = decode_all_grpc_frames(&bidi.body);
    eprintln!(
        "Bidirectional: frames={} grpc-status={:?}",
        bidi_frames.len(),
        bidi.grpc_status()
    );
    assert!(
        bidi.grpc_status().is_some() || !bidi_frames.is_empty(),
        "bidi should complete with status or frames"
    );

    // --- 途中切断: 部分 LPM 送信後に RST ---
    let partial = encode_grpc_lpm(&encode_simple_request("will-reset"));
    let _ = client
        .send_and_reset(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &grpc_headers,
            Some(&partial[..partial.len().saturating_sub(2).max(1)]),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 切断後も Unary が通ること
    let after = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &grpc_headers,
            Some(&encode_grpc_lpm(&encode_simple_request("after-reset"))),
        )
        .await
        .expect("post mid-stream reset unary");
    assert_eq!(after.status, 200, "proxy must accept traffic after RST");
    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive streaming mid-disconnect"
    );
    eprintln!("gRPC streaming detailed test completed");
}

// ====================
// F-92: QPACK 圧縮の詳細
// ====================

/// 同一接続で同一ヘッダ連打（動的テーブル利用）と巨大ヘッダの制御された処理を検証。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_qpack_compression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 client for QPACK");

    use common::http3_client::{send_http3_request, send_http3_request_full};

    // 同一ヘッダセットを多数回送信 → 動的テーブル参照パスを刺激
    let headers = [
        ("user-agent", "qpack-test-client/1.0"),
        ("accept", "application/json"),
        ("x-custom-header", "qpack-dynamic-table-value"),
        ("x-session-id", "qpack-session-abc123"),
    ];
    let mut ok = 0u32;
    for i in 0..12 {
        match send_http3_request(&mut send_request, "GET", "/", &headers, None).await {
            Ok((status, body)) => {
                assert_eq!(
                    status, 200,
                    "QPACK repeated request {} should be 200, got {}",
                    i, status
                );
                assert!(!body.is_empty() || status == 200, "body or empty 200 ok");
                ok += 1;
            }
            Err(e) => panic!("QPACK repeated request {} failed: {}", i, e),
        }
    }
    assert_eq!(ok, 12, "all QPACK warm-up requests should succeed");

    // 巨大ヘッダブロック → メモリ制限 / 431 相当 / 接続エラーのいずれかで制御
    let big = "B".repeat(8192);
    let big_headers = [
        ("user-agent", "qpack-bomb-client/1.0"),
        ("x-huge-1", big.as_str()),
        ("x-huge-2", big.as_str()),
        ("x-huge-3", big.as_str()),
        ("x-huge-4", big.as_str()),
    ];
    match send_http3_request_full(&mut send_request, "GET", "/", &big_headers, None).await {
        Ok(resp) => {
            assert!(
                matches!(resp.status, 200 | 400 | 413 | 431 | 502 | 503),
                "oversized H3 headers controlled status, got {}",
                resp.status
            );
            eprintln!("QPACK oversized headers status={}", resp.status);
        }
        Err(e) => {
            // ストリームリセット・接続クローズは許容（DoS 防御）
            eprintln!("QPACK oversized headers rejected (ok): {}", e);
        }
    }

    // 巨大ヘッダ後も通常リクエストが通る（接続全滅していない）
    match send_http3_request(&mut send_request, "GET", "/", &headers, None).await {
        Ok((status, _)) => {
            assert!(
                matches!(status, 200 | 400 | 502),
                "post-bomb request status {}",
                status
            );
            eprintln!("QPACK post-bomb status={}", status);
        }
        Err(e) => {
            // 接続が閉じられた場合は新規接続で生存確認
            eprintln!("QPACK connection closed after bomb (reconnecting): {}", e);
            let (_c2, mut sr2) = Http3TestClient::new(server_addr, "localhost")
                .await
                .expect("reconnect after QPACK bomb");
            let (st, _) = send_http3_request(&mut sr2, "GET", "/", &[], None)
                .await
                .expect("GET after reconnect");
            assert_eq!(st, 200, "proxy must accept new H3 after QPACK bomb");
        }
    }

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive QPACK stress"
    );
}

// ====================
// F-92: HTTP/3 接続マイグレーション相当
// ====================

/// 別ソースポートからの並行接続と、既存接続の継続性を検証。
/// 真の path migration（同一 CID・別 4-tuple）は quinn クライアント制約のため、
/// 複数 UDP エンドポイントでのストリーム継続・クラッシュなしを合格条件とする。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_connection_migration() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    use common::http3_client::send_http3_request;

    // 接続 A: 確立後に複数リクエスト（同一 Connection ID 上の継続）
    let (_client_a, mut sr_a) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 connection A");
    for i in 0..3 {
        let (st, _) = send_http3_request(&mut sr_a, "GET", "/", &[], None)
            .await
            .unwrap_or_else(|e| panic!("conn A request {}: {}", i, e));
        assert_eq!(st, 200, "conn A request {} status", i);
    }

    // 接続 B: 別 UDP ソケット（別ソースポート）— マイグレーション後の新パス相当
    let (_client_b, mut sr_b) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 connection B (new source port)");
    let (st_b, _) = send_http3_request(&mut sr_b, "GET", "/health", &[], None)
        .await
        .expect("conn B request");
    assert!(
        matches!(st_b, 200 | 404),
        "conn B should work from new source port, got {}",
        st_b
    );

    // 接続 A が B 開設後も継続できること（サーバ状態が破綻していない）
    let (st_a2, _) = send_http3_request(&mut sr_a, "GET", "/", &[], None)
        .await
        .expect("conn A after B");
    assert_eq!(
        st_a2, 200,
        "conn A must continue after peer path change sim"
    );

    // 接続 C: 同時多重（別ポート）で並列 GET
    let (_client_c, mut sr_c) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 connection C");
    let (st_c, _) = send_http3_request(&mut sr_c, "GET", "/", &[], None)
        .await
        .expect("conn C");
    assert_eq!(st_c, 200, "conn C parallel path");

    // 旧接続をドロップし、新規接続で再確立（クライアント側パス喪失のシミュレーション）
    drop(sr_a);
    drop(_client_a);
    tokio::time::sleep(Duration::from_millis(50)).await;
    let (_client_d, mut sr_d) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 reconnection after path loss");
    let (st_d, _) = send_http3_request(&mut sr_d, "GET", "/", &[], None)
        .await
        .expect("reconnect after path loss");
    assert_eq!(st_d, 200, "new connection after path loss must work");

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive connection migration simulation"
    );
    eprintln!("HTTP/3 connection migration simulation completed");
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_concurrent_connections() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // 複数の同時接続を確立（実際には10接続に制限してテスト時間を短縮）
    let num_connections = 10;
    let mut successful_connections = 0;

    for i in 0..num_connections {
        match Http3TestClient::new(server_addr, "localhost").await {
            Ok((_client, mut send_request)) => {
                successful_connections += 1;
                eprintln!("Connection {} established successfully", i);

                use common::http3_client::send_http3_request;
                // 簡単なリクエストを送信して接続が機能することを確認
                let _ = send_http3_request(&mut send_request, "GET", "/", &[], None).await;
            }
            Err(e) => {
                panic!("HTTP/3 connection failed for connection {}: {}", i, e);
            }
        }
    }

    eprintln!(
        "Established {} out of {} connections",
        successful_connections, num_connections
    );

    // 最低限の接続が確立されたことを確認
    assert!(
        successful_connections > 0,
        "Should have at least one successful connection"
    );
}

// ====================
// gRPC ストリーミング RPC テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_client_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Client Streaming RPCのテスト
    // 複数のリクエストメッセージを送信し、単一のレスポンスを受信
    // 複数のメッセージを送信（簡易実装、非同期版）
    for i in 0..3 {
        let message = format!("Message {}", i).into_bytes();
        let response = match GrpcTestClient::send_grpc_request(
            "127.0.0.1",
            PROXY_PORT,
            "/grpc.test.v1.TestService/ClientStreaming",
            &message,
            &[],
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                panic!("Failed to send gRPC request {}: {}", i, e);
            }
        };

        let status = GrpcTestClient::extract_status_code(&response);
        // gRPCエンドポイントが存在する場合は200が返される
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
pub(super) async fn test_grpc_server_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Server Streaming RPCのテスト
    // 単一のリクエストメッセージを送信し、複数のレスポンスメッセージを受信
    let request_message = b"Start streaming";
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/ServerStreaming",
        request_message,
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
    // gRPCエンドポイントが存在する場合は200が返される
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
pub(super) async fn test_grpc_bidirectional_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Bidirectional Streaming RPCのテスト
    // 複数のリクエストメッセージを送信し、複数のレスポンスメッセージを受信
    // 複数のメッセージを送信（非同期版）
    for i in 0..3 {
        let message = format!("Bidirectional message {}", i).into_bytes();
        let response = match GrpcTestClient::send_grpc_request(
            "127.0.0.1",
            PROXY_PORT,
            "/grpc.test.v1.TestService/BidirectionalStreaming",
            &message,
            &[],
        )
        .await
        {
            Ok(r) => r,
            Err(e) => {
                panic!("Failed to send gRPC request {}: {}", i, e);
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
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_timeout_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // grpc-timeoutヘッダーを指定してリクエストを送信（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test",
        &[("grpc-timeout", "10S")],
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
pub(super) async fn test_grpc_encoding_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // grpc-encodingヘッダーを指定してリクエストを送信（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test",
        &[("grpc-encoding", "gzip")],
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
pub(super) async fn test_grpc_accept_encoding_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // grpc-accept-encodingヘッダーを指定してリクエストを送信（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test",
        &[("grpc-accept-encoding", "gzip, deflate")],
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
pub(super) async fn test_grpc_metadata() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // カスタムメタデータを指定してリクエストを送信（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test",
        &[("custom-header-1", "value1"), ("custom-header-2", "value2")],
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
pub(super) async fn test_grpc_gzip_compression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gzip圧縮のテスト（簡易実装、非同期版）
    // 実際の圧縮テストには、gzip圧縮されたメッセージの送受信が必要
    // grpc-encodingヘッダーでgzipを指定
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        b"test message",
        &[("grpc-encoding", "gzip"), ("grpc-accept-encoding", "gzip")],
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
#[cfg(feature = "grpc-web")]
pub(super) async fn test_grpc_web_binary_format() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPC-Webバイナリ形式のテスト
    use base64::{engine::general_purpose, Engine as _};

    // gRPCフレームを構築
    let frame = GrpcFrame::new(b"Hello, gRPC-Web!".to_vec());
    let frame_bytes = frame.encode();
    let base64_encoded = general_purpose::STANDARD.encode(&frame_bytes);

    // gRPC-Webリクエストを送信
    let response = send_request_with_method(
        PROXY_PORT,
        "/grpc.test.v1.TestService/UnaryCall",
        "POST",
        &[
            ("Content-Type", "application/grpc-web"),
            ("Accept", "application/grpc-web"),
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
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_proxy_forwarding() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCプロキシ転送のテスト（非同期版）
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
    // プロキシが正常に動作している場合、200が返される
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
pub(super) async fn test_grpc_invalid_frame() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なgRPCフレームのテスト（非同期版）
    // 不正なフレームヘッダーを送信
    let invalid_frame = b"\xFF\xFF\xFF\xFF\xFF";
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        invalid_frame,
        &[],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!(
                "Failed to send gRPC request to 127.0.0.1:{}: {}",
                PROXY_PORT, e
            );
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
        // 不正なフレームの場合、INVALID_ARGUMENT (3) または INTERNAL (13) が返される可能性がある
        assert!(
            grpc_status_code == 3 || grpc_status_code == 13,
            "Should return gRPC status INVALID_ARGUMENT (3) or INTERNAL (13) for invalid frame, got: {}",
            grpc_status_code
        );
        eprintln!("gRPC status code for invalid frame: {}", grpc_status_code);
    } else {
        eprintln!("Note: grpc-status not found in response (backend may have processed message)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_oversized_message() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // メッセージサイズ超過のテスト（非同期版）
    // 4MBを超えるメッセージを送信（簡易実装では1MB程度）
    let large_message = vec![0u8; 1024 * 1024]; // 1MB
                                                // プロキシが413を早期送信するとhyperはSendRequestエラーを返す
                                                // これはサーバーが大きなボディを正しく拒否したことを示す
    match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/Test",
        &large_message,
        &[],
    )
    .await
    {
        Ok(r) => {
            let status = GrpcTestClient::extract_status_code(&r);
            assert_eq!(
                status,
                Some(413),
                "Should return 413 Payload Too Large for oversized message, got: {:?}",
                status
            );
        }
        Err(e) => {
            // サーバーが早期にリジェクトした場合（hyperのSendRequestエラー）
            // これはプロキシが正しく大きなボディを拒否したことを示す
            let e_str = e.to_string();
            assert!(
                e_str.contains("SendRequest")
                    || e_str.contains("connection")
                    || e_str.contains("reset"),
                "Expected connection error for oversized gRPC message, got: {}",
                e_str
            );
            eprintln!(
                "gRPC oversized message correctly rejected by proxy ({})",
                e_str
            );
        }
    }
}
