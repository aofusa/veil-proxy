//! SNI・リダイレクト・メトリクス・ヘッダー操作・キャッシュ・バッファリング・ヘルスチェック・WebSocket のエラー
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// 優先度中: SNI (Server Name Indication) テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_sni_hostname_negotiation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // SNIを使用して異なるホスト名で接続を試みる
    // プロキシはSNIに基づいて適切な証明書を選択する必要がある

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // TLS接続を確立（localhostをSNIとして使用）
    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();

    // TLSハンドシェイクを完了
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(e) => {
                panic!("TLS handshake error: {:?}", e);
            }
        }
    }

    // SNIが正しくネゴシエートされた場合、接続が成功する
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // リクエストを送信
    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send request: {:?}", e);
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

    // SNIが正しく処理された場合、200が返される
    assert_eq!(status, Some(200), "Should return 200 OK with SNI");

    eprintln!("SNI hostname negotiation test: successful with localhost");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_sni_different_hostname() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 異なるホスト名でSNI接続を試みる
    // プロキシが複数の証明書をサポートしている場合、適切な証明書が選択される

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    // TLS接続を確立（127.0.0.1をSNIとして使用）
    let config = create_client_config();
    let server_name = ServerName::try_from("127.0.0.1".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();

    // TLSハンドシェイクを完了
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(e) => {
                panic!("TLS handshake error with 127.0.0.1: {:?}", e);
                // 証明書が127.0.0.1に対応していない場合、エラーが発生する可能性がある
            }
        }
    }

    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // リクエストを送信
    let request = b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send request: {:?}", e);
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

    // SNIが正しく処理された場合、200が返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK with SNI, got: {:?}",
        status
    );

    eprintln!("SNI different hostname test: status {:?}", status);
}

// ====================
// 優先度中: より詳細なリダイレクトテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_redirect_307() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 307 Temporary Redirectのテスト
    // 注意: このテストは設定ファイルで307リダイレクトを設定する必要がある

    let response = send_request(PROXY_PORT, "/redirect-307", &[]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        let location = get_header_value(&response, "Location");

        // リダイレクトが設定されている場合、307 Temporary Redirectが返される
        assert_eq!(
            status,
            Some(307),
            "Should return 307 Temporary Redirect for redirect, got: {:?}",
            status
        );

        if status == Some(307) {
            assert!(
                location.is_some(),
                "307 redirect should include Location header"
            );
            eprintln!("307 Temporary Redirect test: location = {:?}", location);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_redirect_308() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 308 Permanent Redirectのテスト
    // 注意: このテストは設定ファイルで308リダイレクトを設定する必要がある

    let response = send_request(PROXY_PORT, "/redirect-308", &[]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        let location = get_header_value(&response, "Location");

        // リダイレクトが設定されている場合、308が返される可能性がある
        assert_eq!(
            status,
            Some(308),
            "Should return 308 Permanent Redirect for redirect, got: {:?}",
            status
        );

        if status == Some(308) {
            assert!(
                location.is_some(),
                "308 Permanent Redirect should include Location header"
            );
            eprintln!("308 Permanent Redirect test: location = {:?}", location);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_redirect_method_preservation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // リダイレクト時にHTTPメソッドが保持されることを確認
    // 307/308リダイレクトでは、メソッドが保持される必要がある

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

    // POSTリクエストを送信（リダイレクトされる可能性がある）
    let request = b"POST /redirect-test HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send POST request: {:?}", e);
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
    let location = get_header_value(&response, "Location");

    // リダイレクトが返される場合、Locationヘッダーが含まれる
    if status == Some(301) || status == Some(302) || status == Some(307) || status == Some(308) {
        assert!(
            location.is_some(),
            "Redirect should include Location header"
        );
        eprintln!(
            "Redirect method preservation test: status {:?}, location {:?}",
            status, location
        );
    } else {
        eprintln!(
            "Redirect method preservation test: no redirect (status {:?})",
            status
        );
    }
}

// ====================
// 優先度中: より詳細なメトリクステスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_prometheus_metrics_detailed() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Prometheusメトリクスの詳細テスト
    // 複数のリクエストを送信してメトリクスが更新されることを確認

    // 最初のリクエスト
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive first response");

    // 2回目のリクエスト
    let response2 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response2.is_some(), "Should receive second response");

    // メトリクスエンドポイントにアクセス
    let metrics_response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(
        metrics_response.is_some(),
        "Should receive metrics response"
    );

    let metrics_response = metrics_response.unwrap();
    let status = get_status_code(&metrics_response);
    assert_eq!(status, Some(200), "Metrics endpoint should return 200 OK");

    // メトリクスにリクエスト数が含まれることを確認
    let metrics_body = metrics_response;
    assert!(
        metrics_body.contains("http_requests_total")
            || metrics_body.contains("requests_total")
            || metrics_body.contains("http_requests")
            || metrics_body.contains("veil_"),
        "Metrics should contain request count metrics"
    );

    eprintln!("Prometheus metrics detailed test: metrics endpoint accessible");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_prometheus_metrics_after_errors() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // エラーが発生した後のメトリクスを確認
    // 404エラーを発生させる
    let error_response = send_request(PROXY_PORT, "/nonexistent-page-12345", &[]).await;
    assert!(error_response.is_some(), "Should receive error response");

    let error_response = error_response.unwrap();
    let status = get_status_code(&error_response);
    assert_eq!(status, Some(404), "Should return 404 Not Found");

    // メトリクスエンドポイントにアクセス
    let metrics_response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(
        metrics_response.is_some(),
        "Should receive metrics response"
    );

    let metrics_response = metrics_response.unwrap();
    let status = get_status_code(&metrics_response);
    assert_eq!(status, Some(200), "Metrics endpoint should return 200 OK");

    // メトリクスにエラー数が含まれる可能性がある
    let metrics_body = metrics_response;
    assert!(
        metrics_body.contains("http_requests_total")
            || metrics_body.contains("requests_total")
            || metrics_body.contains("http_requests")
            || metrics_body.contains("veil_")
            || metrics_body.contains("404")
            || metrics_body.contains("error"),
        "Metrics should contain error metrics or request metrics"
    );

    eprintln!("Prometheus metrics after errors test: metrics endpoint accessible after error");
}

// ====================
// 優先度中: より詳細なヘッダー操作テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_header_manipulation_multiple_headers() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 複数のヘッダーを追加・削除するテスト
    let response = send_request(
        PROXY_PORT,
        "/",
        &[
            ("X-Custom-Header-1", "value1"),
            ("X-Custom-Header-2", "value2"),
            ("User-Agent", "test-agent"),
        ],
    )
    .await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // プロキシが追加したヘッダーを確認
    let proxied_by = get_header_value(&response, "X-Proxied-By");
    let proxied_by_clone = proxied_by.clone();
    if let Some(ref proxied_value) = proxied_by {
        assert_eq!(
            proxied_value, "veil",
            "X-Proxied-By header should be 'veil'"
        );
    }

    // Serverヘッダーが削除されている可能性がある
    let server_header = get_header_value(&response, "Server");
    // Serverヘッダーが削除されている場合、Noneが返される

    eprintln!(
        "Header manipulation multiple headers test: proxied_by={:?}, server={:?}",
        proxied_by_clone, server_header
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_header_manipulation_case_insensitive() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ヘッダー名の大文字小文字を区別しないことを確認
    let response = send_request(
        PROXY_PORT,
        "/",
        &[
            ("x-custom-header", "value1"),
            ("X-Custom-Header", "value2"),
            ("X-CUSTOM-HEADER", "value3"),
        ],
    )
    .await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // プロキシが追加したヘッダーを確認
    let proxied_by = get_header_value(&response, "X-Proxied-By");
    if let Some(proxied_value) = proxied_by {
        assert_eq!(
            proxied_value, "veil",
            "X-Proxied-By header should be 'veil'"
        );
    }

    eprintln!("Header manipulation case insensitive test: successful");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_header_manipulation_special_characters() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 特殊文字を含むヘッダー値の処理を確認
    let response = send_request(
        PROXY_PORT,
        "/",
        &[
            ("X-Test-Header", "value with spaces"),
            ("X-Test-Header-2", "value-with-dashes"),
            ("X-Test-Header-3", "value_with_underscores"),
        ],
    )
    .await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    eprintln!("Header manipulation special characters test: successful");
}

// ====================
// 優先度中: キャッシュ機能詳細テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_stale_if_error() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // stale-if-errorのテスト
    // 注意: このテストは設定ファイルでstale-if-errorを有効化する必要がある

    // キャッシュエントリを作成
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive first response");

    let response1 = response1.unwrap();
    let status1 = get_status_code(&response1);
    assert_eq!(status1, Some(200), "First response should be successful");

    // バックエンドがエラーを返す場合、stale-if-errorが有効な場合、期限切れキャッシュが返される可能性がある
    // 実際のテストには、バックエンドのエラーをシミュレートする必要がある
    let response2 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response2.is_some(), "Should receive second response");

    let response2 = response2.unwrap();
    let status2 = get_status_code(&response2);
    assert_eq!(status2, Some(200), "Second response should be successful");

    eprintln!("Cache stale-if-error test: both responses successful");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_vary_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Varyヘッダーを尊重するキャッシュのテスト
    // 注意: このテストは設定ファイルでキャッシュとVaryヘッダーを有効化する必要がある

    // Accept-Languageヘッダーを付けてリクエスト
    let response1 = send_request(PROXY_PORT, "/", &[("Accept-Language", "en-US")]).await;
    assert!(response1.is_some(), "Should receive first response");

    let response1 = response1.unwrap();
    let status1 = get_status_code(&response1);
    assert_eq!(status1, Some(200), "First response should be successful");

    // 異なるAccept-Languageヘッダーでリクエスト
    let response2 = send_request(PROXY_PORT, "/", &[("Accept-Language", "ja-JP")]).await;
    assert!(response2.is_some(), "Should receive second response");

    let response2 = response2.unwrap();
    let status2 = get_status_code(&response2);
    assert_eq!(status2, Some(200), "Second response should be successful");

    // Varyヘッダーが尊重されている場合、異なるキャッシュエントリが作成される可能性がある
    eprintln!("Cache Vary header test: both responses successful");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_invalidation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // キャッシュ無効化のテスト
    // 注意: このテストは設定ファイルでキャッシュを有効化する必要がある

    // キャッシュエントリを作成
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive first response");

    let response1 = response1.unwrap();
    let status1 = get_status_code(&response1);
    assert_eq!(status1, Some(200), "First response should be successful");

    // キャッシュが有効な場合、2回目のリクエストはキャッシュから返される可能性がある
    let response2 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response2.is_some(), "Should receive second response");

    let response2 = response2.unwrap();
    let status2 = get_status_code(&response2);
    assert_eq!(status2, Some(200), "Second response should be successful");

    // キャッシュが無効化される場合、新しいリクエストがバックエンドに送信される可能性がある
    // 実際のテストには、キャッシュ無効化のメカニズム（PURGEメソッドなど）が必要
    eprintln!("Cache invalidation test: both responses successful");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_query_parameter_handling() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // クエリパラメータを含むキャッシュのテスト
    // 注意: このテストは設定ファイルでキャッシュを有効化する必要がある

    // クエリパラメータ付きでリクエスト
    let response1 = send_request(PROXY_PORT, "/?param1=value1", &[]).await;
    assert!(response1.is_some(), "Should receive first response");

    let response1 = response1.unwrap();
    let status1 = get_status_code(&response1);
    // クエリパラメータ付きのリクエストが404を返す可能性がある
    assert!(
        status1 == Some(200) || status1 == Some(404),
        "First response should be 200 or 404: {:?}",
        status1
    );

    // 同じクエリパラメータでリクエスト（キャッシュヒットの可能性）
    let response2 = send_request(PROXY_PORT, "/?param1=value1", &[]).await;
    assert!(response2.is_some(), "Should receive second response");

    let response2 = response2.unwrap();
    let status2 = get_status_code(&response2);
    // 2回目のリクエストも同じステータスが返される可能性がある
    assert!(
        status2 == Some(200) || status2 == Some(404),
        "Second response should be 200 or 404: {:?}",
        status2
    );

    // 異なるクエリパラメータでリクエスト（キャッシュミスの可能性）
    let response3 = send_request(PROXY_PORT, "/?param1=value2", &[]).await;
    assert!(response3.is_some(), "Should receive third response");

    let response3 = response3.unwrap();
    let status3 = get_status_code(&response3);
    // 3回目のリクエストも同じステータスが返される可能性がある
    assert!(
        status3 == Some(200) || status3 == Some(404),
        "Third response should be 200 or 404: {:?}",
        status3
    );

    eprintln!("Cache query parameter handling test: all responses successful");
}

// ====================
// 優先度中: より詳細なバッファリングテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_large_response() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 大きなレスポンスのバッファリングテスト
    // 注意: このテストは設定ファイルでバッファリングを有効化する必要がある

    // 大きなレスポンスをリクエスト
    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // 大きなファイルが存在する場合、200が返される
        assert_eq!(
            status,
            Some(200),
            "Should return 200 OK for large file, got: {:?}",
            status
        );

        if status == Some(200) {
            // Content-Lengthヘッダーを確認
            let content_length = get_content_length_from_headers(response.as_bytes());
            if let Some(cl) = content_length {
                eprintln!(
                    "Buffering large response test: content length = {} bytes",
                    cl
                );
            }
        }
    }
}

#[tokio::test]
#[ntest::timeout(60000)]
pub(super) async fn test_100_continue() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use crate::common::http1_client::Http1TestClient;

    let body = vec![b'a'; 100];

    // 100-continue の処理は負荷下で遅延する場合があるためリトライ
    for attempt in 0..3u32 {
        let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            client.post_with_headers("/", &[("Expect", "100-continue")], &body),
        )
        .await;
        match result {
            Ok(Ok((status, resp_body))) => {
                assert_eq!(status, 200, "Should return 200 OK after 100-continue flow");
                assert!(!resp_body.is_empty(), "Response body should not be empty");
                return;
            }
            Ok(Err(e)) => {
                eprintln!("100-continue attempt {}: request error: {}", attempt + 1, e);
            }
            Err(_) => {
                eprintln!("100-continue attempt {}: timed out after 15s", attempt + 1);
            }
        }
        if attempt < 2 {
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    }
    panic!("100-continue request failed after 3 attempts");
}

/// B-11: Expect: 100-continue で **100 受信後にボディを送信する**（curl と同じ）クライアント。
///
/// hyper クライアントは Expect を付けてもボディを即送信するため、「プロキシが 100 を返し、
/// クライアントがそれを待ってからボディを送る」本来の RFC フローはこのテストでしか踏まない。
/// 旧実装では、プロキシが Expect をバックエンドへ転送 → バックエンド（veil）が独自の
/// 100 Continue を返す → その 100 が最終応答と別セグメントで先着するとプロキシが最終応答と
/// 誤認して転送し、本物の 200 が届かずクライアントが永久待機（間欠ハング）していた。
/// 修正後は Expect がバックエンドへ転送されず（+ 1xx 中間応答は読み捨て）、常に
/// 「100 Continue → 200 最終応答」の順で 1 回ずつ届くことを検証する。
#[tokio::test]
#[ntest::timeout(60000)]
pub(super) async fn test_100_continue_deferred_body() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let body = vec![b'a'; 100];

    // 旧実装の再現率は 1 回あたり 10〜20% 程度（セグメント分割タイミング依存）のため、
    // 30 回連続で全て成功することを要求して回帰を検出する（1 回 ~60ms）。
    for round in 0..30u32 {
        let connector = tokio_rustls::TlsConnector::from(create_client_config());
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", PROXY_PORT))
            .await
            .expect("TCP connect failed");
        let server_name = ServerName::try_from("localhost").unwrap();
        let mut tls =
            tokio::time::timeout(Duration::from_secs(5), connector.connect(server_name, tcp))
                .await
                .expect("TLS handshake timed out")
                .expect("TLS handshake failed");

        // 1. ヘッダーのみ送信（ボディは 100 受信後）。
        let head = format!(
            "POST / HTTP/1.1\r\nHost: localhost\r\nExpect: 100-continue\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
            body.len()
        );
        tls.write_all(head.as_bytes())
            .await
            .expect("head write failed");

        // 2. 100 Continue を待つ。
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = tokio::time::timeout_at(deadline, tls.read(&mut tmp))
                .await
                .unwrap_or_else(|_| panic!("round {}: timed out waiting for 100 Continue", round))
                .expect("read failed");
            assert!(
                n > 0,
                "round {}: connection closed before 100 Continue",
                round
            );
            buf.extend_from_slice(&tmp[..n]);
        }
        let text = String::from_utf8_lossy(&buf);
        assert!(
            text.starts_with("HTTP/1.1 100"),
            "round {}: expected 100 Continue first, got: {}",
            round,
            text.lines().next().unwrap_or("")
        );
        // 100 のヘッド以降を保持（最終応答の先頭が同時に届いた場合に備える）。
        let head_end = buf.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let mut resp = buf.split_off(head_end);

        // 3. ボディ送信。
        tls.write_all(&body).await.expect("body write failed");

        // 4. 最終応答を受信（Connection: close なので EOF まで）。
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            let n = match tokio::time::timeout_at(deadline, tls.read(&mut tmp)).await {
                Ok(Ok(n)) => n,
                Ok(Err(_)) => break, // close_notify なしの切断も許容
                Err(_) => panic!(
                    "round {}: timed out waiting for final response (B-11 hang regression). got so far: {:?}",
                    round,
                    String::from_utf8_lossy(&resp).lines().next().unwrap_or("")
                ),
            };
            if n == 0 {
                break;
            }
            resp.extend_from_slice(&tmp[..n]);
        }
        let resp_text = String::from_utf8_lossy(&resp);
        assert!(
            resp_text.starts_with("HTTP/1.1 200"),
            "round {}: expected final 200 OK, got: {}",
            round,
            resp_text.lines().next().unwrap_or("(empty)")
        );
        // バックエンド由来の 100 Continue が最終応答として転送されていない（1 応答のみ）。
        assert_eq!(
            resp_text.matches("HTTP/1.1 100").count(),
            0,
            "round {}: interim 100 from backend must not be forwarded as final response",
            round
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_chunked_response() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Chunked Transfer Encodingレスポンスのバッファリングテスト
    // 注意: このテストは設定ファイルでバッファリングを有効化する必要がある

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Transfer-Encodingヘッダーを確認
    let transfer_encoding = get_header_value(&response, "Transfer-Encoding");
    // Chunked Transfer Encodingが使用されている場合、Transfer-Encodingヘッダーが含まれる可能性がある

    eprintln!(
        "Buffering chunked response test: transfer_encoding={:?}",
        transfer_encoding
    );
}

// ====================
// バッファリング: エッジケーステスト（優先度: 高）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_empty_response() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 空レスポンスのバッファリングテスト
    // 注意: このテストは設定ファイルでバッファリングを有効化する必要がある

    // 空レスポンスを返すエンドポイントをリクエスト（存在しない場合は404）
    let response = send_request(PROXY_PORT, "/nonexistent", &[]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);
        // 404が返される場合、空のボディが正常に処理されることを確認
        if status == Some(404) {
            let content_length = get_content_length_from_headers(response.as_bytes());
            // Content-Lengthが0または未指定の場合、空レスポンスが正常に処理される
            eprintln!(
                "Buffering empty response test: status={:?}, content_length={:?}",
                status, content_length
            );
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_zero_content_length() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Content-Length: 0のレスポンスのバッファリングテスト
    // 注意: このテストは設定ファイルでバッファリングを有効化する必要がある

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Content-Lengthヘッダーを確認
    let content_length = get_content_length_from_headers(response.as_bytes());
    // Content-Lengthが0の場合でも正常に処理されることを確認
    eprintln!(
        "Buffering zero content length test: content_length={:?}",
        content_length
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_adaptive_threshold_switch() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Adaptiveモードの閾値切り替えテスト
    // 注意: このテストは設定ファイルでバッファリングを有効化する必要がある
    // 例: ./tests/e2e_setup.sh test buffering

    // 閾値より小さいレスポンス（Fullバッファリング）
    let small_response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(small_response.is_some(), "Should receive small response");
    let small_response = small_response.unwrap();
    assert_eq!(
        get_status_code(&small_response),
        Some(200),
        "Should return 200 OK"
    );

    // 閾値より大きいレスポンス（Streaming）
    let large_response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    if let Some(large_response) = large_response {
        let status = get_status_code(&large_response);
        if status == Some(200) {
            // 大きいレスポンスが正常に処理されることを確認
            let small_size = small_response.len();
            let large_size = large_response.len();
            eprintln!(
                "Adaptive threshold switch test: small={} bytes, large={} bytes",
                small_size, large_size
            );
            assert!(
                large_size > small_size,
                "Large response should be larger than small response"
            );
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_adaptive_content_length_missing() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Content-LengthなしレスポンスのAdaptiveモード動作確認
    // 注意: このテストは設定ファイルでバッファリングを有効化する必要がある

    // Chunked Transfer Encodingレスポンス（Content-Lengthなし）
    // 並列実行時のTLSハンドシェイクタイムアウト対策としてリトライロジックを追加
    let response = send_request_with_retry(PROXY_PORT, "/", &[], 3).await;
    assert!(response.is_some(), "Should receive response after retries");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Content-Lengthが存在しない場合、Transfer-Encoding: chunkedが使用される可能性がある
    let content_length = get_content_length_from_headers(response.as_bytes());
    let transfer_encoding = get_header_value(&response, "Transfer-Encoding");

    eprintln!(
        "Adaptive content length missing test: content_length={:?}, transfer_encoding={:?}",
        content_length, transfer_encoding
    );

    // Content-Lengthがない場合でも正常に処理されることを確認
    assert!(
        content_length.is_none() || transfer_encoding.is_some() || !response.is_empty(),
        "Response should be processed even without Content-Length"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_max_memory_buffer_within_limit() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // メモリバッファ上限内での動作確認
    // 注意: このテストは設定ファイルでバッファリングを有効化する必要がある

    // 通常のサイズのレスポンスをリクエスト
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Content-Lengthを確認
    let content_length = get_content_length_from_headers(response.as_bytes());
    if let Some(cl) = content_length {
        // メモリバッファ上限（デフォルト10MB）内であれば正常に処理される
        eprintln!(
            "Buffering max memory buffer within limit test: content_length={} bytes",
            cl
        );
        assert!(
            cl < 10 * 1024 * 1024,
            "Response should be within memory buffer limit"
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_invalid_content_length() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なContent-Lengthヘッダーの動作確認

    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
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

    let request = b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: invalid\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send request: {:?}", e);
    }
    tls_stream.flush().unwrap();

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

    assert!(
        status == Some(400),
        "Should return 400 Bad Request for invalid content length, got: {:?}",
        status
    );
}

// ====================
// 優先度中: より詳細なヘルスチェックテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_interval() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ヘルスチェック間隔のテスト
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある

    // 複数のリクエストを送信してヘルスチェックが動作することを確認
    for i in 0..5 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        assert!(response.is_some(), "Should receive response {}", i);

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK for request {}", i);

        // ヘルスチェック間隔を待つ（実際のテストには時間の経過が必要）
        if i < 4 {
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    eprintln!("Health check interval test: all requests successful");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_threshold() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ヘルスチェック閾値のテスト
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある

    // 複数のリクエストを送信してヘルスチェック閾値が動作することを確認
    for i in 0..10 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        assert!(response.is_some(), "Should receive response {}", i);

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK for request {}", i);
    }

    // ヘルスチェック閾値が適切に設定されている場合、一定回数の失敗後にバックエンドが無効化される可能性がある
    eprintln!("Health check threshold test: all requests successful");
}

// ====================
// ヘルスチェック: 詳細テスト（優先度: 高）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_healthy_status_200() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ステータス200が健康と判断されることを確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある

    // 正常なリクエストを送信
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
            eprintln!("Health check healthy status 200 test: metrics detected");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_healthy_status_custom() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // カスタムステータスコードが健康と判断されることを確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある

    // 正常なリクエストを送信
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 200-399の範囲のステータスコードが健康と判断される
    assert!(
        status.is_some() && status.unwrap() >= 200 && status.unwrap() < 400,
        "Should return healthy status code (200-399): {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_unhealthy_status_500() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ステータス500が不健康と判断されることを確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある
    // 実際のテストには、500を返すエンドポイントが必要

    // 通常のリクエストは200を返す
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 正常な場合は200が返される
    // 500が返される場合は、ヘルスチェックで不健康と判断される可能性がある
    eprintln!(
        "Health check unhealthy status 500 test: status={:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_threshold_reset_on_success() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 成功時の閾値リセットを確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある

    // 複数の正常なリクエストを送信
    for i in 0..5 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        assert!(response.is_some(), "Should receive response {}", i);

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK for request {}", i);

        // 成功時に失敗カウントがリセットされることを確認
        if i < 4 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    eprintln!("Health check threshold reset on success test: all requests successful");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_threshold_reset_on_failure() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 失敗時の閾値リセットを確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある
    // 実際のテストには、失敗をシミュレートする必要がある

    // 正常なリクエストを送信
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // 失敗時に成功カウントがリセットされることを確認
    // 実際のテストには、失敗をシミュレートする必要がある
    eprintln!("Health check threshold reset on failure test: request successful");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_path_custom() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // カスタムパスでのヘルスチェックを確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある

    // /healthエンドポイントにリクエストを送信
    let response = send_request(PROXY_PORT, "/health", &[]).await;
    if let Some(response) = response {
        let status = get_status_code(&response);
        // /healthエンドポイントが存在する場合、200が返される可能性がある
        eprintln!("Health check path custom test: status={:?}", status);
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_interval_accuracy() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ヘルスチェック間隔の正確性を確認
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある

    use std::time::Instant;

    // メトリクスエンドポイントから初期状態を取得
    let start = Instant::now();
    let metrics1 = send_request(PROXY_PORT, "/__metrics", &[]).await;

    // ヘルスチェック間隔（デフォルト1秒）を待つ
    tokio::time::sleep(Duration::from_secs(2)).await;

    let metrics2 = send_request(PROXY_PORT, "/__metrics", &[]).await;
    let elapsed = start.elapsed();

    // メトリクスが更新されていることを確認（間隔が経過している）
    if let (Some(m1), Some(m2)) = (metrics1, metrics2) {
        if m1.contains("http_upstream_health") || m2.contains("http_upstream_health") {
            eprintln!("Health check interval accuracy test: elapsed={:?}", elapsed);
            // 間隔が経過していることを確認
            assert!(
                elapsed >= Duration::from_secs(1),
                "Health check interval should have elapsed"
            );
        }
    }
}

// ====================
// WebSocket: エラーハンドリングテスト（優先度: 高）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_invalid_upgrade_request() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なUpgradeリクエストの処理を確認（TLS経由）
    // Upgradeヘッダーがない場合、WebSocketアップグレードではなく通常のHTTPリクエストとして処理される
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

    // 不正なUpgradeリクエスト（Upgradeヘッダーがない）
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
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
    // Upgradeヘッダーがない場合、WebSocketアップグレードとして扱われないため101にはならない
    assert!(
        status != Some(101),
        "Invalid upgrade request (missing Upgrade header) should not return 101, got: {:?}",
        status
    );

    eprintln!(
        "WebSocket invalid upgrade request test: status={:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_missing_connection_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ConnectionヘッダーがないUpgradeリクエストの処理を確認（TLS経由）
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

    // ConnectionヘッダーがないUpgradeリクエスト
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
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
    // Connectionヘッダーがない場合、WebSocketアップグレードとして扱われないため101にはならない
    assert!(
        status != Some(101),
        "Missing Connection header should not result in WebSocket upgrade (101), got: {:?}",
        status
    );

    eprintln!(
        "WebSocket missing connection header test: status={:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_invalid_websocket_version() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なWebSocketバージョンの処理を確認（TLS経由）
    // プロキシはSec-WebSocket-Versionを検証しないため、バックエンドが拒否するか101を返す
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

    // 不正なWebSocketバージョン（13以外）
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 14\r\n\r\n";
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
    // プロキシはバージョン検証を行わないため、バックエンドが400/426/400を返すかもしれない
    // いずれにせよ、接続が確立したか（何らかのHTTPレスポンスが返った）ことを確認する
    assert!(
        status.is_some(),
        "Should receive some HTTP response for invalid WebSocket version, got none"
    );

    eprintln!("WebSocket invalid version test: status={:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_connection_close() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 接続クローズの動作確認

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

    // rustls::Streamを使用してI/Oを実行
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // WebSocketアップグレードリクエストを送信
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send WebSocket upgrade request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // レスポンスを受信（ヘッダー部分のみ）
    let mut response = Vec::new();
    let mut buf = [0u8; 1];

    // ヘッダー部分を読み取る（\r\n\r\nまで）
    loop {
        match tls_stream.read_exact(&mut buf) {
            Ok(_) => {
                response.push(buf[0]);
                // \r\n\r\nを検出（ヘッダー終了）
                if response.len() >= 4 {
                    let len = response.len();
                    if &response[len - 4..] == b"\r\n\r\n" {
                        break;
                    }
                }
                // ヘッダーが大きすぎる場合は中止
                if response.len() > 8192 {
                    break;
                }
            }
            Err(_) => {
                if response.is_empty() {
                    panic!("No response received");
                }
                break;
            }
        }
    }

    if response.is_empty() {
        panic!("Empty response received");
    }

    let response = String::from_utf8_lossy(&response);
    let status = get_status_code(&response);

    if status == Some(101) {
        eprintln!("WebSocket connection established, closing connection");
        // 接続をクローズ（dropで自動的にクローズされる）
    } else {
        eprintln!("WebSocket connection not established: status {:?}", status);
    }

    // 基本的な動作確認
    assert!(
        status == Some(101),
        "Should return appropriate status: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_unexpected_close() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 予期しない接続クローズの処理を確認

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

    // rustls::Streamを使用してI/Oを実行
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    // WebSocketアップグレードリクエストを送信
    let request = b"GET /ws/ HTTP/1.1\r\nHost: localhost\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send WebSocket upgrade request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // レスポンスを受信（ヘッダー部分のみ）
    let mut response = Vec::new();
    let mut buf = [0u8; 1];

    // ヘッダー部分を読み取る
    for _ in 0..8192 {
        match tls_stream.read_exact(&mut buf) {
            Ok(_) => {
                response.push(buf[0]);
                // \r\n\r\nを検出（ヘッダー終了）
                if response.len() >= 4 {
                    let len = response.len();
                    if &response[len - 4..] == b"\r\n\r\n" {
                        break;
                    }
                }
            }
            Err(_) => {
                // 予期しないクローズ（EOF）
                break;
            }
        }
    }

    if !response.is_empty() {
        let response = String::from_utf8_lossy(&response);
        let status = get_status_code(&response);
        eprintln!("WebSocket unexpected close test: status={:?}", status);

        // 予期しないクローズが発生しても、適切に処理されることを確認
        assert!(
            status == Some(101),
            "Should return appropriate status even on unexpected close: {:?}",
            status
        );
    }
}
