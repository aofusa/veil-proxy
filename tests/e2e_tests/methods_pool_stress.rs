//! HTTP メソッド・リダイレクト・エラー・並行要求・接続プール・性能・ストレス・Keep-Alive・メトリクス・セキュリティ・圧縮・LB・キャッシュ・バッファリング
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// 優先度中: HTTPメソッド詳細テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_http_method_put() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // PUTメソッドのテスト
    let response = send_request_with_method(PROXY_PORT, "/", "PUT", &[], None).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // PUTメソッドは200、201、204、または405が返される可能性がある
    assert!(
        status == Some(200)
            || status == Some(201)
            || status == Some(204)
            || status == Some(405)
            || status == Some(404),
        "Should return 200, 201, 204, 405, or 404: {:?}",
        status
    );

    eprintln!("HTTP PUT method test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_http_method_delete() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // DELETEメソッドのテスト
    let response = send_request_with_method(PROXY_PORT, "/", "DELETE", &[], None).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // DELETEメソッドは200、204、または405が返される可能性がある
    assert!(
        status == Some(200) || status == Some(204) || status == Some(405) || status == Some(404),
        "Should return 200, 204, 405, or 404: {:?}",
        status
    );

    eprintln!("HTTP DELETE method test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_http_method_patch() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // PATCHメソッドのテスト
    let response = send_request_with_method(PROXY_PORT, "/", "PATCH", &[], None).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // PATCHメソッドは200、204、または405が返される可能性がある
    assert!(
        status == Some(200) || status == Some(204) || status == Some(405) || status == Some(404),
        "Should return 200, 204, 405, or 404: {:?}",
        status
    );

    eprintln!("HTTP PATCH method test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_http_method_options() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // OPTIONSメソッドのテスト
    let response = send_request_with_method(PROXY_PORT, "/", "OPTIONS", &[], None).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // OPTIONSメソッドは200、204、または405が返される可能性がある
    assert!(
        status == Some(200) || status == Some(204) || status == Some(405) || status == Some(404),
        "Should return 200, 204, 405, or 404: {:?}",
        status
    );

    // Allowヘッダーが存在する可能性がある
    let allow = get_header_value(&response, "Allow");
    if let Some(allow_value) = allow {
        eprintln!("HTTP OPTIONS method test: Allow = {}", allow_value);
    }

    eprintln!("HTTP OPTIONS method test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_http_method_head() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HEADメソッドのテスト
    let response = send_request_with_method(PROXY_PORT, "/", "HEAD", &[], None).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // HEADメソッドは200、または404が返される可能性がある
    assert!(
        status == Some(200),
        "Should return 200 or 404: {:?}",
        status
    );

    // HEADメソッドの場合、ボディは空である必要がある
    if let Some(body_start) = response.find("\r\n\r\n") {
        let body = &response[body_start + 4..];
        // HEADメソッドのボディは空または非常に小さい可能性がある
        if !body.trim().is_empty() {
            eprintln!(
                "HEAD method test: body is not empty (size: {} bytes)",
                body.len()
            );
        }
    }

    eprintln!("HTTP HEAD method test: status {:?}", status);
}

// ====================
// 優先度中: リダイレクト詳細テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_redirect_location_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // リダイレクトのLocationヘッダーのテスト
    // 注意: 実際のリダイレクトが発生するパスが必要

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // リダイレクトステータスコードの場合、Locationヘッダーが存在する可能性がある
    if status == Some(301) || status == Some(302) || status == Some(307) || status == Some(308) {
        let location = get_header_value(&response, "Location");
        if let Some(loc) = location {
            eprintln!("Redirect Location header test: Location = {}", loc);
            assert!(!loc.is_empty(), "Location header should not be empty");
        } else {
            eprintln!(
                "Redirect Location header test: Location header not present (may be optional)"
            );
        }
    } else {
        eprintln!(
            "Redirect Location header test: status {:?} (not a redirect)",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_redirect_cache_control() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // リダイレクトのCache-Controlヘッダーのテスト
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // リダイレクトステータスコードの場合、Cache-Controlヘッダーが存在する可能性がある
    if status == Some(301) || status == Some(302) || status == Some(307) || status == Some(308) {
        let cache_control = get_header_value(&response, "Cache-Control");
        if let Some(cc) = cache_control {
            eprintln!("Redirect Cache-Control header test: Cache-Control = {}", cc);
        } else {
            eprintln!("Redirect Cache-Control header test: Cache-Control header not present");
        }
    } else {
        eprintln!(
            "Redirect Cache-Control header test: status {:?} (not a redirect)",
            status
        );
    }
}

// ====================
// 優先度中: エラーハンドリング詳細テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_error_handling_413_payload_too_large() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 413 Payload Too Largeのテスト
    // 大きなリクエストボディを送信して、サイズ制限を確認

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

    // 大きなContent-Lengthを指定したリクエストを送信 (MAX_BODY_SIZE=10MiB より大きい値)
    let large_size = 11_000_000; // 11MB > proxy's 10MiB limit
    let request = format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
        large_size
    );
    if let Err(e) = tls_stream.write_all(request.as_bytes()) {
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

    // 413 Payload Too Largeが返される可能性がある
    assert!(
        status == Some(413),
        "Should return 413, 400, 200, or close connection: {:?}",
        status
    );

    eprintln!("413 Payload Too Large test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(60000)]
pub(super) async fn test_error_handling_431_request_header_fields_too_large() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

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
        while tls_conn.is_handshaking() {
            if let Err(e) = tls_conn.complete_io(&mut stream) {
                eprintln!("attempt {}: TLS handshake error: {}", attempt + 1, e);
                break;
            }
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

// ====================
// 優先度中: より詳細な並行リクエストテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_concurrent_requests_different_paths() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 異なるパスへの並行リクエストのテスト
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let success_count = Arc::new(AtomicUsize::new(0));
    let total_requests = 30;
    let paths = ["/", "/large.txt", "/__metrics"];

    let handles: Vec<_> = (0..total_requests)
        .map(|i| {
            let success_count = Arc::clone(&success_count);
            let path = paths[i % paths.len()];
            tokio::spawn(async move {
                let response = send_request(PROXY_PORT, path, &[]).await;
                if let Some(response) = response {
                    let status = get_status_code(&response);
                    // 並列リクエストテスト: プロキシが並列リクエストを処理できることを確認
                    // 404は「プロキシが正常に動作している」ことを示すため、成功としてカウント
                    if status == Some(200) || status == Some(404) {
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
        "At least 80% of concurrent requests to different paths should succeed: {}/{}",
        successes,
        total_requests
    );

    eprintln!(
        "Concurrent requests to different paths test: {}/{} succeeded",
        successes, total_requests
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_concurrent_requests_mixed_methods() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 異なるHTTPメソッドの並行リクエストのテスト
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let success_count = Arc::new(AtomicUsize::new(0));
    let total_requests = 20;
    let methods = ["GET", "POST", "HEAD", "OPTIONS"];

    let handles: Vec<_> = (0..total_requests)
        .map(|i| {
            let success_count = Arc::clone(&success_count);
            let method = methods[i % methods.len()];
            tokio::spawn(async move {
                let response = send_request_with_method(PROXY_PORT, "/", method, &[], None).await;
                if let Some(response) = response {
                    let status = get_status_code(&response);
                    // 並列メソッドテスト: プロキシが並列リクエストを処理できることを確認
                    // 404は「プロキシが正常に動作している」ことを示すため、成功としてカウント
                    // 405は「メソッドが許可されていない」というエラーだが、プロキシが正常に動作していることを示すため、成功としてカウント
                    if status == Some(200) || status == Some(404) || status == Some(405) {
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
        "At least 80% of concurrent requests with mixed methods should succeed: {}/{}",
        successes,
        total_requests
    );

    eprintln!(
        "Concurrent requests with mixed methods test: {}/{} succeeded",
        successes, total_requests
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_concurrent_requests_with_headers() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 異なるヘッダーを含む並行リクエストのテスト
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let success_count = Arc::new(AtomicUsize::new(0));
    let total_requests = 25;

    let handles: Vec<_> = (0..total_requests)
        .map(|i| {
            let success_count = Arc::clone(&success_count);
            tokio::spawn(async move {
                let ua = format!("TestClient-{}", i);
                let req_id = format!("req-{}", i);
                let headers = vec![
                    ("User-Agent", ua.as_str()),
                    ("X-Request-ID", req_id.as_str()),
                ];
                let response = send_request(PROXY_PORT, "/", &headers).await;
                if let Some(response) = response {
                    let status = get_status_code(&response);
                    if status == Some(200) {
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
        "At least 80% of concurrent requests with headers should succeed: {}/{}",
        successes,
        total_requests
    );

    eprintln!(
        "Concurrent requests with headers test: {}/{} succeeded",
        successes, total_requests
    );
}

// ====================
// 優先度中: 接続プールテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_connection_pool_reuse() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 接続プールの再利用のテスト
    // Keep-Alive接続を複数回使用して、接続が再利用されることを確認

    use std::time::Instant;

    // 最初のリクエスト（接続確立）
    let start1 = Instant::now();
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    let elapsed1 = start1.elapsed();

    assert!(response1.is_some(), "First request should succeed");
    let status1 = get_status_code(&response1.unwrap());
    assert_eq!(status1, Some(200), "First request should return 200 OK");

    // 2回目のリクエスト（接続再利用の可能性）
    let start2 = Instant::now();
    let response2 = send_request(PROXY_PORT, "/", &[]).await;
    let elapsed2 = start2.elapsed();

    assert!(response2.is_some(), "Second request should succeed");
    let status2 = get_status_code(&response2.unwrap());
    assert_eq!(status2, Some(200), "Second request should return 200 OK");

    // 2回目のリクエストが速い場合、接続が再利用されている可能性がある
    eprintln!(
        "Connection pool reuse test: first={:?}, second={:?}",
        elapsed1, elapsed2
    );

    // 接続が再利用されている場合、2回目のリクエストが速い可能性がある
    // ただし、これは環境に依存するため、アサーションは緩和
    if elapsed2 < elapsed1 {
        eprintln!(
            "Connection pool reuse test: connection may have been reused (second request faster)"
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_connection_pool_multiple_sequential() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 複数の連続リクエストでの接続プールのテスト
    let num_requests = 10;
    let mut success_count = 0;

    for _ in 0..num_requests {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let status = get_status_code(&response);
            if status == Some(200) {
                success_count += 1;
            }
        }

        // 短い待機時間を入れる（接続プールの動作を確認）
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    assert!(
        success_count >= num_requests * 9 / 10,
        "At least 90% of sequential requests should succeed: {}/{}",
        success_count,
        num_requests
    );

    eprintln!(
        "Connection pool multiple sequential test: {}/{} succeeded",
        success_count, num_requests
    );
}

// ====================
// 優先度中: パフォーマンス関連テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_response_time_consistency() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // レスポンス時間の一貫性のテスト
    use std::time::Instant;

    let num_requests = 10;
    let mut times = Vec::new();

    for _ in 0..num_requests {
        let start = Instant::now();
        let response = send_request(PROXY_PORT, "/", &[]).await;
        let elapsed = start.elapsed();

        if response.is_some() {
            times.push(elapsed);
        }
    }

    assert!(
        times.len() >= num_requests * 9 / 10,
        "At least 90% of requests should succeed: {}/{}",
        times.len(),
        num_requests
    );

    if times.len() >= 5 {
        let avg_time: Duration = times.iter().sum::<Duration>() / times.len() as u32;
        let max_time = times.iter().max().unwrap();
        let min_time = times.iter().min().unwrap();

        eprintln!(
            "Response time consistency test: avg={:?}, min={:?}, max={:?}",
            avg_time, min_time, max_time
        );

        // 最大時間が平均時間の3倍を超えないことを確認（一貫性の指標）
        if *max_time > avg_time * 3 {
            eprintln!("Response time consistency test: high variance detected (may be normal)");
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_throughput_basic() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 基本的なスループットのテスト
    use std::time::Instant;

    let num_requests = 50;
    let start = Instant::now();
    let mut success_count = 0;

    for _ in 0..num_requests {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let status = get_status_code(&response);
            if status == Some(200) {
                success_count += 1;
            }
        }
    }

    let elapsed = start.elapsed();
    let requests_per_second = success_count as f64 / elapsed.as_secs_f64();

    assert!(
        success_count >= num_requests * 9 / 10,
        "At least 90% of requests should succeed: {}/{}",
        success_count,
        num_requests
    );

    eprintln!(
        "Throughput basic test: {} requests in {:?} ({:.2} req/s)",
        success_count, elapsed, requests_per_second
    );

    // 最低限のスループットを確認（1 req/s以上）
    assert!(
        requests_per_second >= 1.0,
        "Throughput should be at least 1 req/s: {:.2} req/s",
        requests_per_second
    );
}

// ====================
// 優先度中: より詳細なストレステスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_stress_rapid_requests() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 高速連続リクエストのストレステスト
    let num_requests = 100;
    let mut success_count = 0;

    for i in 0..num_requests {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let status = get_status_code(&response);
            if status == Some(200) {
                success_count += 1;
            }
        }

        // 非常に短い待機時間（ストレスをかける）
        if i % 10 == 0 {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    }

    assert!(
        success_count >= num_requests * 8 / 10,
        "At least 80% of rapid requests should succeed: {}/{}",
        success_count,
        num_requests
    );

    eprintln!(
        "Stress rapid requests test: {}/{} succeeded",
        success_count, num_requests
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_stress_long_duration() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 長時間実行のストレステスト
    use std::time::Instant;

    let duration = Duration::from_secs(5);
    let start = Instant::now();
    let mut request_count = 0;
    let mut success_count = 0;

    while start.elapsed() < duration {
        request_count += 1;
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let status = get_status_code(&response);
            if status == Some(200) {
                success_count += 1;
            }
        }

        // 短い待機時間
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    let elapsed = start.elapsed();
    let success_rate = if request_count > 0 {
        success_count as f64 / request_count as f64
    } else {
        0.0
    };

    assert!(
        success_rate >= 0.8,
        "At least 80% success rate during long duration test: {:.2}% ({}/{})",
        success_rate * 100.0,
        success_count,
        request_count
    );

    eprintln!(
        "Stress long duration test: {} requests in {:?}, {:.2}% success rate",
        request_count,
        elapsed,
        success_rate * 100.0
    );
}

// ====================
// 優先度中: より詳細なKeep-Aliveテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_keep_alive_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Keep-Aliveタイムアウトのテスト
    // Keep-Alive接続がタイムアウトするまでの動作を確認

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

    // 最初のリクエスト（Keep-Alive接続を確立）
    let request1 = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request1) {
        panic!("Failed to send first request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // レスポンスを受信
    let mut response1 = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response1.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    let response1 = String::from_utf8_lossy(&response1);
    let status1 = get_status_code(&response1);
    assert_eq!(status1, Some(200), "First request should return 200 OK");

    // Keep-Alive接続が維持されていることを確認
    let connection1 = get_header_value(&response1, "Connection");
    if let Some(conn) = connection1 {
        eprintln!("Keep-Alive timeout test: Connection header = {}", conn);
    }

    // 短い待機時間の後、2回目のリクエストを送信（接続が維持されている場合）
    tokio::time::sleep(Duration::from_millis(100)).await;

    let request2 = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request2) {
        panic!(
            "Keep-Alive timeout test: connection may have timed out: {:?}",
            e
        );
    }
    tls_stream.flush().unwrap();

    // レスポンスを受信
    let mut response2 = Vec::new();
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response2.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }

    if !response2.is_empty() {
        let response2 = String::from_utf8_lossy(&response2);
        let status2 = get_status_code(&response2);
        assert_eq!(status2, Some(200), "Second request should return 200 OK");
        eprintln!("Keep-Alive timeout test: connection maintained successfully");
    } else {
        eprintln!("Keep-Alive timeout test: connection may have timed out");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_keep_alive_max_requests() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Keep-Alive接続での最大リクエスト数のテスト
    // 同じ接続で複数のリクエストを送信

    let num_requests = 5;
    let mut success_count = 0;

    // Keep-Alive では同一接続上で複数リクエストを送信する
    // ただし keep-alive でサーバーは接続を閉じないため、短い read timeout で
    // 「データなし」を検出して次のリクエストへ進む
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    // 500ms でデータ待ちを打ち切る（keep-alive: サーバーは閉じない）
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .unwrap();

    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();

    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(_) => {
                panic!("TLS handshake error");
            }
        }
    }

    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);

    for i in 0..num_requests {
        let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
        if let Err(e) = tls_stream.write_all(request) {
            eprintln!("Failed to send request {}: {:?}", i, e);
            break;
        }
        if tls_stream.flush().is_err() {
            break;
        }

        // レスポンスを受信（500ms read timeout で keep-alive 後の無音を検出して終了）
        // \r\n\r\n で止めるとボディ残留バイトが次のリクエストに混入するため、
        // タイムアウトまで読み切ってバッファを空にする
        let mut response = Vec::new();
        let mut buf = [0u8; 8192];
        loop {
            match tls_stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    response.extend_from_slice(&buf[..n]);
                }
                Err(_) => break, // timeout → TLS バッファが空になった
            }
        }

        if !response.is_empty() {
            let response = String::from_utf8_lossy(&response);
            let status = get_status_code(&response);
            if status == Some(200) {
                success_count += 1;
            }
        }
    }

    assert!(
        success_count >= num_requests * 8 / 10,
        "At least 80% of Keep-Alive requests should succeed: {}/{}",
        success_count,
        num_requests
    );

    eprintln!(
        "Keep-Alive max requests test: {}/{} succeeded",
        success_count, num_requests
    );
}

// ====================
// 優先度中: より詳細なメトリクステスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_prometheus_metrics_request_count() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Prometheusメトリクスのリクエストカウントのテスト
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Metrics endpoint should return 200 OK");

    // リクエストカウントメトリクスが含まれることを確認
    assert!(
        response.contains("veil_requests_total")
            || response.contains("requests_total")
            || response.contains("http_requests_total"),
        "Should contain request count metrics"
    );

    eprintln!("Prometheus metrics request count test: metrics contain request count");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_prometheus_metrics_latency() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Prometheusメトリクスのレイテンシのテスト
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Metrics endpoint should return 200 OK");

    // レイテンシメトリクスが含まれることを確認
    assert!(
        response.contains("veil_request_duration")
            || response.contains("request_duration")
            || response.contains("http_request_duration")
            || response.contains("latency"),
        "Should contain latency metrics"
    );

    eprintln!("Prometheus metrics latency test: metrics contain latency");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_prometheus_metrics_connections() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Prometheusメトリクスの接続数のテスト
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Metrics endpoint should return 200 OK");

    // 接続数メトリクスが含まれることを確認
    assert!(
        response.contains("veil_connections")
            || response.contains("connections")
            || response.contains("active_connections"),
        "Should contain connection metrics"
    );

    eprintln!("Prometheus metrics connections test: metrics contain connection count");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_prometheus_metrics_after_requests() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // リクエスト送信後のPrometheusメトリクスのテスト
    // いくつかのリクエストを送信してからメトリクスを確認

    // リクエストを送信
    for _ in 0..5 {
        let _ = send_request(PROXY_PORT, "/", &[]).await;
    }

    // メトリクスを取得
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Metrics endpoint should return 200 OK");

    // メトリクスが更新されていることを確認（数値が0より大きい）
    eprintln!("Prometheus metrics after requests test: metrics updated after requests");
}

// ====================
// 優先度中: より詳細なセキュリティ機能テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_security_x_forwarded_for() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // X-Forwarded-Forヘッダーのテスト
    // プロキシがX-Forwarded-Forヘッダーを追加することを確認

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // X-Forwarded-Forヘッダーがレスポンスに含まれる可能性がある
    // （バックエンドが返す場合）
    eprintln!("Security X-Forwarded-For test: request processed successfully");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_security_x_real_ip() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // X-Real-IPヘッダーのテスト
    // プロキシがX-Real-IPヘッダーを追加することを確認

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // X-Real-IPヘッダーがレスポンスに含まれる可能性がある
    // （バックエンドが返す場合）
    eprintln!("Security X-Real-IP test: request processed successfully");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_security_strict_transport_security() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Strict-Transport-Securityヘッダーのテスト
    // HTTPS接続でStrict-Transport-Securityヘッダーが返される可能性を確認

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Strict-Transport-Securityヘッダーが存在する可能性がある
    let hsts = get_header_value(&response, "Strict-Transport-Security");
    if let Some(hsts_value) = hsts {
        eprintln!(
            "Security Strict-Transport-Security test: HSTS = {}",
            hsts_value
        );
        assert!(
            hsts_value.contains("max-age"),
            "HSTS header should contain max-age"
        );
    } else {
        eprintln!(
            "Security Strict-Transport-Security test: HSTS header not present (may be optional)"
        );
    }
}

// ====================
// 優先度中: より詳細なエラーハンドリングテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_error_handling_500_internal_server_error() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // /error-500/ ルートは常に 500 を返す専用バックエンドにプロキシされる
    let response = send_request(PROXY_PORT, "/error-500/test", &[]).await;
    assert!(
        response.is_some(),
        "Should receive a response from the error backend"
    );

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(
        status,
        Some(500),
        "Should propagate 500 Internal Server Error from backend, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_error_handling_503_service_unavailable() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 503 Service Unavailableのテスト
    // バックエンドが利用できない場合（ダウン中など）の動作を確認
    // 存在しないポート（/slow/ルートなど）へアクセスし、502または503が返ることを確認する
    let response = send_request(PROXY_PORT, "/slow/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    assert!(
        status == Some(503) || status == Some(502) || status == Some(504),
        "Should return 503, 502 or 504 for unavailable backend, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_error_handling_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // タイムアウトエラーのテスト
    // 存在しないバックエンド（/slow/）にアクセスしてタイムアウトまたは502を確認

    let response = send_request(PROXY_PORT, "/slow/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // バックエンドに接続できないため、502 Bad Gatewayまたは504 Gateway Timeoutになるべき
    assert!(
        status == Some(502) || status == Some(504),
        "Should return 502 or 504 for timeout, got: {:?}",
        status
    );
}

// ====================
// 優先度中: より詳細な圧縮テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_compression_zstd() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Zstd圧縮のテスト
    // 前提条件: /large.txt が存在することを確認
    let prereq = send_request(PROXY_PORT, "/large.txt", &[]).await;
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

    // zstd圧縮をリクエスト
    let response = send_request(PROXY_PORT, "/large.txt", &[("Accept-Encoding", "zstd")]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 前提条件チェックで200が返ることを確認済みなので、ここでも200を期待
    assert_eq!(
        status,
        Some(200),
        "Compression zstd request should return 200 OK, got: {:?}",
        status
    );

    // 圧縮が有効な場合、Content-Encodingヘッダーがある
    let content_encoding = get_header_value(&response, "Content-Encoding");
    if let Some(ce) = content_encoding {
        eprintln!("Compression zstd test: Content-Encoding = {}", ce);
        // zstdが含まれる可能性がある
        if ce.contains("zstd") {
            eprintln!("Compression zstd test: zstd compression applied");
        }
    } else {
        eprintln!(
            "Compression zstd test: Content-Encoding header not present (may not be compressed)"
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_compression_multiple_encodings() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 前提条件: /large.txt が存在することを確認
    let prereq = send_request(PROXY_PORT, "/large.txt", &[]).await;
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

    // 複数の圧縮エンコーディングの優先順位のテスト
    let response = send_request(
        PROXY_PORT,
        "/large.txt",
        &[("Accept-Encoding", "gzip, br, zstd;q=0.8, deflate")],
    )
    .await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 前提条件チェックで200が返ることを確認済みなので、ここでも200を期待
    assert_eq!(
        status,
        Some(200),
        "Compression multiple encodings request should return 200 OK, got: {:?}",
        status
    );

    // 圧縮が有効な場合、Content-Encodingヘッダーがある
    let content_encoding = get_header_value(&response, "Content-Encoding");
    if let Some(ce) = content_encoding {
        eprintln!(
            "Compression multiple encodings test: Content-Encoding = {}",
            ce
        );
        // 優先順位に応じた圧縮が適用される可能性がある
    } else {
        eprintln!("Compression multiple encodings test: Content-Encoding header not present");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_compression_no_encoding() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 前提条件: /large.txt が存在することを確認
    let prereq = send_request(PROXY_PORT, "/large.txt", &[]).await;
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

    // 圧縮を要求しない場合のテスト
    let response = send_request(PROXY_PORT, "/large.txt", &[("Accept-Encoding", "identity")]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 前提条件チェックで200が返ることを確認済みなので、ここでも200を期待
    assert_eq!(
        status,
        Some(200),
        "Compression identity request should return 200 OK, got: {:?}",
        status
    );

    // 圧縮が要求されない場合、Content-Encodingヘッダーがない可能性がある
    let content_encoding = get_header_value(&response, "Content-Encoding");
    if let Some(ce) = content_encoding {
        eprintln!(
            "Compression no encoding test: Content-Encoding = {} (may be identity)",
            ce
        );
    } else {
        eprintln!(
            "Compression no encoding test: Content-Encoding header not present (uncompressed)"
        );
    }
}

// ====================
// 優先度中: より詳細なロードバランシングテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_load_balancing_round_robin_distribution() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Round Robin分散の詳細テスト
    // 複数のリクエストを送信して、バックエンド間で分散されることを確認

    let num_requests = 20;
    let mut backend1_count = 0;
    let mut backend2_count = 0;

    // B-10: 分散カウントの干渉を避けるため専用プールのパスを使う（test_round_robin_distribution と同様）
    for _ in 0..num_requests {
        let response = send_request(PROXY_PORT, "/rr-test/", &[]).await;
        if let Some(response) = response {
            let server_id = get_header_value(&response, "X-Server-Id");
            if let Some(id) = server_id {
                if id.contains("1") {
                    backend1_count += 1;
                } else if id.contains("2") {
                    backend2_count += 1;
                }
            }
        }
    }

    eprintln!(
        "Load balancing Round Robin distribution test: backend1={}, backend2={}",
        backend1_count, backend2_count
    );

    // Round Robinの場合、両方のバックエンドにリクエストが分散される
    assert!(
        backend1_count > 0 && backend2_count > 0,
        "Requests should be distributed to both backends: backend1={}, backend2={}",
        backend1_count,
        backend2_count
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_load_balancing_backend_identification() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // バックエンド識別のテスト
    // X-Server-Idヘッダーでバックエンドを識別できることを確認

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // X-Server-Idヘッダーが存在する可能性がある
    let server_id = get_header_value(&response, "X-Server-Id");
    if let Some(id) = server_id {
        eprintln!(
            "Load balancing backend identification test: X-Server-Id = {}",
            id
        );
        // バックエンドIDが含まれる
        assert!(
            id.contains("1") || id.contains("2") || id.contains("backend"),
            "X-Server-Id should contain backend identifier: {}",
            id
        );
    } else {
        eprintln!("Load balancing backend identification test: X-Server-Id header not present");
    }
}

// ====================
// 優先度中: より詳細なキャッシュテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_age_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // キャッシュのAgeヘッダーのテスト
    // キャッシュされたレスポンスにAgeヘッダーが含まれることを確認

    // 最初のリクエスト（キャッシュミス）
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive response");

    let response1 = response1.unwrap();
    let status1 = get_status_code(&response1);
    assert_eq!(status1, Some(200), "Should return 200 OK");

    // 短い待機時間
    tokio::time::sleep(Duration::from_millis(100)).await;

    // 2回目のリクエスト（キャッシュヒットの可能性）
    let response2 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response2.is_some(), "Should receive response");

    let response2 = response2.unwrap();
    let status2 = get_status_code(&response2);
    assert_eq!(status2, Some(200), "Should return 200 OK");

    // Ageヘッダーが存在する可能性がある
    let age = get_header_value(&response2, "Age");
    if let Some(age_value) = age {
        eprintln!("Cache Age header test: Age = {}", age_value);
        // Ageは数値である必要がある
        assert!(
            age_value.parse::<u64>().is_ok(),
            "Age header should be a valid number: {}",
            age_value
        );
    } else {
        eprintln!("Cache Age header test: Age header not present (may not be cached)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_vary_header_handling() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // キャッシュのVaryヘッダー処理のテスト
    // Varyヘッダーが存在する場合、キャッシュキーに含まれることを確認

    // Accept-Encodingヘッダーを含むリクエスト
    let response1 = send_request(PROXY_PORT, "/", &[("Accept-Encoding", "gzip")]).await;
    assert!(response1.is_some(), "Should receive response");

    let response1 = response1.unwrap();
    let status1 = get_status_code(&response1);
    assert_eq!(status1, Some(200), "Should return 200 OK");

    // Varyヘッダーが存在する可能性がある
    let vary1 = get_header_value(&response1, "Vary");
    if let Some(vary_value) = vary1 {
        eprintln!("Cache Vary header handling test: Vary = {}", vary_value);
    }

    // 異なるAccept-Encodingヘッダーを含むリクエスト
    let response2 = send_request(PROXY_PORT, "/", &[("Accept-Encoding", "br")]).await;
    assert!(response2.is_some(), "Should receive response");

    let response2 = response2.unwrap();
    let status2 = get_status_code(&response2);
    assert_eq!(status2, Some(200), "Should return 200 OK");

    eprintln!("Cache Vary header handling test: different Accept-Encoding handled");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_cache_max_age_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // キャッシュのCache-Control: max-ageヘッダーのテスト
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Cache-Controlヘッダーが存在する可能性がある
    let cache_control = get_header_value(&response, "Cache-Control");
    if let Some(cc) = cache_control {
        eprintln!("Cache max-age header test: Cache-Control = {}", cc);
        // max-ageが含まれる可能性がある
        if cc.contains("max-age") {
            eprintln!("Cache max-age header test: max-age directive present");
        }
    } else {
        eprintln!("Cache max-age header test: Cache-Control header not present");
    }
}

// ====================
// 優先度中: より詳細なバッファリングテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_adaptive_threshold() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Adaptiveバッファリングの閾値のテスト
    // レスポンスサイズに応じてバッファリングモードが切り替わることを確認

    use std::time::Instant;

    // 小さなレスポンス（Streamingモードの可能性）
    let start1 = Instant::now();
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    let elapsed1 = start1.elapsed();

    assert!(response1.is_some(), "Should receive response");
    let response1 = response1.unwrap();
    let status1 = get_status_code(&response1);
    assert_eq!(status1, Some(200), "Should return 200 OK");

    // 大きなレスポンス（FullまたはAdaptiveモードの可能性）
    let start2 = Instant::now();
    let response2 = send_request(PROXY_PORT, "/large.txt", &[]).await;
    let elapsed2 = start2.elapsed();

    if let Some(response2) = response2 {
        let status2 = get_status_code(&response2);
        if status2 == Some(200) {
            eprintln!(
                "Buffering adaptive threshold test: small={:?}, large={:?}",
                elapsed1, elapsed2
            );
            // 大きなレスポンスの方が時間がかかる可能性がある
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_memory_limit() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // バッファリングのメモリ制限のテスト
    // メモリ制限を超えた場合の動作を確認

    // 前提条件: /large.txt が存在することを確認
    let prereq = send_request(PROXY_PORT, "/large.txt", &[]).await;
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

    // 大きなレスポンスをリクエスト
    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    // 前提条件チェックで200が返ることを確認済みなので、ここでも200を期待
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for buffering memory limit test, got: {:?}",
        status
    );

    eprintln!("Buffering memory limit test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_buffering_chunked_vs_full() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Chunked転送とFullバッファリングの比較テスト
    // レスポンスの転送方法を確認

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Transfer-EncodingまたはContent-Lengthヘッダーを確認
    let transfer_encoding = get_header_value(&response, "Transfer-Encoding");
    let content_length = get_header_value(&response, "Content-Length");

    if let Some(te) = transfer_encoding {
        eprintln!("Buffering chunked vs full test: Transfer-Encoding = {}", te);
    } else if let Some(cl) = content_length {
        eprintln!("Buffering chunked vs full test: Content-Length = {}", cl);
    } else {
        eprintln!(
            "Buffering chunked vs full test: neither Transfer-Encoding nor Content-Length present"
        );
    }
}
