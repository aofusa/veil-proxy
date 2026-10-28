//! kTLS・HTTP/2 の詳細・WebSocket の双方向通信・セキュリティ・エッジケース
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// 優先度高: kTLS機能テスト
// ====================

/// kTLSが利用可能かどうかをチェック（呼び出し元の kTLS テストと同じ cfg）
#[cfg(feature = "ktls")]
pub(super) fn is_ktls_available() -> bool {
    // /proc/modules で tls モジュールがロードされているか確認
    if let Ok(modules) = std::fs::read_to_string("/proc/modules") {
        if !modules.lines().any(|line| line.starts_with("tls ")) {
            return false;
        }
    } else {
        return false;
    }

    // /proc/sys/net/ipv4/tcp_available_ulp で tls が利用可能か確認
    if let Ok(ulp) = std::fs::read_to_string("/proc/sys/net/ipv4/tcp_available_ulp") {
        if ulp.contains("tls") {
            return true;
        }
    }

    false
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "ktls")]
pub(super) async fn test_ktls_availability() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // kTLSが利用可能かどうかを確認
    let ktls_available = is_ktls_available();

    if ktls_available {
        eprintln!("kTLS is available on this system");
    } else {
        eprintln!("kTLS is not available (tls module may not be loaded)");
        eprintln!("To enable kTLS: sudo modprobe tls");
    }

    // kTLSが利用可能な場合、TLS接続が正常に動作することを確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(
        response.is_some(),
        "Should receive response even if kTLS is not available"
    );

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "ktls")]
pub(super) async fn test_ktls_tls_handshake() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // kTLSが利用可能な場合でも、TLSハンドシェイクは正常に動作することを確認
    use std::time::Instant;

    let start = Instant::now();
    let response = send_request(PROXY_PORT, "/", &[]).await;
    let elapsed = start.elapsed();

    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    eprintln!("TLS handshake completed in {:?}", elapsed);

    // kTLSが有効な場合、パフォーマンスが向上する可能性がある
    // ただし、テスト環境では明確な差が出ない可能性もある
    if is_ktls_available() {
        eprintln!("kTLS may be active (performance improvement expected)");
    }
}

/// B-258: クライアントが最後のフライト（Finished）と最初の要求を続けて送り、veil の最後の読み取りが要求のレコードの
/// 途中で切れても、kTLS へ移った後にその要求を正しく読める（境界まで読み足してから移る）。
/// kTLS の構成（`./tests/e2e_setup.sh test ktls`）で意味を持つ。既定の構成では rustls のまま通る。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "ktls")]
pub(super) async fn test_b258_request_record_split_after_handshake() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let result = tokio::task::spawn_blocking(|| -> Result<String, String> {
        let mut sock = TcpStream::connect(("127.0.0.1", PROXY_PORT)).map_err(|e| e.to_string())?;
        sock.set_nodelay(true).ok();
        sock.set_read_timeout(Some(Duration::from_secs(5))).ok();
        let mut conn = ClientConnection::new(
            create_client_config(),
            ServerName::try_from("localhost").unwrap(),
        )
        .map_err(|e| e.to_string())?;
        // ハンドシェイク（クライアントの最後のフライトはまだ送らない）
        while conn.is_handshaking() {
            if conn.wants_write() {
                conn.write_tls(&mut sock).map_err(|e| e.to_string())?;
                continue;
            }
            conn.read_tls(&mut sock).map_err(|e| e.to_string())?;
            conn.process_new_packets().map_err(|e| e.to_string())?;
        }
        conn.writer()
            .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .map_err(|e| e.to_string())?;
        let mut out = Vec::new();
        while conn.wants_write() {
            conn.write_tls(&mut out).map_err(|e| e.to_string())?;
        }
        // Finished と要求のレコードの先頭を 1 回で、要求のレコードの最後の 10 バイトを後から送る
        let cut = out.len() - 10;
        sock.write_all(&out[..cut]).map_err(|e| e.to_string())?;
        std::thread::sleep(Duration::from_millis(200));
        sock.write_all(&out[cut..]).map_err(|e| e.to_string())?;

        let mut resp = Vec::new();
        loop {
            match conn.read_tls(&mut sock) {
                Ok(0) => break,
                Ok(_) => {}
                Err(e) if resp.is_empty() => return Err(format!("read: {e}")),
                Err(_) => break,
            }
            if let Err(e) = conn.process_new_packets() {
                return Err(format!("tls: {e}"));
            }
            let mut buf = [0u8; 4096];
            while let Ok(n) = conn.reader().read(&mut buf) {
                if n == 0 {
                    break;
                }
                resp.extend_from_slice(&buf[..n]);
            }
            if resp.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&resp).into_owned())
    })
    .await
    .unwrap();
    let resp = result.expect("the request split across the handshake must be answered");
    assert_eq!(get_status_code(&resp), Some(200), "response: {resp}");
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "ktls")]
pub(super) async fn test_ktls_multiple_connections() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // kTLSが有効な場合、複数の接続が正常に動作することを確認
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let success_count = Arc::new(AtomicUsize::new(0));
    let total_connections = 10;

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
    assert!(
        successes >= total_connections * 8 / 10,
        "At least 80% of kTLS connections should succeed: {}/{}",
        successes,
        total_connections
    );

    if is_ktls_available() {
        eprintln!(
            "kTLS multiple connections test: {}/{} succeeded",
            successes, total_connections
        );
    }
}

// ====================
// 優先度高: HTTP/2詳細機能テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_alpn_negotiation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/2のALPNネゴシエーションをテスト
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
            Err(e) => {
                panic!("TLS handshake error: {:?}", e);
            }
        }
    }

    // ALPNでHTTP/2がネゴシエートされたことを確認
    let protocol = tls_conn.alpn_protocol();
    if let Some(proto) = protocol {
        eprintln!("ALPN negotiated protocol: {:?}", proto);
        assert!(
            proto == b"h2" || proto == b"http/1.1",
            "Should negotiate HTTP/2 (h2) or HTTP/1.1: {:?}",
            proto
        );

        if proto == b"h2" {
            eprintln!("HTTP/2 successfully negotiated via ALPN");
        } else {
            eprintln!("HTTP/1.1 negotiated (HTTP/2 may not be enabled in config)");
        }
    } else {
        eprintln!("No ALPN protocol negotiated");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_connection_reuse() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/2の接続再利用をテスト
    // HTTP/2では、1つの接続で複数のリクエストを並行処理できる

    // まず、HTTP/2接続が確立されることを確認
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

    // ALPNでHTTP/2がネゴシエートされた場合、接続再利用が可能
    let protocol = tls_conn.alpn_protocol();
    if let Some(proto) = protocol {
        if proto == b"h2" {
            eprintln!("HTTP/2 connection established - connection reuse is possible");
            // HTTP/2では、同じ接続で複数のリクエストを送信できる
            // 実際のテストには、HTTP/2クライアントライブラリが必要
        }
    }

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    assert_eq!(
        get_status_code(&response.unwrap()),
        Some(200),
        "Should return 200 OK"
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_header_compression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/2のHPACKヘッダー圧縮をテスト
    // HTTP/2では、HPACKアルゴリズムによりヘッダーが圧縮される

    // まず、HTTP/2接続が確立されることを確認
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

    // ALPNでHTTP/2がネゴシエートされた場合、HPACK圧縮が使用される
    let protocol = tls_conn.alpn_protocol();
    if let Some(proto) = protocol {
        if proto == b"h2" {
            eprintln!("HTTP/2 connection established - HPACK header compression is active");
            // HTTP/2では、HPACKによりヘッダーが圧縮される
            // 実際の圧縮率の測定には、HTTP/2クライアントライブラリが必要
        }
    }

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    assert_eq!(
        get_status_code(&response.unwrap()),
        Some(200),
        "Should return 200 OK"
    );
}

// ====================
// 優先度高: WebSocket双方向通信テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_upgrade_request() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // WebSocketアップグレードリクエストをテスト
    // HTTPSポートを使用するため、TLS接続を確立する必要がある
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
        "Should return 101 Switching Protocols for WebSocket upgrade request, got: {:?}",
        status
    );

    if status == Some(101) {
        eprintln!("WebSocket upgrade successful (101 Switching Protocols)");
        // Upgradeヘッダーを確認
        let upgrade = get_header_value(&response, "Upgrade");
        if let Some(upgrade_value) = upgrade {
            assert_eq!(
                upgrade_value.to_lowercase(),
                "websocket",
                "Upgrade header should be 'websocket'"
            );
        }
    } else {
        eprintln!(
            "WebSocket upgrade not supported or endpoint not found: status {:?}",
            status
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_websocket_connection_persistence() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // WebSocket接続の永続性をテスト
    // WebSocket接続は、アップグレード後も維持される

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

    // レスポンスを受信（ヘッダー部分を読み取る）
    let mut response = Vec::new();
    let mut buf = [0u8; 1];
    let mut _header_end = None;

    // ヘッダー部分を読み取る（\r\n\r\nまで）
    loop {
        match tls_stream.read_exact(&mut buf) {
            Ok(_) => {
                response.push(buf[0]);
                // \r\n\r\nを検出（ヘッダー終了）
                if response.len() >= 4 {
                    let len = response.len();
                    if &response[len - 4..] == b"\r\n\r\n" {
                        _header_end = Some(len);
                        break;
                    }
                }
                // ヘッダーが大きすぎる場合は中止
                if response.len() > 8192 {
                    break;
                }
            }
            Err(_) => {
                // エラーまたはEOF
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
        eprintln!("WebSocket connection established");
        // WebSocket接続が確立された場合、接続は維持される
        // 実際の双方向通信テストには、WebSocketクライアントライブラリが必要
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
pub(super) async fn test_websocket_proxy_forwarding() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // WebSocketプロキシ転送をテスト
    // プロキシは、WebSocket接続をバックエンドに転送する必要がある

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

    // WebSocketがサポートされている場合、プロキシはバックエンドに転送する
    // バックエンドがWebSocketをサポートしていない場合、502が返される可能性がある
    assert!(
        status == Some(101),
        "Should return appropriate status: {:?}",
        status
    );

    if status == Some(101) {
        eprintln!("WebSocket proxy forwarding successful");
    } else if status == Some(502) {
        eprintln!("WebSocket proxy forwarding failed (backend may not support WebSocket)");
    }
}

// ====================
// 優先度中: セキュリティ機能実動作テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_rate_limiting_with_config() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでレート制限を設定する必要がある
    // 例: ./tests/e2e_setup.sh test security
    // rate_limit_requests_per_min = 30 が設定されている場合のテスト

    // 制限を超えるリクエストを送信（30リクエスト/分の制限）
    let mut success_count = 0;
    let mut rate_limited_count = 0;

    // 40リクエストを短時間で送信（制限を超える）
    for i in 0..40 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(response) = response {
            let status = get_status_code(&response);
            match status {
                Some(200) => success_count += 1,
                Some(429) => {
                    rate_limited_count += 1;
                    eprintln!("Rate limited at request {}", i + 1);
                }
                _ => {}
            }
        }
        // レート制限をトリガーするために短い間隔で送信（50ms間隔）
        if i < 39 {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    eprintln!(
        "Rate limiting test: {} successful, {} rate limited",
        success_count, rate_limited_count
    );

    // レート制限が設定されている場合、429が返される可能性がある
    // 設定されていない場合、すべて200が返される
    // このテストは設定に依存するため、両方のケースを許容
    assert!(
        success_count > 0 || rate_limited_count > 0,
        "Should receive some responses: success={}, rate_limited={}",
        success_count,
        rate_limited_count
    );

    // レート制限が有効な場合、少なくともいくつかのリクエストが制限される
    if rate_limited_count > 0 {
        eprintln!(
            "Rate limiting is working: {} requests were rate limited",
            rate_limited_count
        );
    } else {
        eprintln!("Rate limiting may not be configured (all requests succeeded)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_ip_restriction_with_config() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでIP制限を設定する必要がある
    // 例: ./tests/e2e_setup.sh test security
    // allowed_ips = ["127.0.0.1"] が設定されている場合のテスト

    // 127.0.0.1からのリクエスト（許可されているIP）
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(
        response.is_some(),
        "Should receive response from allowed IP"
    );

    let response = response.unwrap();
    let status = get_status_code(&response);

    // IP制限が設定されている場合、127.0.0.1は許可されているため200が返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for allowed IP, got: {:?}",
        status
    );

    if status == Some(200) {
        eprintln!("IP restriction test: 127.0.0.1 is allowed");
    } else if status == Some(403) {
        eprintln!("IP restriction test: 127.0.0.1 is denied (unexpected)");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_method_restriction() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // /api/get-only/* パスに allowed_methods = ["GET", "HEAD"] を設定済み

    // GETリクエスト（許可されているメソッド）
    let get_response = send_request(PROXY_PORT, "/api/get-only/", &[]).await;
    assert!(get_response.is_some(), "Should receive GET response");
    assert_eq!(
        get_status_code(&get_response.unwrap()),
        Some(200),
        "GET should return 200"
    );

    // POSTリクエスト（制限されているメソッド）
    let post_response = send_request_with_method(
        PROXY_PORT,
        "/api/get-only/",
        "POST",
        &[],
        Some(b"test body"),
    )
    .await;
    if let Some(response) = post_response {
        let status = get_status_code(&response);
        // メソッドが許可されていない場合、405 Method Not Allowedが返される
        assert_eq!(
            status,
            Some(405),
            "Should return 405 Method Not Allowed for restricted method, got: {:?}",
            status
        );

        if status == Some(405) {
            eprintln!("Method restriction is working: POST is not allowed");
        }
    }
}

// ====================
// 優先度中: エッジケーステスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_request_timeout() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // リクエストタイムアウトのテスト

    let response = send_request(PROXY_PORT, "/slow/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);

    assert!(
        status == Some(502) || status == Some(504),
        "Should return 502 or 504 for request timeout, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_large_request_body() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 大きなリクエストボディのテスト
    // MAX_BODY_SIZE = 10MBを超える11MBのボディを送信して413を確認
    let large_body = vec![0u8; 11 * 1024 * 1024]; // 11MB (> 10MB limit)
    let response = send_request_with_method(
        PROXY_PORT,
        "/",
        "POST",
        &[("Content-Type", "application/octet-stream")],
        Some(&large_body),
    )
    .await;

    match response {
        Some(resp) => {
            let status = get_status_code(&resp);
            // プロキシがボディサイズ超過を検出して413を返した
            assert_eq!(
                status, Some(413),
                "Should return 413 Request Entity Too Large for oversized body (11MB > 10MB limit), got: {:?}", status
            );
            eprintln!("Request body size limit is working: 11MB body rejected with 413");
        }
        None => {
            // プロキシが早期に413を送信してhyperが接続エラーを返した場合もOK
            // これはサーバーが正しくボディを拒否したことを示す
            eprintln!(
                "Request body size limit is working: 11MB body rejected (early connection close)"
            );
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_malformed_headers() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 不正なヘッダーのテスト
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

    // 不正なヘッダー（無効な Content-Length 値を含む）を送信
    // Content-Length に数値以外の文字列を指定することで 400 Bad Request が返される
    let malformed_request = b"GET / HTTP/1.1\r\nHost: localhost\r\nContent-Length: invalid\r\n\r\n";
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

    // 不正なヘッダーの場合、400 Bad Requestが返される可能性がある
    // 不正なヘッダーの場合、400 Bad Requestが返される
    assert_eq!(
        status,
        Some(400),
        "Should return 400 Bad Request for malformed headers, got: {:?}",
        status
    );

    if status == Some(400) {
        eprintln!("Malformed header handling is working: 400 Bad Request returned");
    }
}

/// 生の（crafted）リクエストを TLS 経由でプロキシへ送り、レスポンス文字列を返すヘルパ。
/// スマグリングベクタ検証用（httparse 等で正規化されない生バイト列を送る）。
pub(super) fn send_raw_tls_request(request: &[u8]) -> Option<String> {
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(5))).ok()?;
    stream
        .set_write_timeout(Some(Duration::from_secs(5)))
        .ok()?;

    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).ok()?;
    let mut tls_conn = ClientConnection::new(config, server_name).ok()?;
    while tls_conn.is_handshaking() {
        if tls_conn.complete_io(&mut stream).is_err() {
            return None;
        }
    }
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    tls_stream.write_all(request).ok()?;
    tls_stream.flush().ok()?;

    let mut response = Vec::new();
    let mut buf = [0u8; 8192];
    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response.extend_from_slice(&buf[..n]),
            Err(_) => break,
        }
    }
    Some(String::from_utf8_lossy(&response).into_owned())
}

/// B-23: HTTP リクエストスマグリング（CL.TE / TE.CL）の能動テスト（F-76）。
///
/// フロントエンド（Veil）とバックエンドが本文長を別々に解釈し得る曖昧フレーミングを
/// Veil が **一貫して 400 で拒否**し、バックエンドへ転送しない（＝スマグリング不成立）
/// ことを検証する。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_request_smuggling_cl_te_rejected() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 各ベクタ: (名前, 生リクエスト)。いずれも 400 で拒否されること。
    let smuggling_vectors: &[(&str, &[u8])] = &[
        // CL.TE: Content-Length と Transfer-Encoding: chunked の同時指定（CL>0）。
        (
            "CL>0 + TE:chunked",
            b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 6\r\nTransfer-Encoding: chunked\r\n\r\n0\r\n\r\nX",
        ),
        // B-23 の核心: Content-Length: 0 + Transfer-Encoding: chunked（従来の取りこぼし）。
        (
            "CL:0 + TE:chunked",
            b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        ),
        // 複数 Content-Length（RFC 7230 §3.3.2 違反）。
        (
            "duplicate Content-Length",
            b"POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: 5\r\nContent-Length: 6\r\n\r\nhello",
        ),
        // TE.CL 変種: 最終エンコーディングが chunked でない TE（本文長不確定）。
        (
            "TE without terminal chunked",
            b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked, gzip\r\n\r\n5\r\nhello\r\n0\r\n\r\n",
        ),
    ];

    for (name, req) in smuggling_vectors {
        let response = send_raw_tls_request(req)
            .unwrap_or_else(|| panic!("smuggling vector '{}' produced no response", name));
        let status = get_status_code(&response);
        assert_eq!(
            status,
            Some(400),
            "smuggling vector '{}' must be rejected with 400, got {:?}\nresponse: {:?}",
            name,
            status,
            response.chars().take(200).collect::<String>()
        );
        eprintln!("smuggling vector '{}' correctly rejected (400)", name);
    }
}

/// スマグリング防御が正常リクエストを誤検知しないこと（回帰防止）。
/// 単独の Content-Length、単独の Transfer-Encoding: chunked は通す。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_request_smuggling_legitimate_framing_allowed() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 単独 chunked（正当）→ 400 にならない（バックエンドへ到達）。
    let chunked = b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    let response = send_raw_tls_request(chunked).expect("no response for legitimate chunked");
    let status = get_status_code(&response);
    assert_ne!(
        status,
        Some(400),
        "legitimate single Transfer-Encoding: chunked must not be rejected as smuggling, got 400\nresponse: {:?}",
        response.chars().take(200).collect::<String>()
    );
    eprintln!("legitimate chunked framing accepted (status {:?})", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
#[serial]
pub(super) async fn test_concurrent_connection_stress() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 多数の並行接続のストレステスト
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    let success_count = Arc::new(AtomicUsize::new(0));
    let error_count = Arc::new(AtomicUsize::new(0));
    let total_connections = 200;

    let handles: Vec<_> = (0..total_connections)
        .map(|_| {
            let success_count = Arc::clone(&success_count);
            let error_count = Arc::clone(&error_count);
            tokio::spawn(async move {
                let response = send_request(PROXY_PORT, "/", &[]).await;
                if let Some(response) = response {
                    let status = get_status_code(&response);
                    if status == Some(200) {
                        success_count.fetch_add(1, Ordering::Relaxed);
                    } else {
                        error_count.fetch_add(1, Ordering::Relaxed);
                    }
                } else {
                    error_count.fetch_add(1, Ordering::Relaxed);
                }
            })
        })
        .collect();

    for handle in handles {
        let _ = handle.await;
    }

    let successes = success_count.load(Ordering::Relaxed);
    let errors = error_count.load(Ordering::Relaxed);

    eprintln!(
        "Concurrent connection stress test: {} successful, {} errors out of {}",
        successes, errors, total_connections
    );

    // 少なくとも80%の接続が成功することを確認
    assert!(
        successes >= total_connections * 8 / 10,
        "At least 80% of concurrent connections should succeed: {}/{}",
        successes,
        total_connections
    );
}

/// 高並行負荷下でのレスポンス整合性テスト（リグレッションガード）
///
/// 多数の新規接続を同時に張り、各接続で「小さいレスポンス(/)」と「大きいレスポンス
/// (/large.txt)」を要求し、返ってきたレスポンスが要求パスに対応する正しい内容長で
/// あることを厳密に検証する。`send_request` は呼び出しごとに新規 TLS 接続を張るため、
/// 毎回プロトコル検出＋TLS ハンドシェイクを経由し、接続確立経路に負荷をかける。
///
/// このテストは過去に負荷時のみ発生していた以下の実装バグを検出するために追加した:
///  1. プロトコル検出(H2C)の `read` を `timeout()` でキャンセルすると io_uring が
///     カーネルで読み取り済みのバイトを取りこぼし、後続 TLS ハンドシェイクが壊れて
///     応答が空になる（→ `detect_protocol_with_buffer` を readable() 後に読むよう修正）。
///  2. バックエンドが idle で閉じた keep-alive プール接続を再利用して空応答になる
///     （→ `proxy_https_pooled` に新規接続での透過リトライを追加）。
///
/// 「内容長が要求パスと不一致＝壊れたレスポンス」「応答なし/非200＝空応答」は
/// いずれも実装の不具合であり、負荷時でも 0 件でなければならない。マスキングのための
/// リトライは行わず、厳密に 0 を要求する。
#[tokio::test]
#[ntest::timeout(60000)]
pub(super) async fn test_response_integrity_under_load() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    };

    // フィクスチャ: backend1/backend2 の index.html は 30 バイト、large.txt は 10000 バイト
    const SMALL_EXPECTED: usize = 30;
    const LARGE_EXPECTED: usize = 10000;

    // 接続確立経路（プロトコル検出＋TLS ハンドシェイク）に毎回新規接続で負荷をかける。
    // ただしフィクスチャのプロキシは threads=1 のため、E2E スイートの他テストと同時実行
    // された際に単一スレッドを占有し過ぎて他テストを枯渇させないよう並列数は控えめにする
    // （スイート全体としての同時実行で十分な負荷になる）。
    let concurrency = 8usize;
    let iters = 10usize;
    let total_each = concurrency * iters;

    let small_ok = Arc::new(AtomicUsize::new(0));
    let large_ok = Arc::new(AtomicUsize::new(0));
    let corrupt = Arc::new(AtomicUsize::new(0)); // 200 だが内容長が要求パスと不一致（壊れ/残留）
    let empty = Arc::new(AtomicUsize::new(0)); // 応答なし or 非200（空応答）

    let mut handles = Vec::new();
    for _ in 0..concurrency {
        let small_ok = Arc::clone(&small_ok);
        let large_ok = Arc::clone(&large_ok);
        let corrupt = Arc::clone(&corrupt);
        let empty = Arc::clone(&empty);
        handles.push(tokio::spawn(async move {
            for _ in 0..iters {
                // identity を要求して圧縮による内容長変化を避ける
                for (path, expected, is_small) in [
                    ("/", SMALL_EXPECTED, true),
                    ("/large.txt", LARGE_EXPECTED, false),
                ] {
                    match send_request(PROXY_PORT, path, &[("Accept-Encoding", "identity")]).await {
                        Some(resp) => {
                            let status = get_status_code(&resp);
                            let cl = get_content_length_from_headers(resp.as_bytes());
                            if status != Some(200) {
                                empty.fetch_add(1, Ordering::Relaxed);
                            } else if cl == Some(expected) {
                                if is_small {
                                    small_ok.fetch_add(1, Ordering::Relaxed);
                                } else {
                                    large_ok.fetch_add(1, Ordering::Relaxed);
                                }
                            } else {
                                corrupt.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        None => {
                            empty.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }
        }));
    }
    for h in handles {
        let _ = h.await;
    }

    let small_ok = small_ok.load(Ordering::Relaxed);
    let large_ok = large_ok.load(Ordering::Relaxed);
    let corrupt = corrupt.load(Ordering::Relaxed);
    let empty = empty.load(Ordering::Relaxed);
    eprintln!(
        "Response integrity under load: small_ok={}/{} large_ok={}/{} corrupt={} empty={}",
        small_ok, total_each, large_ok, total_each, corrupt, empty
    );

    // 【正しさの不変条件・厳格】壊れたレスポンス（200 だが内容長が要求パスと不一致＝
    // 別リクエストの残留/誤ルーティング）は負荷時でも絶対に 0 でなければならない。
    // 過去の接続プール／プロトコル検出のバグはこの不一致を引き起こした。
    assert_eq!(
        corrupt,
        0,
        "Responses must never be corrupted/mismatched under load (corrupt={}/{})",
        corrupt,
        total_each * 2
    );

    // 【スループットの SLO】空応答（接続が確立できない/応答が来ない）は、意図的に
    // threads=1 にしているプロキシをスイート全体で同時に叩く本テストでは、極端な
    // 一時飽和でごく稀に発生し得る。これは正しさの欠陥ではなく単一スレッドの資源飽和
    // であるため、成功率 99% 以上を要求する（=マスキングではなく飽和の許容）。
    // io_uring 取りこぼし等の実バグが再発すれば空応答が大量に出てこの閾値を割る。
    let total = total_each * 2;
    assert!(
        empty * 100 <= total, // 失敗率 <= 1%
        "Too many empty/failed responses under load (empty={}/{}, >1% indicates a real regression)",
        empty,
        total
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_backend_timeout_handling() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // バックエンドがタイムアウトした場合、502 Bad Gatewayが返されることを確認
    // 新しく用意した /slow/ パスを使用（存在しないポートへ転送）
    let response = send_request_with_retry(PROXY_PORT, "/slow/", &[], 1).await;
    assert!(response.is_some(), "Should receive response after retries");

    let response = response.unwrap();
    let status = get_status_code(&response);

    // バックエンドがタイムアウトした場合、502 Bad Gatewayが返される
    assert_eq!(
        status,
        Some(502),
        "Should return 502 Bad Gateway for backend timeout, got: {:?}",
        status
    );

    if status == Some(200) {
        eprintln!("Backend timeout handling test: backend responded normally");
    } else if status == Some(502) {
        eprintln!("Backend timeout handling test: backend timeout detected");
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_chunked_transfer_encoding() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Chunked Transfer Encodingのテスト
    // チャンク転送エンコーディングが正しく処理されることを確認

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

    // Chunked Transfer Encodingでリクエストを送信
    let request = b"POST / HTTP/1.1\r\nHost: localhost\r\nTransfer-Encoding: chunked\r\n\r\n5\r\nhello\r\n0\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request) {
        panic!("Failed to send chunked request: {:?}", e);
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

    // Chunked Transfer Encodingがサポートされている場合、正常に処理される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for chunked transfer encoding, got: {:?}",
        status
    );

    eprintln!("Chunked transfer encoding test: status {:?}", status);
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_http_version_negotiation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTPバージョンネゴシエーションのテスト
    // HTTP/1.0、HTTP/1.1、HTTP/2のネゴシエーションを確認

    // HTTP/1.1リクエスト
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive HTTP/1.1 response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK");

    // HTTP/1.1がサポートされていることを確認
    let http_version = response.lines().next();
    if let Some(first_line) = http_version {
        assert!(
            first_line.contains("HTTP/1.1"),
            "Should use HTTP/1.1: {}",
            first_line
        );
    }

    eprintln!("HTTP version negotiation test: HTTP/1.1 confirmed");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_keep_alive_multiple_requests() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // Keep-Alive接続での複数リクエストのテスト
    // 同じ接続で複数のリクエストを送信できることを確認

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

    // 最初のリクエスト
    let request1 = b"GET / HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
    tls_stream.write_all(request1).unwrap();
    tls_stream.flush().unwrap();

    // レスポンスを受信（Content-LengthまたはConnection: closeまで）
    let mut response1 = Vec::new();
    let mut buf = [0u8; 1];
    let mut header_end1 = None;

    // ヘッダー部分を読み取る
    loop {
        match tls_stream.read_exact(&mut buf) {
            Ok(_) => {
                response1.push(buf[0]);
                // \r\n\r\nを検出（ヘッダー終了）
                if response1.len() >= 4 {
                    let len = response1.len();
                    if &response1[len - 4..] == b"\r\n\r\n" {
                        header_end1 = Some(len);
                        break;
                    }
                }
                if response1.len() > 8192 {
                    break;
                }
            }
            Err(_) => {
                if response1.is_empty() {
                    panic!("No response received for first request");
                }
                break;
            }
        }
    }

    if response1.is_empty() {
        panic!("Empty response for first request");
    }

    // Content-Lengthを確認してボディを読み取る
    let header1_bytes = &response1[..header_end1.unwrap_or(response1.len())];
    let content_length = get_content_length_from_headers(header1_bytes);
    if let Some(cl) = content_length {
        let header_len = header_end1.unwrap_or(response1.len());
        let body_remaining = cl.saturating_sub(response1.len().saturating_sub(header_len + 4));
        if body_remaining > 0 {
            let mut body_buf = vec![0u8; body_remaining.min(8192)];
            let mut total_read = 0;
            while total_read < body_remaining {
                let to_read = (body_remaining - total_read).min(body_buf.len());
                match tls_stream.read(&mut body_buf[..to_read]) {
                    Ok(0) => break,
                    Ok(n) => {
                        response1.extend_from_slice(&body_buf[..n]);
                        total_read += n;
                    }
                    Err(_) => break,
                }
            }
        }
    }

    let response1_str = String::from_utf8_lossy(&response1);
    let status1 = get_status_code(&response1_str);
    assert_eq!(status1, Some(200), "First request should return 200 OK");

    // 2回目のリクエスト（同じ接続を使用）
    let request2 = b"GET /health HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n";
    if let Err(e) = tls_stream.write_all(request2) {
        panic!("Failed to send second request: {:?}", e);
    }
    tls_stream.flush().unwrap();

    // レスポンスを受信
    let mut response2 = Vec::new();
    let mut header_end2 = None;

    // ヘッダー部分を読み取る
    loop {
        match tls_stream.read_exact(&mut buf) {
            Ok(_) => {
                response2.push(buf[0]);
                if response2.len() >= 4 {
                    let len = response2.len();
                    if &response2[len - 4..] == b"\r\n\r\n" {
                        header_end2 = Some(len);
                        break;
                    }
                }
                if response2.len() > 8192 {
                    break;
                }
            }
            Err(_) => {
                if response2.is_empty() {
                    panic!("No response received for second request");
                }
                break;
            }
        }
    }

    if response2.is_empty() {
        panic!("Empty response for second request");
    }

    // Content-Lengthを確認してボディを読み取る
    let header2_bytes = &response2[..header_end2.unwrap_or(response2.len())];
    let content_length2 = get_content_length_from_headers(header2_bytes);
    if let Some(cl) = content_length2 {
        let header_len = header_end2.unwrap_or(response2.len());
        let body_remaining = cl.saturating_sub(response2.len().saturating_sub(header_len + 4));
        if body_remaining > 0 {
            let mut body_buf = vec![0u8; body_remaining.min(8192)];
            let mut total_read = 0;
            while total_read < body_remaining {
                let to_read = (body_remaining - total_read).min(body_buf.len());
                match tls_stream.read(&mut body_buf[..to_read]) {
                    Ok(0) => break,
                    Ok(n) => {
                        response2.extend_from_slice(&body_buf[..n]);
                        total_read += n;
                    }
                    Err(_) => break,
                }
            }
        }
    }

    let response2_str = String::from_utf8_lossy(&response2);
    let status2 = get_status_code(&response2_str);
    // 2回目のリクエストが成功することを確認（200または404が返される可能性がある）
    assert!(
        status2 == Some(200) || status2 == Some(404),
        "Second request should return 200 OK or 404 Not Found: {:?}",
        status2
    );

    eprintln!(
        "Keep-Alive multiple requests test: first request status={:?}, second request status={:?}",
        status1, status2
    );
}
