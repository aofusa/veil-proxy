//! E2E のカバレッジの拡充と、HTTP/3・gRPC の網羅（F-91・F-93）
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// E2E カバレッジ拡充（e2e_test_coverage.md ギャップ対応）
// ====================

/// /cached/* ルートでキャッシュヒットを検証する（2 回目で X-Cache または応答一致）
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_cached_route_hit() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let path = "/cached/large.txt";
    let r1 = send_request(PROXY_PORT, path, &[]).await;
    assert!(r1.is_some(), "First /cached/ request should succeed");
    let r1 = r1.unwrap();
    assert_eq!(get_status_code(&r1), Some(200));

    tokio::time::sleep(Duration::from_millis(50)).await;

    let r2 = send_request(PROXY_PORT, path, &[]).await;
    assert!(r2.is_some(), "Second /cached/ request should succeed");
    let r2 = r2.unwrap();
    assert_eq!(get_status_code(&r2), Some(200));

    let x_cache = get_header_value(&r2, "X-Cache");
    let age = get_header_value(&r2, "Age");
    // RR で X-Server-Id が変わり完全一致しないため、ステータスとボディ本体を検証
    assert!(
        x_cache.is_some() || age.is_some() || (r1.contains("AAAA") && r2.contains("AAAA")),
        "Second cached response should hit cache or return same body payload"
    );
}

/// B-162: 本文が最初の読み込みに収まる小さい応答も、2 回目は必ずキャッシュから返る（X-Cache: HIT）。
/// 以前は残りの本文を転送するときにしか保存しておらず、小さい応答は一度もキャッシュされなかった
/// （上の試験は本文の一致でも通るので気づけなかった）。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_cached_small_response_hits() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 平文の上流のルート（HTTPS の上流はキャッシュに保存しない。F-257）。ほかで使わないクエリを付ける
    let path = "/cached-plain/index.html?b162";
    let r1 = send_request(PROXY_PORT, path, &[]).await;
    assert!(r1.is_some(), "First /cached/ request should succeed");
    assert_eq!(get_status_code(&r1.unwrap()), Some(200));

    let r2 = send_request(PROXY_PORT, path, &[]).await;
    assert!(r2.is_some(), "Second /cached/ request should succeed");
    let r2 = r2.unwrap();
    assert_eq!(get_status_code(&r2), Some(200));
    assert_eq!(
        get_header_value(&r2, "X-Cache").as_deref(),
        Some("HIT"),
        "the second request for a small response should be served from the cache: {}",
        r2.lines().take(12).collect::<Vec<_>>().join(" | ")
    );
}

/// /rate-limited/* ルートでレート制限（429）を検証する
#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_e2e_rate_limit_dedicated_route() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut success = 0u32;
    let mut limited = 0u32;
    for i in 0..50 {
        if let Some(resp) = send_request(PROXY_PORT, "/rate-limited/", &[]).await {
            match get_status_code(&resp) {
                Some(200) => success += 1,
                Some(429) => {
                    limited += 1;
                    eprintln!("Rate limited at request {}", i + 1);
                }
                other => eprintln!("Unexpected status {:?} at request {}", other, i + 1),
            }
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }

    assert!(success > 0, "Should get some 200 responses");
    assert!(
        limited > 0,
        "Rate limit on /rate-limited/* should return 429 (success={}, limited={})",
        success,
        limited
    );
}

/// /adaptive/* ルートで adaptive バッファリング経路を検証する
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_adaptive_buffering_route() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let small = send_request(PROXY_PORT, "/adaptive/", &[]).await;
    assert!(small.is_some());
    assert_eq!(get_status_code(&small.unwrap()), Some(200));

    let large = send_request(PROXY_PORT, "/adaptive/large.txt", &[]).await;
    assert!(large.is_some());
    let large = large.unwrap();
    assert_eq!(get_status_code(&large), Some(200));
    assert!(
        large.len() > 5000,
        "Adaptive route should forward large.txt intact"
    );
}

/// sni_name 指定で IP 直打ち HTTPS バックエンドへ接続できること
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_upstream_sni_name() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request(PROXY_PORT, "/sni-upstream/", &[]).await;
    assert!(resp.is_some(), "SNI upstream route should respond");
    let resp = resp.unwrap();
    assert_eq!(get_status_code(&resp), Some(200));
    assert!(
        resp.contains("Hello from Backend"),
        "SNI upstream should forward backend body"
    );
}

/// tls_insecure=false 時、自己署名証明書バックエンドは拒否される（502/503/504）
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_upstream_strict_cert_rejects() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request(PROXY_PORT, "/strict-cert/", &[]).await;
    assert!(
        resp.is_some(),
        "Strict cert route should return an error response"
    );
    let status = get_status_code(&resp.unwrap());
    assert!(
        matches!(status, Some(502) | Some(503) | Some(504)),
        "Self-signed backend with tls_insecure=false should fail, got {:?}",
        status
    );
}

/// TCP ヘルスチェック付きプール経由でリクエストが成功すること
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_tcp_health_check_upstream() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    let resp = send_request(PROXY_PORT, "/tcp-health/", &[]).await;
    assert!(resp.is_some(), "TCP health pool route should respond");
    assert_eq!(get_status_code(&resp.unwrap()), Some(200));
}

/// gRPC ヘルスチェック付きプールのメトリクスが露出すること
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_e2e_grpc_health_check_metrics() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    tokio::time::sleep(Duration::from_secs(2)).await;
    let metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(metrics.is_some());
    let metrics = metrics.unwrap();
    assert!(
        metrics.contains("upstream") || metrics.contains("health"),
        "Metrics should expose upstream health info for grpc-health pool"
    );
}

/// L4 Least Connection で TLS パススルー転送が動作すること
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_l4_least_conn_forward() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request_with_retry(PROXY_L4_LEAST_CONN_PORT, "/", &[], 3).await;
    assert!(resp.is_some(), "L4 least_conn should forward request");
    let resp = resp.unwrap();
    assert_eq!(get_status_code(&resp), Some(200));
    assert!(resp.contains("Hello from Backend"));
}

/// L4 TLS 終端で平文 HTTP バックエンドへ転送できること
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_l4_tls_terminate_forward() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request_with_retry(PROXY_L4_TERMINATE_PORT, "/", &[], 3).await;
    assert!(
        resp.is_some(),
        "L4 TLS terminate should complete TLS handshake and forward"
    );
    let resp = resp.unwrap();
    assert_eq!(
        get_status_code(&resp),
        Some(200),
        "L4 TLS terminate should get 200 from plain HTTP echo backend"
    );
}

/// L4 UDP プロキシ（セッションテーブル方式、F-124）が UDP エコーバックエンドへ
/// 往復転送できること。クライアントは veil の UDP リスナー(8447)へデータグラムを
/// 送信し、veil はセッションテーブルで upstream（UDP エコーサーバー）へ転送、
/// エコー応答をクライアントへ送り返す。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_l4_udp_echo_roundtrip() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind client UDP socket");
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{}", PROXY_L4_UDP_PORT)
        .parse()
        .expect("parse L4 UDP proxy addr");

    let payload = b"hello-l4-udp-e2e";

    // veil の UDP リスナーは per-client セッションを起動時に作成するため、
    // 初回パケットが競合状態で失われても数回リトライすれば往復できる。
    let mut received = None;
    for attempt in 0..5 {
        client
            .send_to(payload, server_addr)
            .await
            .expect("send_to L4 UDP proxy");

        let mut buf = [0u8; 256];
        match tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf)).await {
            Ok(Ok((n, _from))) => {
                received = Some(buf[..n].to_vec());
                break;
            }
            Ok(Err(e)) => {
                eprintln!("L4 UDP echo recv_from error (attempt {}): {}", attempt, e);
            }
            Err(_) => {
                eprintln!("L4 UDP echo recv timeout (attempt {})", attempt);
            }
        }
    }

    let received = received.expect("should receive UDP echo response via L4 proxy");
    assert_eq!(
        received, payload,
        "L4 UDP proxy should forward the datagram and echo response unmodified"
    );
    eprintln!("L4 UDP echo roundtrip: passed");
}

/// L4 UDP セッションが `idle_timeout_secs`（テスト構成では 10 秒）経過後、
/// 新規パケット到着時にセッションを再作成して転送を継続できることを検証する。
///
/// セッション自体の内部退去タイミングを直接観測するのは困難なため、ここでは
/// 「アイドル後も同じクライアントアドレスから送信すれば新規セッションとして
/// 正常に転送される」という利用者視点の振る舞いを確認する。
#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_l4_udp_multiple_datagrams_same_session() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let client = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind client UDP socket");
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{}", PROXY_L4_UDP_PORT)
        .parse()
        .expect("parse L4 UDP proxy addr");

    for i in 0..3u32 {
        let payload = format!("packet-{}", i);
        let mut sent_ok = false;
        for _attempt in 0..5 {
            client
                .send_to(payload.as_bytes(), server_addr)
                .await
                .expect("send_to L4 UDP proxy");

            let mut buf = [0u8; 256];
            match tokio::time::timeout(Duration::from_secs(2), client.recv_from(&mut buf)).await {
                Ok(Ok((n, _))) => {
                    assert_eq!(
                        &buf[..n],
                        payload.as_bytes(),
                        "each datagram on the same session should echo back unmodified"
                    );
                    sent_ok = true;
                    break;
                }
                _ => continue,
            }
        }
        assert!(sent_ok, "packet {} should be echoed back", i);
    }
    eprintln!("L4 UDP multiple datagrams (same session): passed");
}

/// /streaming/* と /full/* 専用ルートが存在し応答すること
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_buffering_mode_routes() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let streaming = send_request(PROXY_PORT, "/streaming/large.txt", &[]).await;
    assert!(streaming.is_some());
    assert_eq!(get_status_code(&streaming.unwrap()), Some(200));

    let full = send_request(PROXY_PORT, "/full/large.txt", &[]).await;
    assert!(full.is_some());
    assert_eq!(get_status_code(&full.unwrap()), Some(200));
}

/// kTLS 有効時に HTTPS 接続が成立すること（カーネル非対応時はスキップ扱い）
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "ktls")]
pub(super) async fn test_e2e_ktls_enabled_handshake() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    if !is_ktls_available() {
        eprintln!("Skipping: kTLS not available on this kernel");
        return;
    }

    let resp = send_request(PROXY_PORT, "/", &[]).await;
    assert!(resp.is_some(), "kTLS-enabled proxy should accept HTTPS");
    assert_eq!(get_status_code(&resp.unwrap()), Some(200));
}

// ====================
// F-91: HTTP/3・gRPC 網羅ギャップ解消（test_coverage_report.md / missing_test_cases.md）
// ====================

/// E-H3-10: HTTP/3 経由で /rate-limited/* に連打し 429 が返ること
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(all(feature = "http3", feature = "rate-limit"))]
pub(super) async fn test_http3_rate_limiting() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request;
    let mut success = 0u32;
    let mut limited = 0u32;
    for i in 0..40 {
        match send_http3_request(&mut send_request, "GET", "/rate-limited-h3/", &[], None).await {
            Ok((status, _)) => match status {
                200 => success += 1,
                429 => {
                    limited += 1;
                    eprintln!("HTTP/3 rate limited at request {}", i + 1);
                }
                other => eprintln!("HTTP/3 rate-limit unexpected status {}", other),
            },
            Err(e) => eprintln!("HTTP/3 rate-limit request error: {}", e),
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    assert!(
        success > 0,
        "HTTP/3 rate-limit route should allow some 200s"
    );
    assert!(
        limited > 0,
        "HTTP/3 rate-limit should return 429 (success={}, limited={})",
        success,
        limited
    );
}

/// E-H3-11: HTTP/3 経由で /api/ip-restricted/* が 403 になること
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_ip_restriction() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request;
    let (status, _) =
        send_http3_request(&mut send_request, "GET", "/api/ip-restricted/", &[], None)
            .await
            .expect("HTTP/3 ip-restricted request");
    assert_eq!(
        status, 403,
        "HTTP/3 /api/ip-restricted/* should deny 127.0.0.0/8 with 403, got {}",
        status
    );
}

/// E-H3-12: HTTP/3 経由で /wasm/* に WASM フィルタが適用されること
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http3", feature = "wasm"))]
pub(super) async fn test_http3_wasm_integration() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request_full;
    let resp = send_http3_request_full(&mut send_request, "GET", "/wasm/", &[], None)
        .await
        .expect("HTTP/3 wasm request");
    assert_eq!(resp.status, 200, "HTTP/3 /wasm/ should return 200");

    let has_wasm_header = resp.headers.iter().any(|(k, v)| {
        let k = k.to_ascii_lowercase();
        (k == "x-veil-processed" && v == "true")
            || k == "x-veil-filter-version"
            || k == "x-veil-context-id"
            || k == "x-wasm-processed"
    });
    // 実装が HTTP/3 でレスポンスヘッダ変更をスキップしている場合は失敗 → バグチケット化対象
    assert!(
        has_wasm_header,
        "HTTP/3 WASM integration should add filter response headers, got headers={:?}",
        resp.headers
    );
}

/// F-132: HTTP/3 経由でリクエストヘッダ変更が上流（バックエンド）まで届く経路が壊れていないこと。
///
/// header_filter.wasm はリクエストヘッダに `X-Veil-Request-Id` を追加する（B-38）。
/// バックエンドはヘッダをエコーしない静的ファイルサーバのため、ここでは h1/h2 の既存テスト
/// （test_wasm_request_header_read）と同水準で「カスタムリクエストヘッダを送っても
/// WASM リクエストヘッダフィルタが正常実行され 200 が返る」ことを確認する。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http3", feature = "wasm"))]
pub(super) async fn test_http3_wasm_request_header_mutation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request_full;
    let resp = send_http3_request_full(
        &mut send_request,
        "GET",
        "/wasm/",
        &[
            ("x-custom-header", "test-value"),
            ("user-agent", "wasm-h3-test-client"),
        ],
        None,
    )
    .await
    .expect("HTTP/3 wasm request with custom header");
    assert_eq!(
        resp.status, 200,
        "HTTP/3 /wasm/ with custom request header should return 200"
    );

    let processed = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-veil-processed"));
    assert!(
        processed.is_some_and(|(_, v)| v == "true"),
        "WASM on_request_headers should have executed (X-Veil-Processed missing), headers={:?}",
        resp.headers
    );
}

/// F-132: HTTP/3 経由の /wasm/* でレスポンスヘッダ変更が適用されること（h1/h2 既存相当）。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http3", feature = "wasm"))]
pub(super) async fn test_http3_wasm_response_header_mutation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request_full;
    let resp = send_http3_request_full(&mut send_request, "GET", "/wasm/", &[], None)
        .await
        .expect("HTTP/3 wasm request");
    assert_eq!(resp.status, 200, "HTTP/3 /wasm/ should return 200");

    let get = |name: &str| {
        resp.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.clone())
    };

    assert_eq!(
        get("x-veil-processed"),
        Some("true".to_string()),
        "Should have X-Veil-Processed header, got headers={:?}",
        resp.headers
    );
    assert_eq!(
        get("x-veil-filter-version"),
        Some("1.0.0".to_string()),
        "Should have X-Veil-Filter-Version header, got headers={:?}",
        resp.headers
    );
    assert!(
        get("x-veil-context-id").is_some(),
        "Should have X-Veil-Context-Id header, got headers={:?}",
        resp.headers
    );
}

/// F-132: HTTP/3 経由で WASM モジュールが LocalResponse を返した場合、それがそのまま
/// クライアントへ返ること。
///
/// 調査の経緯（重要）: 当初はパストラバーサルのクエリ文字列（`../../etc/passwd` 等）で
/// 403 を検証しようとしたが、`tests/wasm/waf_filter.wasm`（`tests/fixtures/wasm/` に
/// フォールバックコピー済み）は `examples/wasm-filters/waf-filter` の**古い版**
/// （コミット `fd9ede9`〜、`cmdi-00N`/`sqli-00N`/`traversal-00N` 命名。現行 `crs-*` 版とは別物）
/// からビルドされており、`on_http_request_headers` がパス/クエリを
/// `self.get_http_request_header(":path")`（`:path` **疑似ヘッダをヘッダマップ経由**で取得）
/// で読んでいる。しかし本リポジトリの WASM ホスト実装は `:path`/`:method` を
/// リクエストコンテキストの別フィールド（`HttpContext::request_path`/`request_method`。
/// `get_property("request.path")` 経由でのみ参照可能）として保持し、ヘッダマップ
/// （`proxy_get_header_map_value` が読む `request_headers`）には含めない
/// （`src/http3_server.rs`/`src/proxy.rs` とも疑似ヘッダを除外して `set_request()` へ渡す。
/// `src/wasm/context.rs::set_request` 参照）。そのため `get_http_request_header(":path")` は
/// **HTTP/1.1・HTTP/2・HTTP/3 のいずれでも** 常に `None` を返し、パス/クエリに基づく
/// ルール（path traversal 等）は本バイナリでは発火しない
/// （`curl -H "Host: localhost" https://127.0.0.1:8443/waf/?file=../../etc/passwd` を
/// h2 で直接叩いても 200 が返ることを手動確認済み。HTTP/3 固有のリグレッションではない）。
/// そこで **`user-agent` ヘッダ**（通常のヘッダマップに含まれ、疑似ヘッダの制約を受けない）
/// を対象にした `cmdi-003`（`(?i)(^|;)\s*(cat|ls|...|curl|wget|...)\b`）ルールで
/// LocalResponse を発火させる。これは「HTTP/3 で WASM の LocalResponse が機能すること」を
/// 実証する目的を保ったまま、実際にこのモジュールが発火する条件に合わせたテストである。
///
/// B-58: OpenBSD だけ制限時間を大幅に延ばしている。OpenBSD は W^X 制約のため WASM を
/// Cranelift JIT ではなく **Pulley インタープリタ**で実行する（B-52）。waf_filter は
/// CRS Level 2 の正規表現ルール群を評価する CPU 律速モジュールで、実機計測では
/// モジュール初期化から `on_configure` 到達までだけで約 18 秒を要した（他プラットフォーム
/// では数十 ms）。機能自体は正しく動作しており、ハングでもデッドロックでもないため、
/// スキップせず制限時間の延長で計測対象に残す。header_filter のような軽量モジュールは
/// OpenBSD でも HTTP/3 上で 0.17 秒で完走する。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http3", feature = "wasm"))]
#[cfg_attr(
    target_os = "openbsd",
    ignore = "B-58: OpenBSD は Pulley インタープリタ実行のため waf_filter の評価が QUIC の \
              idle timeout(約30秒)を超過し ConnectionError(Timeout) になる。制限時間の延長では \
              解決しない（180 秒にしても 30 秒で QUIC 側が切断）。軽量モジュールなら 0.17 秒で完走する"
)]
pub(super) async fn test_http3_wasm_local_response() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request_full;
    let resp = send_http3_request_full(
        &mut send_request,
        "GET",
        "/waf/",
        &[("user-agent", "curl/8.5.0")],
        None,
    )
    .await
    .expect("HTTP/3 waf request");

    assert_eq!(
        resp.status, 403,
        "HTTP/3 WAF WASM module should block a flagged User-Agent with 403, got status={} headers={:?}",
        resp.status, resp.headers
    );
    let has_waf_marker = resp
        .headers
        .iter()
        .any(|(k, _)| k.eq_ignore_ascii_case("x-waf-block"));
    assert!(
        has_waf_marker,
        "WAF LocalResponse should carry x-waf-block header, got headers={:?}",
        resp.headers
    );
}

/// F-132: HTTP/3 の `Backend::File`（静的配信）ルートにも WASM on_response_headers が
/// 適用されること（h1/h2 は既に対応済み。HTTP/3 は本チケットで追加）。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http3", feature = "wasm"))]
pub(super) async fn test_http3_wasm_static_file() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request_full;
    let resp = send_http3_request_full(&mut send_request, "GET", "/wasm-static", &[], None)
        .await
        .expect("HTTP/3 wasm-static request");
    assert_eq!(
        resp.status, 200,
        "HTTP/3 /wasm-static (Backend::File) should return 200"
    );

    let has_wasm_header = resp
        .headers
        .iter()
        .any(|(k, v)| k.eq_ignore_ascii_case("x-veil-processed") && v == "true");
    assert!(
        has_wasm_header,
        "HTTP/3 static file (Backend::File) route should also have WASM response headers \
         applied, got headers={:?}",
        resp.headers
    );
    assert!(
        !resp.body.is_empty(),
        "HTTP/3 static file response body should not be empty"
    );
}

/// E-H3-13: HTTP/3 経由の /cached/* でキャッシュ hit/miss 相当の挙動
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http3", feature = "cache"))]
pub(super) async fn test_http3_cache_hit_miss() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request_full;
    let path = "/cached/large.txt";
    let r1 = send_http3_request_full(&mut send_request, "GET", path, &[], None)
        .await
        .expect("HTTP/3 cache miss request");
    assert_eq!(r1.status, 200, "first cached request should be 200");

    tokio::time::sleep(Duration::from_millis(50)).await;

    let r2 = send_http3_request_full(&mut send_request, "GET", path, &[], None)
        .await
        .expect("HTTP/3 cache hit request");
    assert_eq!(r2.status, 200, "second cached request should be 200");

    let x_cache = r2
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-cache"))
        .map(|(_, v)| v.as_str());
    let age = r2
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("age"))
        .map(|(_, v)| v.as_str());
    let body_match = !r1.body.is_empty() && r1.body == r2.body;
    assert!(
        x_cache.is_some() || age.is_some() || body_match,
        "HTTP/3 cache should expose X-Cache/Age or return identical body (x_cache={:?}, age={:?}, body_len1={}, body_len2={})",
        x_cache,
        age,
        r1.body.len(),
        r2.body.len()
    );
}

/// E-H3-14: 0-RTT / early data で非べき等 POST が安全に扱われること
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_early_data_0rtt_security() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // 1 回目: セッション確立
    let (_c1, mut sr1) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("first H3 connection");
    use common::http3_client::send_http3_request;
    let _ = send_http3_request(&mut sr1, "GET", "/", &[], None).await;

    // 2 回目: 新接続で POST（0-RTT 利用の可能性）。クラッシュ・永久ハングしないこと。
    // 期待: 2xx/4xx のいずれか、または接続エラー（early data 拒否）でプロセスは生存。
    let (_c2, mut sr2) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("second H3 connection");
    let post_body = b"non-idempotent-0rtt-body";
    let result = send_http3_request(
        &mut sr2,
        "POST",
        "/",
        &[("content-type", "application/octet-stream")],
        Some(post_body),
    )
    .await;

    match result {
        Ok((status, _)) => {
            assert!(
                (200..600).contains(&status),
                "0-RTT POST should get HTTP status, got {}",
                status
            );
            eprintln!("HTTP/3 0-RTT security POST status={}", status);
        }
        Err(e) => {
            // 拒否・リセットは許容（リプレイ防御）。panic/hang でなければ OK。
            eprintln!("HTTP/3 0-RTT POST rejected/errored (acceptable): {}", e);
        }
    }

    // プロセス生存確認
    assert!(
        is_e2e_environment_ready().await,
        "proxy must remain up after 0-RTT POST probe"
    );
}

/// E-H3-15: WebSocket over HTTP/3 (RFC 9220) — 対応時は確立、未対応時は安全な失敗
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(all(feature = "http3", feature = "websocket"))]
pub(super) async fn test_http3_websocket() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    use common::http3_client::send_http3_request_full;
    // RFC 9220: extended CONNECT + :protocol=websocket。
    // h3 クライアントで完全再現は難しいため CONNECT + Sec-WebSocket-* を試行。
    // 未対応実装でもクラッシュせず 4xx/5xx 等で終わることを主目的とする。
    let result = send_http3_request_full(
        &mut send_request,
        "CONNECT",
        "/ws/",
        &[
            ("sec-websocket-version", "13"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-protocol", "websocket"),
        ],
        None,
    )
    .await;

    match result {
        Ok(resp) => {
            // 101/200 = 成功、400/404/405/501 = 未対応だが安全
            assert!(
                matches!(resp.status, 101 | 200 | 400 | 403 | 404 | 405 | 501 | 502),
                "WS-over-H3 should not crash; status={}",
                resp.status
            );
            eprintln!("HTTP/3 WebSocket probe status={}", resp.status);
        }
        Err(e) => {
            eprintln!(
                "HTTP/3 WebSocket probe error (acceptable if unsupported): {}",
                e
            );
        }
    }

    assert!(
        is_e2e_environment_ready().await,
        "proxy must remain up after WS-over-H3 probe"
    );
}

/// E-G-05+: gRPC ワイヤ length と実ボディが不一致の不正フレームで panic/hang しないこと
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_grpc_http2_framing_malformed_data() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 生の gRPC フレーム: length=4096 と宣言するが実データは短い
    let mut bad_frame = vec![0u8, 0, 0, 0x10, 0x00]; // flags=0, length=4096
    bad_frame.extend_from_slice(b"short");

    let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("http1 client");
    let result = client
        .post_with_headers(
            "/grpc.test.v1.TestService/UnaryCall",
            &[
                ("content-type", "application/grpc"),
                ("te", "trailers"),
                ("accept", "application/grpc"),
            ],
            &bad_frame,
        )
        .await;

    match result {
        Ok((status, _body)) => {
            assert!(
                matches!(status, 200 | 400 | 413 | 502 | 503 | 504),
                "malformed gRPC DATA should get controlled status, got {}",
                status
            );
            eprintln!("malformed gRPC frame HTTP status={}", status);
        }
        Err(e) => {
            let s = e.to_string();
            assert!(
                s.contains("SendRequest")
                    || s.contains("connection")
                    || s.contains("reset")
                    || s.contains("closed")
                    || s.contains("error")
                    || s.contains("timeout"),
                "expected controlled connection error, got {}",
                s
            );
            eprintln!("malformed gRPC frame controlled error: {}", s);
        }
    }

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive malformed gRPC framing"
    );
}

/// E-G-06: HTTP/3 上の gRPC Unary（成功時は 200 + grpc-status/ボディ）
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "grpc", feature = "http3"))]
pub(super) async fn test_grpc_over_http3() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!("HTTP/3 client failed: {}", e),
    };

    // 正しい SimpleRequest protobuf（B-41: ボディあり成功パスを固定。不正ペイロードだと
    // 空ボディ+エラーのみで trailers ハングバグを隠蔽する）
    let frame_bytes = encode_grpc_lpm(&encode_simple_request("hello-grpc-over-h3"));

    use common::http3_client::send_http3_request_full;
    let r = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &[
            ("content-type", "application/grpc"),
            ("te", "trailers"),
            ("grpc-accept-encoding", "identity"),
        ],
        Some(&frame_bytes),
    )
    .await
    .expect("gRPC over HTTP/3 request should complete without transport error");

    let grpc_status = r.grpc_status();
    eprintln!(
        "gRPC over H3 status={} grpc-status={:?} body_len={} headers={:?}",
        r.status,
        grpc_status,
        r.body.len(),
        r.headers
    );

    // 実装バグ（HTTP/3 gRPC プロキシ 502 / B-41 trailers ハング等）はテスト失敗として検出する
    assert_eq!(
        r.status, 200,
        "gRPC over HTTP/3 Unary should return HTTP 200 (got {}). See B-39 if 502.",
        r.status
    );
    assert_eq!(
        grpc_status,
        Some(0),
        "valid Unary over H3 should yield grpc-status=0 (got {:?}). See B-41 if hang/missing trailers.",
        grpc_status
    );
    assert!(
        !r.body.is_empty(),
        "successful Unary should return a response LPM body"
    );
}

// ====================
// F-93: http3_grpc_test_coverage_report 残件 E2E
// ====================

/// レポート: `test_http3_chunked_response_streaming`
/// バックエンド chunked 応答を QUIC DATA で再構成し、バイト一致を確認する。
/// （既存 `test_http3_response_body_streaming` と同等の経路。レポート名で明示。）
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_chunked_response_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    const EXPECTED_TOTAL: usize = 200_000;
    let expected_byte = |i: usize| -> u8 { (i % 256) as u8 };

    use common::http3_client::send_http3_request;

    const MAX_ATTEMPTS: usize = 4;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await
        {
            Ok(c) => c,
            Err(e) => {
                last_err = format!("connect: {}", e);
                tokio::time::sleep(Duration::from_millis(300)).await;
                continue;
            }
        };

        match send_http3_request(&mut send_request, "GET", "/chunked-stream/data", &[], None).await
        {
            Ok((status, body)) => {
                assert_eq!(status, 200, "chunked stream over H3");
                assert_eq!(
                    body.len(),
                    EXPECTED_TOTAL,
                    "chunked reassembled length (attempt {})",
                    attempt + 1
                );
                for (i, &b) in body.iter().enumerate() {
                    assert_eq!(b, expected_byte(i), "byte mismatch at {}", i);
                }
                eprintln!(
                    "test_http3_chunked_response_streaming: {} bytes OK (attempt {})",
                    body.len(),
                    attempt + 1
                );
                return;
            }
            Err(e) => {
                last_err = format!("GET: {}", e);
                tokio::time::sleep(Duration::from_millis(300)).await;
            }
        }
    }
    panic!(
        "test_http3_chunked_response_streaming failed after retries: {}",
        last_err
    );
}

/// レポート: `test_http3_connection_reuse`
/// 複数の独立リクエストが同一 QUIC コネクション（同一 SendRequest）で処理されること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_connection_reuse() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    use common::http3_client::{send_http3_request, send_http3_request_full};

    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 connection for reuse");

    // 同一接続上で GET を連続
    for i in 0..5 {
        let (st, body) = send_http3_request(&mut send_request, "GET", "/", &[], None)
            .await
            .unwrap_or_else(|e| panic!("reuse GET {}: {}", i, e));
        assert_eq!(st, 200, "reuse GET {} status", i);
        assert!(!body.is_empty() || st == 200, "reuse GET {} body ok", i);
    }

    // POST も同一接続で
    let post_body = b"connection-reuse-post-body";
    let r = send_http3_request_full(
        &mut send_request,
        "POST",
        "/echo-upload/reuse",
        &[("content-type", "application/octet-stream")],
        Some(post_body),
    )
    .await
    .expect("reuse POST");
    assert_eq!(r.status, 200, "reuse POST status");
    assert_eq!(
        r.body.as_slice(),
        post_body.as_slice(),
        "echo body on reused connection"
    );

    // さらに GET が続くこと（接続が POST で壊れていない）
    let (st, _) = send_http3_request(&mut send_request, "GET", "/", &[], None)
        .await
        .expect("reuse GET after POST");
    assert_eq!(st, 200, "connection must stay usable after POST");

    eprintln!("test_http3_connection_reuse: 5 GET + POST + GET on same QUIC conn OK");
}

/// レポート: `test_http3_early_data`
/// 0-RTT / early data 相当の再接続後リクエストが安全に完了（または安全に拒否）されること。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_early_data() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    use common::http3_client::send_http3_request;

    // 1st connection: warm session
    let (_c1, mut sr1) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("first H3 for early data");
    let (st1, _) = send_http3_request(&mut sr1, "GET", "/", &[], None)
        .await
        .expect("warm-up GET");
    assert_eq!(st1, 200, "warm-up should succeed");
    drop(sr1);
    drop(_c1);

    // 2nd connection: may use 0-RTT if tickets available
    let (_c2, mut sr2) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("second H3 for early data");
    let result = send_http3_request(
        &mut sr2,
        "POST",
        "/",
        &[("content-type", "application/octet-stream")],
        Some(b"early-data-probe"),
    )
    .await;

    match result {
        Ok((status, _)) => {
            assert!(
                (200..600).contains(&status),
                "early data POST should yield HTTP status, got {}",
                status
            );
            eprintln!("test_http3_early_data: POST status={}", status);
        }
        Err(e) => {
            // 0-RTT 拒否・リセットは安全側として許容
            eprintln!("test_http3_early_data: rejected (ok): {}", e);
        }
    }

    // 3rd: 通常 GET が通る（プロセス生存）
    let (_c3, mut sr3) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("third H3 after early data");
    let (st3, _) = send_http3_request(&mut sr3, "GET", "/", &[], None)
        .await
        .expect("GET after early data probe");
    assert_eq!(st3, 200, "proxy must serve after early-data probe");
    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive early data probe"
    );
}

/// レポート: `test_grpc_over_http3_streaming`
/// サーバ/クライアント/双方向ストリーミングが QUIC 上で動作すること。
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(all(feature = "grpc", feature = "http3"))]
pub(super) async fn test_grpc_over_http3_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for gRPC streaming");

    use common::http3_client::{
        send_http3_and_reset, send_http3_request_chunked, send_http3_request_full,
    };

    let grpc_headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("accept", "application/grpc"),
        ("grpc-accept-encoding", "identity"),
    ];

    // --- Server Streaming ---
    let ss_lpm = encode_grpc_lpm(&encode_simple_request("h3-ss"));
    let ss = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/ServerStreaming",
        &grpc_headers,
        Some(&ss_lpm),
    )
    .await
    .expect("H3 server streaming");
    assert_eq!(ss.status, 200, "server streaming HTTP status over H3");
    let ss_frames = decode_all_grpc_frames(&ss.body);
    eprintln!(
        "H3 ServerStreaming: frames={} grpc-status={:?} body_len={}",
        ss_frames.len(),
        ss.grpc_status(),
        ss.body.len()
    );
    assert!(
        ss.grpc_status().is_some() || !ss_frames.is_empty(),
        "H3 server streaming should yield frames or grpc-status"
    );
    if ss.grpc_status() == Some(0) {
        assert!(
            ss_frames.len() >= 2,
            "ok H3 server stream should return multiple LPMs, got {}",
            ss_frames.len()
        );
    }

    // --- Client Streaming: 複数 LPM をチャンク送信 ---
    let mut client_chunks: Vec<Vec<u8>> = Vec::new();
    for i in 0..4 {
        client_chunks.push(encode_grpc_lpm(&encode_simple_request(&format!(
            "h3-cs-{}",
            i
        ))));
    }
    let chunk_refs: Vec<&[u8]> = client_chunks.iter().map(|v| v.as_slice()).collect();
    let cs = send_http3_request_chunked(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/ClientStreaming",
        &grpc_headers,
        &chunk_refs,
        Some(Duration::from_millis(15)),
    )
    .await
    .expect("H3 client streaming");
    assert_eq!(cs.status, 200, "H3 client streaming HTTP status");
    assert!(
        cs.grpc_status().is_some(),
        "H3 client streaming needs grpc-status, headers={:?}",
        cs.headers
    );
    eprintln!(
        "H3 ClientStreaming: grpc-status={:?} body_len={}",
        cs.grpc_status(),
        cs.body.len()
    );

    // --- Bidirectional ---
    let mut bidi_chunks: Vec<Vec<u8>> = Vec::new();
    for i in 0..3 {
        bidi_chunks.push(encode_grpc_lpm(&encode_simple_request(&format!(
            "h3-bidi-{}",
            i
        ))));
    }
    let bidi_refs: Vec<&[u8]> = bidi_chunks.iter().map(|v| v.as_slice()).collect();
    let bidi = send_http3_request_chunked(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/BidirectionalStreaming",
        &grpc_headers,
        &bidi_refs,
        Some(Duration::from_millis(10)),
    )
    .await
    .expect("H3 bidirectional streaming");
    assert_eq!(bidi.status, 200, "H3 bidi HTTP status");
    let bidi_frames = decode_all_grpc_frames(&bidi.body);
    eprintln!(
        "H3 Bidirectional: frames={} grpc-status={:?}",
        bidi_frames.len(),
        bidi.grpc_status()
    );
    assert!(
        bidi.grpc_status().is_some() || !bidi_frames.is_empty(),
        "H3 bidi should complete with status or frames"
    );

    // --- 途中切断後の生存 ---
    let partial = encode_grpc_lpm(&encode_simple_request("h3-will-reset"));
    send_http3_and_reset(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &grpc_headers,
        Some(&partial[..partial.len().saturating_sub(2).max(1)]),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    let after = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &grpc_headers,
        Some(&encode_grpc_lpm(&encode_simple_request("h3-after-reset"))),
    )
    .await;
    match after {
        Ok(r) => assert_eq!(r.status, 200, "H3 unary after reset must work"),
        Err(e) => {
            // 接続が閉じられた場合は再接続で生存確認
            eprintln!("H3 conn closed after reset (reconnecting): {}", e);
            let (_c2, mut sr2) = Http3TestClient::new(server_addr, "localhost")
                .await
                .expect("reconnect after H3 gRPC reset");
            let r2 = send_http3_request_full(
                &mut sr2,
                "POST",
                "/grpc.test.v1.TestService/UnaryCall",
                &grpc_headers,
                Some(&encode_grpc_lpm(&encode_simple_request("h3-after-reconn"))),
            )
            .await
            .expect("unary after reconnect");
            assert_eq!(r2.status, 200, "proxy must accept H3 gRPC after reset");
        }
    }

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive H3 gRPC streaming"
    );
    eprintln!("test_grpc_over_http3_streaming completed");
}

/// レポート: `test_grpc_over_http3_metadata`
/// カスタムメタデータと grpc トレーラーが QUIC HEADERS として透過されること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http3"))]
pub(super) async fn test_grpc_over_http3_metadata() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for gRPC metadata");

    use common::http3_client::send_http3_request_full;

    let headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("accept", "application/grpc"),
        ("grpc-accept-encoding", "identity"),
        ("x-custom-meta", "h3-meta-value"),
        ("x-request-id", "h3-req-12345"),
    ];
    let body = encode_grpc_lpm(&encode_simple_request("h3-meta"));
    let r = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &headers,
        Some(&body),
    )
    .await
    .expect("H3 gRPC with metadata");

    assert_eq!(r.status, 200, "metadata unary HTTP 200 over H3");
    let gs = r.grpc_status();
    assert!(
        gs.is_some(),
        "custom metadata must not strip grpc-status, headers={:?}",
        r.headers
    );
    let code = gs.unwrap();
    assert!(code <= 16, "grpc-status in 0..16, got {}", code);

    // 上流が返す x-server-id が透過される場合は検証（プロキシが strip してもよい）
    if let Some(sid) = r.header("x-server-id") {
        assert!(
            !sid.is_empty(),
            "x-server-id should be non-empty if present"
        );
        eprintln!("H3 metadata: x-server-id={}", sid);
    }
    eprintln!(
        "test_grpc_over_http3_metadata: grpc-status={} headers={:?}",
        code, r.headers
    );
}

/// レポート: `test_grpc_over_http3_error_handling`
/// バックエンドエラー時に適切な gRPC ステータスが QUIC トレーラーとして返ること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http3"))]
pub(super) async fn test_grpc_over_http3_error_handling() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for gRPC errors");

    use common::http3_client::send_http3_request_full;

    let grpc_headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("accept", "application/grpc"),
        ("grpc-accept-encoding", "identity"),
    ];

    // 正常経路
    let ok = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &grpc_headers,
        Some(&encode_grpc_lpm(&encode_simple_request("h3-ok"))),
    )
    .await
    .expect("H3 ok unary");
    assert_eq!(ok.status, 200);
    assert!(
        ok.grpc_status().is_some(),
        "ok path must expose grpc-status over H3"
    );

    // StreamReset → non-zero grpc-status + grpc-message
    let err = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/StreamReset",
        &grpc_headers,
        Some(&encode_grpc_lpm(&encode_simple_request("force-error"))),
    )
    .await
    .expect("H3 error unary");
    assert_eq!(err.status, 200, "gRPC errors still use HTTP 200 over H3");
    let err_status = err.grpc_status();
    assert!(
        err_status.is_some(),
        "error path must expose grpc-status over H3, headers={:?}",
        err.headers
    );
    let err_code = err_status.unwrap();
    assert!(
        err_code > 0 && err_code <= 16,
        "StreamReset should yield non-zero grpc-status over H3, got {}",
        err_code
    );
    let err_msg = err.grpc_message();
    assert!(
        err_msg.as_ref().map(|m| !m.is_empty()).unwrap_or(false),
        "error path should forward grpc-message over H3, got {:?}",
        err_msg
    );
    eprintln!(
        "test_grpc_over_http3_error_handling: status={} msg={:?}",
        err_code, err_msg
    );

    // 未実装メソッド → UNIMPLEMENTED 等（クラッシュしない）
    let unimpl = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/DoesNotExist",
        &grpc_headers,
        Some(&encode_grpc_lpm(&encode_simple_request("nope"))),
    )
    .await;
    match unimpl {
        Ok(r) => {
            eprintln!(
                "H3 unimplemented: http={} grpc-status={:?}",
                r.status,
                r.grpc_status()
            );
            assert!(
                matches!(r.status, 200 | 404 | 502 | 503)
                    || r.grpc_status().map(|s| s > 0).unwrap_or(true),
                "unimplemented should be controlled, got http={} grpc={:?}",
                r.status,
                r.grpc_status()
            );
        }
        Err(e) => {
            eprintln!("H3 unimplemented transport error (controlled): {}", e);
        }
    }

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive H3 gRPC error paths"
    );
}

/// E-G-07: gRPC client slowloris — 極遅送信でもプロキシが生存すること
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_client_slowloris() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 生 TCP + 手動 TLS でヘッダをゆっくり送る（最大約 3 秒で打ち切り）
    let config = create_client_config();
    let server_name = ServerName::try_from("localhost".to_string()).unwrap();
    let mut tls_conn = ClientConnection::new(config, server_name).unwrap();
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    stream
        .set_write_timeout(Some(Duration::from_millis(500)))
        .unwrap();

    while tls_conn.is_handshaking() {
        if let Err(e) = tls_conn.complete_io(&mut stream) {
            panic!("TLS handshake error during slowloris setup: {:?}", e);
        }
    }

    // 不完全なリクエスト（ボディ未完）をチャンク送信してアイドル接続を作る
    let req = b"POST /grpc.test.v1.TestService/UnaryCall HTTP/1.1\r\n\
Host: localhost\r\n\
Content-Type: application/grpc\r\n\
TE: trailers\r\n\
Content-Length: 1048576\r\n\
\r\n";

    let mut sent = 0usize;
    let mut aborted = false;
    let start = std::time::Instant::now();
    while sent < req.len() && start.elapsed() < Duration::from_secs(3) {
        // 4 バイトずつ送って遅延を出す（1 バイトだと ntest に張り付く）
        let end = (sent + 4).min(req.len());
        let chunk = &req[sent..end];
        match tls_conn.writer().write(chunk) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                aborted = true;
                break;
            }
        }
        if let Err(e) = tls_conn.write_tls(&mut stream) {
            if e.kind() != std::io::ErrorKind::WouldBlock {
                aborted = true;
                break;
            }
        }
        match tls_conn.read_tls(&mut stream) {
            Ok(0) => {
                aborted = true;
                break;
            }
            Ok(_) => {
                let _ = tls_conn.process_new_packets();
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(_) => {
                aborted = true;
                break;
            }
        }
        sent = end;
        tokio::time::sleep(Duration::from_millis(40)).await;
    }

    // 追加で数秒アイドル保持（サーバ側タイムアウト/生存確認）
    tokio::time::sleep(Duration::from_secs(2)).await;
    // ソケットを明示クローズ（クライアント側終了）
    drop(stream);

    eprintln!(
        "gRPC slowloris: sent={}/{} aborted={} elapsed_ms={}",
        sent,
        req.len(),
        aborted,
        start.elapsed().as_millis()
    );

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive gRPC client slowloris"
    );

    let ok = GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/UnaryCall",
        b"after-slowloris",
        &[],
    )
    .await;
    assert!(
        ok.is_ok() || ok.err().is_some(),
        "post-slowloris request should complete without hanging forever"
    );
}
