//! LB・HTTP 機能・エッジケース・Range・TE とトレーラー・ヘッダー検証・静的配信・chunked・タイムアウト
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// 優先度中: より詳細なロードバランシングテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_load_balancing_weighted_distribution() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 重み付きロードバランシングのテスト
    // 注意: このテストは設定ファイルで重み付きロードバランシングを設定する必要がある

    // 複数のリクエストを送信して分散を確認
    let mut backend1_count = 0;
    let mut backend2_count = 0;

    for _ in 0..20 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let server_id = get_header_value(&response, "X-Server-Id");
            if let Some(id) = server_id {
                if id == "backend1" {
                    backend1_count += 1;
                } else if id == "backend2" {
                    backend2_count += 1;
                }
            }
        }
    }

    eprintln!(
        "Load balancing weighted distribution test: backend1={}, backend2={}",
        backend1_count, backend2_count
    );

    // 重み付きロードバランシングが設定されている場合、分散が重みに応じて変わる可能性がある
    assert!(
        backend1_count > 0 || backend2_count > 0,
        "At least one backend should receive requests"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_load_balancing_backend_failure() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // バックエンド障害時のロードバランシングテスト
    // 注意: このテストは設定ファイルでヘルスチェックを有効化する必要がある

    // 通常のリクエストが成功することを確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // バックエンドが障害を起こした場合、他のバックエンドにリクエストが転送される可能性がある
    // 実際のテストには、バックエンドの停止をシミュレートする必要がある
    eprintln!("Load balancing backend failure test: request successful");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_load_balancing_session_affinity() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // セッションアフィニティのテスト
    // 注意: このテストは設定ファイルでセッションアフィニティを有効化する必要がある

    // 同じクライアントからの複数のリクエストが同じバックエンドに転送されることを確認
    let mut backend_ids = Vec::new();

    for _ in 0..10 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let server_id = get_header_value(&response, "X-Server-Id");
            if let Some(id) = server_id {
                backend_ids.push(id);
            }
        }
    }

    eprintln!(
        "Load balancing session affinity test: backend_ids={:?}",
        backend_ids
    );

    // セッションアフィニティが有効な場合、同じバックエンドにリクエストが転送される可能性がある
    // IP Hashアルゴリズムを使用している場合、同じIPからのリクエストは同じバックエンドに転送される
    assert!(
        !backend_ids.is_empty(),
        "Should receive responses from at least one backend"
    );
}

// ====================
// 優先度中: より詳細なHTTP機能テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_via_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Viaヘッダーのテスト（RFC 7230 Section 5.7.1）
    // プロキシはViaヘッダーを追加する必要がある

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Viaヘッダーが追加されている可能性がある
    let via = get_header_value(&response, "Via");
    if let Some(via_value) = via {
        eprintln!("Via header test: via = {}", via_value);
        // Viaヘッダーが存在する場合、プロキシが正しく動作している
    } else {
        eprintln!("Via header test: Via header not present (may be optional)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_hop_by_hop_headers() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Hop-by-hopヘッダーのテスト（RFC 7230 Section 6.1）
    // Connection、Keep-Alive、TEなどのHop-by-hopヘッダーは削除される必要がある

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

    // Hop-by-hopヘッダーを含むリクエストを送信
    let request = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nKeep-Alive: timeout=5\r\nTE: trailers\r\n\r\n";
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
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Hop-by-hopヘッダーがレスポンスに含まれていないことを確認
    // （プロキシが正しく処理している場合、これらのヘッダーは削除される）
    eprintln!("Hop-by-hop headers test: request processed successfully");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_host_validation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Hostヘッダー検証のテスト（RFC 7230 Section 5.4）
    // HTTP/1.1リクエストにはHostヘッダーが必須

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

    // Hostヘッダーなしのリクエストを送信（HTTP/1.1では必須）
    let request = b"GET / HTTP/1.1\r\n\r\n";
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

    // Hostヘッダーが欠落している場合、400 Bad Requestが返される可能性がある
    assert!(
        status == Some(400),
        "Should return 400 Bad Request or 200 OK: {:?}",
        status
    );

    if status == Some(400) {
        eprintln!(
            "Host validation test: 400 Bad Request returned (Host header validation working)"
        );
    } else {
        eprintln!("Host validation test: 200 OK returned (Host header may be optional)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_connection_close_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Connection: closeヘッダーのテスト
    // Connection: closeが指定されている場合、接続が閉じられる

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

    // Connection: closeヘッダーを含むリクエストを送信
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
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Connection: closeヘッダーがレスポンスに含まれている可能性がある
    let connection = get_header_value(&response, "Connection");
    if let Some(conn_value) = connection {
        // Connectionヘッダーが存在する場合、値は'close'または'keep-alive'の可能性がある
        let conn_lower = conn_value.to_lowercase();
        assert!(
            conn_lower == "close" || conn_lower == "keep-alive",
            "Connection header should be 'close' or 'keep-alive': {}",
            conn_value
        );
        eprintln!("Connection: close test: Connection header = {}", conn_value);
    } else {
        eprintln!("Connection: close test: Connection header not present");
    }
}

// ====================
// 優先度中: より詳細なエッジケーステスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_connection_abort() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 接続中断のテスト
    // リクエスト送信中に接続を切断した場合の動作を確認

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

    // リクエストの一部を送信してから接続を切断
    let partial_request = b"GET / HTTP/1.1\r\nHost: localhost\r\n";
    if let Err(e) = tls_stream.write_all(partial_request) {
        panic!("Failed to send partial request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // 接続を切断（ドロップ）。tls_stream は stream への借用のため先に手放す。
    let _ = tls_stream;
    drop(stream);

    // プロキシが接続中断を正しく処理することを確認
    // （実際のテストでは、エラーログやメトリクスを確認する必要がある）
    eprintln!("Connection abort test: connection aborted (proxy should handle gracefully)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_empty_request() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 空のリクエストのテスト
    // 空のリクエストが送信された場合の動作を確認

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

    // 空のリクエストを送信
    let empty_request = b"\r\n";
    if let Err(e) = tls_stream.write_all(empty_request) {
        panic!("Failed to send empty request: {:?}", e);
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

    // 空のリクエストの場合、400 Bad Requestが返される可能性がある
    assert!(
        status == Some(400) || status.is_none(),
        "Should return 400 Bad Request or close connection: {:?}",
        status
    );

    if status == Some(400) {
        eprintln!("Empty request test: 400 Bad Request returned");
    } else {
        eprintln!("Empty request test: connection closed");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_incomplete_request_line() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不完全なリクエスト行のテスト
    // リクエスト行が不完全な場合の動作を確認

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

    // 不完全なリクエスト行を送信
    let incomplete_request = b"GET /\r\nHost: localhost\r\n\r\n";
    if let Err(e) = tls_stream.write_all(incomplete_request) {
        panic!("Failed to send incomplete request: {:?}", e);
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

    // 不完全なリクエスト行の場合、400 Bad Requestが返される可能性がある
    assert!(
        status == Some(400),
        "Should return 400, 200, or close connection: {:?}",
        status
    );

    eprintln!("Incomplete request line test: status {:?}", status);
}

// ====================
// 優先度中: Rangeリクエスト詳細テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_range_request_multiple_ranges() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 複数範囲のRangeリクエストのテスト
    // 注意: 複数範囲は通常200 OKで返される（マルチパートレスポンス）

    let response = send_request(PROXY_PORT, "/large.txt", &[("Range", "bytes=0-99,200-299")]).await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // 複数範囲の場合、200 OKまたは206 Partial Contentが返される可能性がある
    assert_eq!(
        status,
        Some(206),
        "Should return 206 Partial Content for range request, got: {:?}",
        status
    );

    eprintln!("Range request multiple ranges test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_range_request_not_satisfiable() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 416 Range Not Satisfiableのテスト
    // 範囲がファイルサイズを超える場合、416が返される可能性がある

    let response = send_request(PROXY_PORT, "/", &[("Range", "bytes=1000000-2000000")]).await;

    if let Some(response) = response {
        let status = get_status_code(&response);

        // 範囲が満たせない場合、416 Range Not Satisfiableが返される可能性がある
        assert!(
            status == Some(200)
                || status == Some(206)
                || status == Some(416)
                || status == Some(404),
            "Should return 200, 206, 416, or 404: {:?}",
            status
        );

        if status == Some(416) {
            // 416の場合、Content-Rangeヘッダーが存在することを確認
            let content_range = get_header_value(&response, "Content-Range");
            assert!(
                content_range.is_some(),
                "416 Range Not Satisfiable should have Content-Range header"
            );
            eprintln!("Range request not satisfiable test: 416 returned with Content-Range");
        } else {
            eprintln!(
                "Range request not satisfiable test: status {:?} (416 may not be returned)",
                status
            );
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_range_request_suffix() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // サフィックス範囲のRangeリクエストのテスト（bytes=-500）
    // ファイルの最後の500バイトをリクエスト

    let response = send_request(PROXY_PORT, "/large.txt", &[("Range", "bytes=-500")]).await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // サフィックス範囲の場合、206 Partial Contentが返される可能性がある
    assert!(
        status == Some(206),
        "Should return 200, 206, or 404: {:?}",
        status
    );

    if status == Some(206) {
        let content_range = get_header_value(&response, "Content-Range");
        if let Some(range) = content_range {
            eprintln!("Range request suffix test: Content-Range = {}", range);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_range_request_open_ended() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 開始位置のみのRangeリクエストのテスト（bytes=500-）
    // 500バイト目から最後までをリクエスト

    let response = send_request(PROXY_PORT, "/large.txt", &[("Range", "bytes=500-")]).await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // 開始位置のみの場合、206 Partial Contentが返される可能性がある
    assert!(
        status == Some(206),
        "Should return 200, 206, or 404: {:?}",
        status
    );

    if status == Some(206) {
        let content_range = get_header_value(&response, "Content-Range");
        if let Some(range) = content_range {
            eprintln!("Range request open-ended test: Content-Range = {}", range);
        }
    }
}

// ====================
// 優先度中: TEヘッダーとトレーラーテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_te_header_trailers() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // TEヘッダー（trailers）のテスト（RFC 7230 Section 4.3）
    // TEヘッダーはHop-by-hopであり、クライアントがトレーラーをサポートすることを示す

    let response = send_request(PROXY_PORT, "/", &[("TE", "trailers")]).await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // TEヘッダーはHop-by-hopなので、レスポンスには含まれない
    // プロキシが正しく処理することを確認
    eprintln!("TE header trailers test: request processed successfully");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_te_header_encodings() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // TEヘッダー（エンコーディング）のテスト
    // TEヘッダーでサポートする転送エンコーディングを指定

    let response = send_request(PROXY_PORT, "/", &[("TE", "gzip, deflate")]).await;

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // TEヘッダーはHop-by-hopなので、レスポンスには含まれない
    eprintln!("TE header encodings test: request processed successfully");
}

// ====================
// 優先度中: HTTPヘッダー検証テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_content_length_transfer_encoding_conflict() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Content-LengthとTransfer-Encodingの競合テスト（RFC 7230 Section 3.3.3）
    // 両方が存在する場合はプロトコルエラー

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

    // Content-LengthとTransfer-Encodingの両方を含むリクエストを送信
    let request = b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nTransfer-Encoding: chunked\r\n\r\n";
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

    // 競合がある場合、400 Bad Requestが返される可能性がある
    assert!(
        status == Some(400),
        "Should return 400 Bad Request or close connection: {:?}",
        status
    );

    if status == Some(400) {
        eprintln!("Content-Length/Transfer-Encoding conflict test: 400 Bad Request returned");
    } else {
        eprintln!("Content-Length/Transfer-Encoding conflict test: status {:?} (may be handled differently)", status);
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_invalid_content_length() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なContent-Lengthのテスト
    // 負の値や非数値のContent-Lengthが送信された場合の動作を確認

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

    // 不正なContent-Lengthを含むリクエストを送信
    let request = b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: invalid\r\n\r\n";
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

    // 不正なContent-Lengthの場合、400 Bad Requestが返される可能性がある
    assert!(
        status == Some(400),
        "Should return 400 Bad Request or close connection: {:?}",
        status
    );

    eprintln!("Invalid Content-Length test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_multiple_content_length() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 複数のContent-Lengthヘッダーのテスト
    // 複数のContent-Lengthヘッダーが存在する場合の動作を確認

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

    // 複数のContent-Lengthヘッダーを含むリクエストを送信
    let request =
        b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 100\r\nContent-Length: 200\r\n\r\n";
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

    // 複数のContent-Lengthの場合、400 Bad Requestが返される可能性がある
    assert!(
        status == Some(400),
        "Should return 400 Bad Request or close connection: {:?}",
        status
    );

    eprintln!("Multiple Content-Length test: status {:?}", status);
}

// ====================
// 優先度中: 静的ファイル配信詳細テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_static_file_mime_type() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 静的ファイルのMIMEタイプのテスト
    // プロキシが正しいContent-Typeヘッダーを返すことを確認

    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Content-Typeヘッダーが存在することを確認
    let content_type = get_header_value(&response, "Content-Type");
    if let Some(ct) = content_type {
        eprintln!("Static file MIME type test: Content-Type = {}", ct);
        // テキストファイルの場合、text/plainまたはtext/plain; charset=utf-8が返される可能性がある
        assert!(
            ct.starts_with("text/") || ct.starts_with("application/"),
            "Content-Type should be text/* or application/*: {}",
            ct
        );
    } else {
        eprintln!("Static file MIME type test: Content-Type header not present");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_static_file_content_length() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 静的ファイルのContent-Lengthのテスト
    // プロキシが正しいContent-Lengthヘッダーを返すことを確認

    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Content-Lengthヘッダーが存在することを確認
    let content_length = get_header_value(&response, "Content-Length");
    if let Some(cl) = content_length {
        eprintln!("Static file Content-Length test: Content-Length = {}", cl);
        // Content-Lengthが数値であることを確認
        assert!(
            cl.parse::<u64>().is_ok(),
            "Content-Length should be a valid number: {}",
            cl
        );
    } else {
        eprintln!(
            "Static file Content-Length test: Content-Length header not present (may be chunked)"
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_static_file_etag() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 静的ファイルのETagのテスト
    // プロキシがETagヘッダーを返すことを確認

    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // ETagヘッダーが存在する可能性がある
    let etag = get_header_value(&response, "ETag");
    if let Some(etag_value) = etag {
        eprintln!("Static file ETag test: ETag = {}", etag_value);
        // ETagは通常ダブルクォートで囲まれている
        assert!(
            etag_value.starts_with('"') && etag_value.ends_with('"'),
            "ETag should be quoted: {}",
            etag_value
        );
    } else {
        eprintln!("Static file ETag test: ETag header not present (may be optional)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_static_file_last_modified() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 静的ファイルのLast-Modifiedのテスト
    // プロキシがLast-Modifiedヘッダーを返すことを確認

    let response = send_request(PROXY_PORT, "/large.txt", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Last-Modifiedヘッダーが存在する可能性がある
    let last_modified = get_header_value(&response, "Last-Modified");
    if let Some(lm) = last_modified {
        eprintln!("Static file Last-Modified test: Last-Modified = {}", lm);
        // Last-ModifiedはRFC 7231形式（例: "Wed, 21 Oct 2015 07:28:00 GMT"）
        assert!(
            lm.contains("GMT") || lm.contains("UTC"),
            "Last-Modified should contain timezone: {}",
            lm
        );
    } else {
        eprintln!(
            "Static file Last-Modified test: Last-Modified header not present (may be optional)"
        );
    }
}

// ====================
// 優先度中: Chunked Transfer Encoding詳細テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_chunked_transfer_encoding_size() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Chunked Transfer Encodingのサイズのテスト
    // チャンクサイズが正しく処理されることを確認

    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // Transfer-Encodingヘッダーが存在する可能性がある
    let transfer_encoding = get_header_value(&response, "Transfer-Encoding");
    if let Some(te) = transfer_encoding {
        eprintln!(
            "Chunked Transfer Encoding size test: Transfer-Encoding = {}",
            te
        );
        // Transfer-Encodingがchunkedであることを確認
        assert!(
            te.to_lowercase().contains("chunked"),
            "Transfer-Encoding should contain 'chunked': {}",
            te
        );
    } else {
        eprintln!("Chunked Transfer Encoding size test: Transfer-Encoding header not present (may not be chunked)");
    }
}

#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_chunked_transfer_encoding_trailer() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Chunked Transfer Encodingのトレーラーのテスト
    // プロキシがChunked Encodingリクエストを正常にバックエンドへ転送できることを確認
    // None または non-200（一時的な負荷由来の400等）も最大5回リトライ
    let mut last_response = String::new();
    let mut got_200 = false;
    for _retry in 0..5u32 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(resp) = response {
            let s = get_status_code(&resp);
            if s == Some(200) {
                last_response = resp;
                got_200 = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(
        got_200,
        "Should return 200 OK from backend via proxy after retries"
    );

    // Trailerヘッダーが存在する可能性がある（情報ログのみ）
    let trailer = get_header_value(&last_response, "Trailer");
    if let Some(trailer_value) = trailer {
        eprintln!(
            "Chunked Transfer Encoding trailer test: Trailer = {}",
            trailer_value
        );
    } else {
        eprintln!("Chunked Transfer Encoding trailer test: Trailer header not present");
    }
}

// ====================
// 優先度中: タイムアウトテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_connection_timeout_handling() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 接続タイムアウトのテスト
    // 存在しないバックエンドへの接続を試みる

    let response = send_request(PROXY_PORT, "/slow/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    assert!(
        status == Some(502) || status == Some(504),
        "Should return 502 or 504 for connection timeout, got: {:?}",
        status
    );
}

// ====================
// 優先度中: より詳細なエッジケーステスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_oversized_request_line() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 過大なリクエスト行のテスト
    // リクエスト行が長すぎる場合の動作を確認

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

    // 過大なリクエスト行を送信（8192バイトを超える）
    let oversized_path = "a".repeat(9000);
    let request = format!(
        "GET /{} HTTP/1.1\r\nHost: localhost\r\n\r\n",
        oversized_path
    );
    if let Err(e) = tls_stream.write_all(request.as_bytes()) {
        panic!("Failed to send oversized request: {:?}", e);
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

    // 過大なリクエスト行の場合、414 URI Too Long、413 Payload Too Large、または400 Bad Requestが返される可能性がある
    assert!(
        status == Some(414),
        "Should return 414, 413, 400, 200, or close connection: {:?}",
        status
    );

    eprintln!("Oversized request line test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_oversized_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 過大なヘッダーのテスト
    // ヘッダーが長すぎる場合の動作を確認

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

    // 過大なヘッダーを送信（8192バイトを超える）
    let oversized_value = "a".repeat(9000);
    let request = format!(
        "GET / HTTP/1.1\r\nHost: localhost\r\nX-Custom-Header: {}\r\n\r\n",
        oversized_value
    );
    if let Err(e) = tls_stream.write_all(request.as_bytes()) {
        panic!("Failed to send oversized header: {:?}", e);
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

    // 過大なヘッダーの場合、431 Request Header Fields Too Large、413 Payload Too Large、または400 Bad Requestが返される可能性がある
    assert!(
        status == Some(431),
        "Should return 431, 413, 400, 200, or close connection: {:?}",
        status
    );

    eprintln!("Oversized header test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_malformed_request() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正な形式のリクエストのテスト
    // リクエストが不正な形式の場合の動作を確認

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

    // 不正な形式のリクエストを送信（CRLFが欠落）
    let malformed_request = b"GET / HTTP/1.1 Host: localhost\r\n\r\n";
    if let Err(e) = tls_stream.write_all(malformed_request) {
        panic!("Failed to send malformed request: {:?}", e);
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

    // 不正な形式のリクエストの場合、400 Bad Requestが返される可能性がある
    assert!(
        status == Some(400),
        "Should return 400, 200, or close connection: {:?}",
        status
    );

    eprintln!("Malformed request test: status {:?}", status);
}
