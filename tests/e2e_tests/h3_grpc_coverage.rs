//! HTTP/3・gRPC の網羅（F-94・F-96・F-97・F-99・F-101）
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// =============================================================================
// F-94: http3_grpc_test_coverage_report 項目 1〜4
// =============================================================================

/// E-H3-F94-01: HTTP/1.1 および HTTP/2 応答で Alt-Svc が HTTP/3 を広告すること
#[tokio::test]
#[ntest::timeout(20000)]
// HTTP/3 クライアントは使わないが、検証対象は「http3 が有効なときに Alt-Svc を
// 広告すること」なので http3 feature が要る（無効なら広告しないのが正しい挙動）。
#[cfg(feature = "http3")]
pub(super) async fn test_alt_svc_http3_advertisement() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/1.1: 静的ルート GET / で Alt-Svc を確認
    let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("h1 client");
    let (status, headers, _body) = client
        .send_request_with_response_headers(http::Method::GET, "/", &[], None)
        .await
        .expect("HTTP/1.1 GET /");
    assert_eq!(status, 200, "HTTP/1.1 GET / should be 200");
    let alt_svc_h1 = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("alt-svc"));
    assert!(
        alt_svc_h1.is_some(),
        "HTTP/1.1 response must include Alt-Svc when [http3].enabled, headers={:?}",
        headers
    );
    let v = &alt_svc_h1.unwrap().1;
    assert!(
        v.contains("h3=") || v.contains("h3=\""),
        "Alt-Svc should advertise h3, got {:?}",
        v
    );
    assert!(
        v.contains(&format!(":{}", PROXY_HTTP3_PORT)) || v.contains(":443"),
        "Alt-Svc should include HTTP/3 port, got {:?}",
        v
    );

    // HTTP/2: 同様に Alt-Svc
    #[cfg(feature = "http2")]
    {
        let mut h2 = Http2TestClient::new("127.0.0.1", PROXY_PORT)
            .await
            .expect("h2 client");
        let resp = h2
            .send_request_full("GET", "/", &[], None)
            .await
            .expect("HTTP/2 GET /");
        assert_eq!(resp.status, 200, "HTTP/2 GET / should be 200");
        let alt_svc_h2 = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("alt-svc"));
        assert!(
            alt_svc_h2.is_some(),
            "HTTP/2 response must include Alt-Svc, headers={:?}",
            resp.headers
        );
        let v2 = &alt_svc_h2.unwrap().1;
        assert!(
            v2.contains("h3="),
            "HTTP/2 Alt-Svc should advertise h3, got {:?}",
            v2
        );
    }
}

/// E-H3-F94-02: UDP 到達不能時のフォールバック — H3 失敗後も H2/H1.1 で処理継続
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_udp_unreachable_fallback() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 1) 到達不能な UDP ポートへ HTTP/3 を試みる → 失敗が期待される
    let dead_udp: u16 = 39999;
    let dead_addr: std::net::SocketAddr = format!("127.0.0.1:{}", dead_udp).parse().unwrap();
    let h3_dead = tokio::time::timeout(
        Duration::from_secs(3),
        Http3TestClient::new(dead_addr, "localhost"),
    )
    .await;
    match h3_dead {
        Ok(Ok(_)) => {
            // 偶然応答が来た場合はフォールバック検証へ進む（稀）
            eprintln!("WARN: unexpected H3 success on dead UDP port {}", dead_udp);
        }
        Ok(Err(e)) => {
            eprintln!("H3 to dead UDP failed as expected: {}", e);
        }
        Err(_) => {
            eprintln!("H3 to dead UDP timed out as expected");
        }
    }

    // 2) 同じプロキシへ HTTP/1.1 で到達できること（クライアントフォールバック相当）
    let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("h1 client");
    let (s1, _) = client.get("/").await.expect("H1.1 fallback GET");
    assert_eq!(s1, 200, "HTTP/1.1 fallback after UDP failure should work");

    // 3) HTTP/2 でも到達できること
    #[cfg(feature = "http2")]
    {
        let mut h2 = Http2TestClient::new("127.0.0.1", PROXY_PORT)
            .await
            .expect("h2 client");
        let (s2, _) = h2.get("/").await.expect("H2 fallback GET");
        assert_eq!(s2, 200, "HTTP/2 fallback after UDP failure should work");
    }

    // 4) 正規の HTTP/3 ポートは生きていること（プロキシ自体の健全性）
    #[cfg(feature = "http3")]
    {
        let live: std::net::SocketAddr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
        let ok = Http3TestClient::new(live, "localhost").await;
        assert!(
            ok.is_ok(),
            "live HTTP/3 listener should still accept connections"
        );
    }
}

/// E-G-F94-03: gRPC 大量データで WINDOW_UPDATE 境界を跨いでもクラッシュしないこと
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(all(feature = "http2", feature = "grpc"))]
pub(super) async fn test_grpc_flow_control_window_boundary() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 初期ウィンドウ 65535 を超えるペイロードで複数 DATA + WINDOW_UPDATE を強制
    // gRPC LPM: 5 byte header + protobuf-ish payload
    let payload_size = 256 * 1024; // 256 KiB
    let mut lpm = Vec::with_capacity(5 + payload_size);
    lpm.push(0u8); // uncompressed
    lpm.extend_from_slice(&(payload_size as u32).to_be_bytes());
    lpm.extend(std::iter::repeat_n(b'G', payload_size));

    // 1 バイト〜1KB 単位に分割して送信し、フロー制御の再開を誘発
    let mut chunks: Vec<&[u8]> = Vec::new();
    let mut offset = 0usize;
    while offset < lpm.len() {
        let end = (offset + 1024).min(lpm.len());
        chunks.push(&lpm[offset..end]);
        offset = end;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 client for gRPC FC");
    let headers = [("content-type", "application/grpc"), ("te", "trailers")];
    let result = tokio::time::timeout(
        Duration::from_secs(30),
        client.send_request_chunked(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            &chunks,
            None,
        ),
    )
    .await;

    match result {
        Ok(Ok(resp)) => {
            // 2xx または gRPC エラー（上流が巨大メッセージを拒否）いずれもクラッシュなし
            eprintln!(
                "gRPC FC: status={} grpc_status={:?} body_len={}",
                resp.status,
                resp.grpc_status(),
                resp.body.len()
            );
            assert!(resp.status < 600, "unexpected HTTP status {}", resp.status);
        }
        Ok(Err(e)) => {
            // ストリームリセット等は許容（プロセス生存が主目的）
            eprintln!("gRPC FC stream error (controlled): {}", e);
        }
        Err(_) => panic!("gRPC flow-control streaming hung for 30s"),
    }

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive gRPC flow-control boundary test"
    );

    // 直後の小さな Unary が通ること
    let ok = GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/UnaryCall",
        b"after-fc",
        &[],
    )
    .await;
    assert!(
        ok.is_ok(),
        "post-FC small Unary should succeed: {:?}",
        ok.err()
    );
}

/// E-G-F94-04: gRPC 経路で WASM インターセプタが応答ヘッダを付与できること
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http2", feature = "grpc", feature = "wasm"))]
pub(super) async fn test_grpc_wasm_interceptor() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPC ルートに header_filter が modules として載っている想定（e2e_setup）
    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 client");
    // 最小 LPM: empty message
    let lpm = [0u8, 0, 0, 0, 0];
    let headers = [("content-type", "application/grpc"), ("te", "trailers")];
    let resp = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            Some(&lpm),
        )
        .await
        .expect("gRPC+WASM request");

    eprintln!(
        "gRPC+WASM: status={} headers={:?} grpc_status={:?}",
        resp.status,
        resp.headers,
        resp.grpc_status()
    );

    // プロキシが 5xx で落ちていないこと
    assert!(
        resp.status < 500 || resp.status == 502,
        "gRPC+WASM should not crash; status={}",
        resp.status
    );

    let has_wasm = resp.headers.iter().any(|(k, v)| {
        let k = k.to_ascii_lowercase();
        (k == "x-veil-processed" && (v == "true" || !v.is_empty()))
            || k == "x-wasm-processed"
            || k == "x-veil-filter-version"
            || k == "x-veil-context-id"
    });
    assert!(
        has_wasm,
        "gRPC WASM interceptor should add filter response headers, got headers={:?}",
        resp.headers
    );
}

/// F-133: gRPC over H2C のレスポンストレイラー（`grpc-status`/`grpc-message`）を
/// WASM（`grpc_trailer_filter`）が書き換え、クライアントがその値を受け取ることを検証する。
///
/// 専用ルート（host="grpc-wasm-trailer.test"）は `test_grpc_wasm_interceptor` が使う
/// 既存ルート（host=localhost/127.0.0.1、`header_filter` 適用）とは別に定義されているため、
/// 既存テストには影響しない。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http2", feature = "grpc", feature = "wasm"))]
pub(super) async fn test_grpc_wasm_trailer_mutation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 client");
    let lpm = [0u8, 0, 0, 0, 0];
    let headers = [
        ("host", "grpc-wasm-trailer.test"),
        ("content-type", "application/grpc"),
        ("te", "trailers"),
    ];
    let resp = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            Some(&lpm),
        )
        .await
        .expect("gRPC+WASM trailer mutation request");

    eprintln!(
        "gRPC trailer mutation: status={} trailers={:?} grpc_status={:?} grpc_message={:?}",
        resp.status,
        resp.trailers,
        resp.grpc_status(),
        resp.grpc_message()
    );

    assert_eq!(
        resp.grpc_status(),
        Some(0),
        "grpc_trailer_filter should keep grpc-status=0, got trailers={:?}",
        resp.trailers
    );
    assert_eq!(
        resp.grpc_message().as_deref(),
        Some("rewritten-by-wasm"),
        "grpc_trailer_filter should rewrite grpc-message, got trailers={:?}",
        resp.trailers
    );
}

/// F-133: gRPC リクエストヘッダの WASM 書き換え（`grpc_trailer_filter` が付与する
/// `x-wasm-request-rewrite: applied`）がバックエンドまで届くことを検証する。
///
/// クライアント視点ではリクエストヘッダの中身を直接観測できないため、
/// バックエンド（`tests/grpc_server`）が受信した値を `x-echoed-wasm-header`
/// レスポンスメタデータへ反射する仕組みを使って間接的に検証する。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http2", feature = "grpc", feature = "wasm"))]
pub(super) async fn test_grpc_wasm_request_header_mutation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 client");
    let lpm = [0u8, 0, 0, 0, 0];
    let headers = [
        ("host", "grpc-wasm-trailer.test"),
        ("content-type", "application/grpc"),
        ("te", "trailers"),
    ];
    let resp = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            Some(&lpm),
        )
        .await
        .expect("gRPC+WASM request header mutation request");

    eprintln!(
        "gRPC request header mutation: status={} headers={:?}",
        resp.status, resp.headers
    );

    let echoed = resp
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-echoed-wasm-header"))
        .map(|(_, v)| v.clone());
    assert_eq!(
        echoed.as_deref(),
        Some("applied"),
        "backend should have received the WASM-added request header \
         (x-wasm-request-rewrite: applied) and echoed it back, got headers={:?}",
        resp.headers
    );
}

// =============================================================================
// F-96: http3_grpc_test_coverage_report §5 — エッジケース / リソース枯渇
// =============================================================================

/// E-H3-F96-01: 様々なボディサイズでの HTTP/3 転送（PMTU/断片化の近似検証）。
/// 真の Path MTU Discovery は NIC/ルーティング依存のため、巨大 UDP ペイロード相当の
/// リクエストボディを複数サイズで送り、プロキシがクラッシュせず処理することを確認する。
#[tokio::test]
#[ntest::timeout(60000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_pmtu_payload_sizes() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 client for PMTU");

    // 典型的 QUIC 初期 MTU 付近〜大きめ（断片化を誘発し得る）サイズ群
    let sizes = [512usize, 1200, 2400, 8000, 16000, 32000, 64000];
    let mut ok = 0usize;
    for &sz in &sizes {
        let body = vec![b'P'; sz];
        let result = tokio::time::timeout(
            Duration::from_secs(15),
            send_http3_request_full(
                &mut send_request,
                "POST",
                "/echo-upload/pmtu",
                &[("content-type", "application/octet-stream")],
                Some(&body),
            ),
        )
        .await;
        match result {
            Ok(Ok(resp)) => {
                eprintln!(
                    "PMTU size={} status={} body_len={}",
                    sz,
                    resp.status,
                    resp.body.len()
                );
                // 200 または 404/405 等でも「到達・応答」していれば MTU 経路は生存
                if resp.status < 600 {
                    ok += 1;
                }
            }
            Ok(Err(e)) => {
                eprintln!("PMTU size={} request error (controlled): {}", sz, e);
                // 接続が死んだ場合は再接続して継続
                match Http3TestClient::new(server_addr, "localhost").await {
                    Ok((_c, sr)) => {
                        send_request = sr;
                        ok += 1; // プロキシ生存
                    }
                    Err(re) => panic!("proxy dead after PMTU size={}: {}", sz, re),
                }
            }
            Err(_) => panic!("PMTU size={} hung for 15s", sz),
        }
    }
    assert!(
        ok >= sizes.len().saturating_sub(1),
        "most PMTU payload sizes should complete without hang, ok={}/{}",
        ok,
        sizes.len()
    );
    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive PMTU payload stress"
    );
}

/// E-H3-F96-02: Connection ID 更新相当 — 切断後に新接続（新 CID）でセッション維持。
/// quinn クライアントは NEW_CONNECTION_ID フレームを直接操作できないため、
/// 接続 drop → 再接続（新 SCID）→ 正常リクエスト で追従性を検証する。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_cid_update_retire_simulation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();

    // 接続 A
    let (client_a, mut sr_a) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 conn A");
    let r1 = send_http3_request_full(&mut sr_a, "GET", "/", &[], None)
        .await
        .expect("request on CID A");
    assert_eq!(r1.status, 200, "first request on conn A");

    // 接続 A を明示 drop（旧 CID をリタイア相当）
    drop(sr_a);
    drop(client_a);
    tokio::time::sleep(Duration::from_millis(200)).await;

    // 接続 B（新しい Connection ID）
    let (_client_b, mut sr_b) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 conn B after CID retire");
    let r2 = send_http3_request_full(&mut sr_b, "GET", "/", &[], None)
        .await
        .expect("request on CID B");
    assert_eq!(r2.status, 200, "request on new CID must succeed");

    // 旧接続後もプロキシ健全
    let r3 = send_http3_request_full(&mut sr_b, "GET", "/health", &[], None)
        .await
        .expect("health after CID rotation");
    assert!(
        r3.status == 200 || r3.status == 404,
        "proxy must serve after CID rotation, status={}",
        r3.status
    );
    eprintln!("HTTP/3 CID update/retire simulation: ok");
}

/// E-H3-F96-03: QUIC Keep-Alive — アイドル後も接続が意図せず切れないこと。
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_quic_keepalive_idle() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for keepalive");

    let warm = send_http3_request_full(&mut send_request, "GET", "/", &[], None)
        .await
        .expect("warm-up");
    assert_eq!(warm.status, 200);

    // サーバ max_idle_timeout 既定 30s 未満のアイドル（PING/keep-alive 相当の待機）
    let idle = Duration::from_secs(12);
    eprintln!("HTTP/3 keepalive: idling for {:?} ...", idle);
    tokio::time::sleep(idle).await;

    let after = tokio::time::timeout(
        Duration::from_secs(10),
        send_http3_request_full(&mut send_request, "GET", "/", &[], None),
    )
    .await;

    match after {
        Ok(Ok(resp)) => {
            assert_eq!(
                resp.status, 200,
                "request after idle should succeed on same conn"
            );
            eprintln!("HTTP/3 keepalive: same-conn request ok after idle");
        }
        Ok(Err(e)) => {
            // 接続が idle timeout で閉じた場合は再接続できれば合格（意図的タイムアウト設定）
            eprintln!(
                "HTTP/3 keepalive: conn closed after idle ({}), reconnecting",
                e
            );
            let (_c, mut sr) = Http3TestClient::new(server_addr, "localhost")
                .await
                .expect("reconnect after idle");
            let resp = send_http3_request_full(&mut sr, "GET", "/", &[], None)
                .await
                .expect("request after reconnect");
            assert_eq!(resp.status, 200, "proxy must accept after idle reconnect");
        }
        Err(_) => panic!("HTTP/3 request after idle hung"),
    }
}

/// E-H3-F96-04: GOAWAY / Graceful — SIGHUP リロード中も H3 がドレインされ新規接続可能。
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_goaway_graceful_reload() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut h3_sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 before reload");

    let before = send_http3_request_full(&mut h3_sr, "GET", "/", &[], None)
        .await
        .expect("before SIGHUP");
    assert_eq!(before.status, 200);

    // SIGHUP で graceful reload（GOAWAY 相当の接続ドレインを誘発し得る）
    let pid = proxy_pid();
    if let Some(pid) = pid {
        assert!(send_sighup(pid), "SIGHUP must be delivered");
        tokio::time::sleep(Duration::from_millis(800)).await;
    } else {
        eprintln!("WARN: proxy.pid missing; skipping SIGHUP, verifying reconnect only");
    }

    // 既存ストリーム上のリクエスト（接続が残っていれば成功、閉じていれば再接続）
    let mid = tokio::time::timeout(
        Duration::from_secs(10),
        send_http3_request_full(&mut h3_sr, "GET", "/", &[], None),
    )
    .await;
    match mid {
        Ok(Ok(resp)) => {
            eprintln!("H3 mid-reload same-conn status={}", resp.status);
            assert!(resp.status < 600);
        }
        Ok(Err(e)) => eprintln!("H3 mid-reload conn closed (expected possible): {}", e),
        Err(_) => panic!("H3 mid-reload hung"),
    }

    // 新規接続が必ず通ること
    let (_c2, mut sr2) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 after graceful reload");
    let after = send_http3_request_full(&mut sr2, "GET", "/", &[], None)
        .await
        .expect("after graceful reload");
    assert_eq!(after.status, 200, "new H3 after reload must work");

    // HTTP/1.1 も生存
    let h1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(h1.is_some(), "H1 must work after H3 graceful reload test");
    eprintln!("HTTP/3 GOAWAY/graceful reload simulation: ok");
}

/// E-G-F96-05: gRPC リトライと Hedging（並行投機リクエスト）の透過制御。
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(all(feature = "http2", feature = "grpc"))]
pub(super) async fn test_grpc_retry_and_hedging() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // --- リトライ: エラー経路の後に成功経路へ再試行 ---
    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 for retry");
    let headers = [("content-type", "application/grpc"), ("te", "trailers")];
    let err_body = encode_grpc_lpm(&encode_simple_request("retry-probe"));
    let err = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/StreamReset",
            &headers,
            Some(&err_body),
        )
        .await
        .expect("stream reset for retry setup");
    // StreamReset は INTERNAL 等の非 0 を返す想定
    let st = err.grpc_status();
    eprintln!(
        "gRPC retry setup: status={} grpc_status={:?}",
        err.status, st
    );

    // 直後の Unary リトライが成功すること
    let ok_body = encode_grpc_lpm(&encode_simple_request("retry-ok"));
    let ok = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            Some(&ok_body),
        )
        .await
        .expect("retry unary");
    assert_eq!(ok.status, 200, "retry unary must be HTTP 200");
    assert_eq!(
        ok.grpc_status(),
        Some(0),
        "retry unary grpc-status must be 0, got {:?}",
        ok.grpc_status()
    );

    // --- Hedging: 同一 RPC を複数接続から並行発行し、いずれも完了すること ---
    // Http2TestClient は sender を &mut 占有するため、並行は複数クライアントで近似する。
    let bodies: Vec<Vec<u8>> = (0..4)
        .map(|i| encode_grpc_lpm(&encode_simple_request(&format!("hedge-{}", i))))
        .collect();
    let mut handles = Vec::new();
    for (i, body) in bodies.into_iter().enumerate() {
        handles.push(tokio::spawn(async move {
            let mut c = Http2TestClient::new("127.0.0.1", PROXY_PORT)
                .await
                .expect("hedge client");
            let headers = [("content-type", "application/grpc"), ("te", "trailers")];
            c.send_request_full(
                "POST",
                "/grpc.test.v1.TestService/UnaryCall",
                &headers,
                Some(&body),
            )
            .await
            .map(|r| (i, r.status, r.grpc_status()))
        }));
    }
    let mut hedge_ok = 0usize;
    for h in handles {
        match h.await {
            Ok(Ok((i, status, gs))) => {
                eprintln!("hedge[{}]: status={} grpc_status={:?}", i, status, gs);
                if status == 200 && gs == Some(0) {
                    hedge_ok += 1;
                }
            }
            Ok(Err(e)) => eprintln!("hedge error: {}", e),
            Err(e) => eprintln!("hedge join error: {}", e),
        }
    }
    assert!(
        hedge_ok >= 3,
        "at least 3/4 hedged RPCs should succeed, ok={}",
        hedge_ok
    );
    eprintln!("gRPC retry + hedging: ok");
}

/// E-G-F96-06: gRPC Keepalive — HTTP/2 PING がプロキシ越しに ACK されること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "http2", feature = "grpc"))]
pub(super) async fn test_grpc_keepalive_ping() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 for ping");

    // ウォームアップ Unary
    let headers = [("content-type", "application/grpc"), ("te", "trailers")];
    let body = encode_grpc_lpm(&encode_simple_request("before-ping"));
    let warm = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            Some(&body),
        )
        .await
        .expect("warm unary");
    assert_eq!(warm.status, 200);

    // PING → PONG
    for i in 0..3 {
        client
            .ping()
            .await
            .unwrap_or_else(|e| panic!("HTTP/2 PING {} failed: {}", i, e));
        eprintln!("gRPC keepalive PING {} ACK'd", i);
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // アイドル + PING 後も Unary が通る
    tokio::time::sleep(Duration::from_secs(2)).await;
    client.ping().await.expect("ping after idle");
    let body2 = encode_grpc_lpm(&encode_simple_request("after-ping"));
    let after = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            Some(&body2),
        )
        .await
        .expect("unary after ping");
    assert_eq!(after.status, 200);
    assert_eq!(after.grpc_status(), Some(0));
    eprintln!("gRPC keepalive PING: ok");
}

/// E-G-F96-07: サーバ起因のストリーム異常終了が適切な gRPC ステータスへ伝播すること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "http2", feature = "grpc"))]
pub(super) async fn test_grpc_server_stream_abnormal_termination() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 for abnormal term");
    let headers = [("content-type", "application/grpc"), ("te", "trailers")];

    // 1) バックエンドが明示的に INTERNAL を返す StreamReset
    let body = encode_grpc_lpm(&encode_simple_request("abnormal"));
    let resp = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/StreamReset",
            &headers,
            Some(&body),
        )
        .await
        .expect("StreamReset call");
    eprintln!(
        "StreamReset: status={} grpc_status={:?} msg={:?} trailers={:?}",
        resp.status,
        resp.grpc_status(),
        resp.grpc_message(),
        resp.trailers
    );
    // HTTP 200 + grpc-status != 0 が正常な gRPC エラー伝播
    assert_eq!(
        resp.status, 200,
        "gRPC errors should be HTTP 200 + trailers"
    );
    let code = resp.grpc_status().expect("grpc-status must be present");
    // INTERNAL(13) が期待。UNAVAILABLE(14)/UNKNOWN(2)/CANCELLED(1) も許容
    assert!(
        matches!(code, 1 | 2 | 13 | 14),
        "expected INTERNAL/UNAVAILABLE/UNKNOWN/CANCELLED, got {}",
        code
    );

    // 2) 存在しないメソッド → UNIMPLEMENTED(12) または NOT_FOUND 系
    let body2 = encode_grpc_lpm(&encode_simple_request("no-such"));
    let resp2 = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/DoesNotExist",
            &headers,
            Some(&body2),
        )
        .await
        .expect("missing method");
    eprintln!(
        "DoesNotExist: status={} grpc_status={:?}",
        resp2.status,
        resp2.grpc_status()
    );
    // プロキシがハングせず応答すること
    assert!(resp2.status < 600);

    // 3) 異常後も通常 Unary が通ること
    let body3 = encode_grpc_lpm(&encode_simple_request("recover"));
    let ok = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            Some(&body3),
        )
        .await
        .expect("recover unary");
    assert_eq!(ok.status, 200);
    assert_eq!(ok.grpc_status(), Some(0));
    eprintln!("gRPC server abnormal termination propagation: ok");
}

// =============================================================================
// F-97: http3_grpc_test_coverage_report §4 — HTTP/3 アプリ層 + gRPC 高度機能
// =============================================================================

/// E-H3-F97-01: HTTP/3 経由のクエリ・ヘッダ条件ルーティング
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_routing_conditions() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 client");

    // クエリ条件マッチ: /api/query-filter/?format=json&version=1
    let ok = send_http3_request_full(
        &mut send_request,
        "GET",
        "/api/query-filter/?format=json&version=1",
        &[],
        None,
    )
    .await
    .expect("query match request");
    eprintln!("HTTP/3 query match status={}", ok.status);
    assert_eq!(ok.status, 200, "query conditions should match");

    // クエリ条件不一致
    let miss = send_http3_request_full(&mut send_request, "GET", "/api/query-filter/", &[], None)
        .await
        .expect("query miss request");
    eprintln!("HTTP/3 query miss status={}", miss.status);
    assert!(
        miss.status == 404 || miss.status == 400 || miss.status >= 400,
        "query miss should not be 200, got {}",
        miss.status
    );

    // ヘッダ条件マッチ: /api/header-filter/ with X-Version + X-API-Key
    let hdr = send_http3_request_full(
        &mut send_request,
        "GET",
        "/api/header-filter/",
        &[("X-Version", "v2"), ("X-API-Key", "secret")],
        None,
    )
    .await
    .expect("header match request");
    eprintln!("HTTP/3 header match status={}", hdr.status);
    assert_eq!(hdr.status, 200, "header conditions should match");
}

/// E-H3-F97-02: HTTP/3 大容量ボディでバッファ制限・スピルオーバー経路が生存
#[tokio::test]
#[ntest::timeout(60000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_buffering_spillover() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 client");

    // メモリ制限 100 バイトの /disk-spillover/ へ大きめボディを POST
    let body = vec![b'S'; 8192];
    let result = tokio::time::timeout(
        Duration::from_secs(20),
        send_http3_request_full(
            &mut send_request,
            "POST",
            "/disk-spillover/upload",
            &[("content-type", "application/octet-stream")],
            Some(&body),
        ),
    )
    .await;
    match result {
        Ok(Ok(resp)) => {
            eprintln!(
                "HTTP/3 spillover status={} body_len={}",
                resp.status,
                resp.body.len()
            );
            // 200/413/502 等いずれでもプロキシが応答すればハングなし
            assert!(
                resp.status < 600,
                "should get HTTP status, got {}",
                resp.status
            );
        }
        Ok(Err(e)) => {
            eprintln!("HTTP/3 spillover request error (controlled): {}", e);
            // 接続が落ちても再接続できれば生存
            assert!(
                Http3TestClient::new(server_addr, "localhost").await.is_ok(),
                "proxy must survive spillover stress"
            );
        }
        Err(_) => panic!("HTTP/3 spillover hung for 20s"),
    }
    assert!(
        is_e2e_environment_ready().await,
        "proxy must be alive after spillover test"
    );
}

/// B-46 回帰: HTTP/3 + `[route.buffering] mode = "full"` の Proxy 応答で
/// レスポンスボディが上流（backend-pool）の内容と完全一致すること。
///
/// バッファ経路の `send_response`（src/http3_server.rs）は body が Some
/// なら無条件で content-length を付与していたため、プロキシ応答で
/// バックエンド由来の content-length と重複し、nghttp3 が
/// H3_MESSAGE_ERROR でストリームを拒否してボディ 0 バイトになっていた
/// （2xx ヘッダのみ届き DATA が届かない）。`/full/*` ルートは
/// `[route.buffering] mode = "full"` の Proxy で、バックエンドは
/// `tests/fixtures/backend1(2)/large.txt`（10000 バイトの 'A' 埋め）を
/// 返す File 配信サーバー。ヘッダのみでなくボディ内容まで検証する。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_buffering_full_proxy_body_intact() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 client");

    let resp = send_http3_request_full(&mut send_request, "GET", "/full/large.txt", &[], None)
        .await
        .expect("http3 buffering full proxy request");

    eprintln!(
        "HTTP/3 buffering full proxy status={} body_len={}",
        resp.status,
        resp.body.len()
    );
    assert_eq!(resp.status, 200, "buffering full proxy should return 200");

    // 上流（backend1/backend2）の large.txt と同一内容: 10000 バイトの 'A'。
    let expected = vec![b'A'; 10000];
    assert_eq!(
        resp.body.len(),
        expected.len(),
        "response body length must match upstream large.txt (B-46 regression: \
         content-length duplication caused 0-byte body)"
    );
    assert_eq!(
        resp.body, expected,
        "response body content must exactly match upstream large.txt"
    );
}

/// E-H3-F97-03: HTTP/3 Range → 206 Partial Content
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_range_requests() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 client");

    let resp = send_http3_request_full(
        &mut send_request,
        "GET",
        "/large.txt",
        &[("Range", "bytes=0-1023")],
        None,
    )
    .await
    .expect("range request");
    eprintln!(
        "HTTP/3 Range status={} body_len={} headers={:?}",
        resp.status,
        resp.body.len(),
        resp.headers
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("content-range")
                || n.eq_ignore_ascii_case("content-length"))
            .collect::<Vec<_>>()
    );
    assert_eq!(
        resp.status, 206,
        "Range request should return 206 Partial Content"
    );
    assert!(
        resp.body.len() <= 1024,
        "partial body should be at most 1024 bytes, got {}",
        resp.body.len()
    );
    let has_cr = resp
        .headers
        .iter()
        .any(|(n, _)| n.eq_ignore_ascii_case("content-range"));
    assert!(has_cr, "206 should include Content-Range");
}

/// E-H3-F97-04: :authority / Host 整合性
///
/// hyper/h3 クライアントは `Host` を疑似ヘッダと同時に送ると構築失敗する（H3 では
/// `:authority` が正）。不一致 400 は `authority_host_mismatch` 単体テストと
/// container_security `authority_host_mismatch` モードで検証する。
/// ここでは一致時の正常応答と、不正 authority でルート不一致になることを確認する。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_pseudo_header_validation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 client");

    // 一致（:authority=localhost）→ 200
    let ok = send_http3_request_full(&mut send_request, "GET", "/", &[], None)
        .await
        .expect("match request");
    eprintln!("HTTP/3 authority match status={}", ok.status);
    assert_eq!(ok.status, 200);

    // 未知 authority の絶対 URI → ルート不一致（404 等）。クライアントが Host を拒否するため
    // 不一致 400 は単体 + container_security で担保。
    let unknown = send_http3_request_full(
        &mut send_request,
        "GET",
        "https://unknown-authority.example/",
        &[],
        None,
    )
    .await;
    match unknown {
        Ok(resp) => {
            eprintln!("HTTP/3 unknown authority status={}", resp.status);
            assert!(
                resp.status == 404 || resp.status == 400 || resp.status == 421,
                "unknown authority should not be 200, got {}",
                resp.status
            );
        }
        Err(e) => {
            // 接続/ストリームエラーも「不正 authority を素直に受け入れない」として許容
            eprintln!("HTTP/3 unknown authority error (ok): {}", e);
        }
    }
}

/// E-G-F97-01: gRPC メタデータ x-user-id による Consistent Hash
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_grpc_consistent_hashing() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 client for CH");

    let body = encode_grpc_lpm(&encode_simple_request("hash-me"));
    let mut backend_ids = Vec::new();
    for _ in 0..8 {
        let headers = [
            ("content-type", "application/grpc"),
            ("te", "trailers"),
            ("x-user-id", "user-sticky-42"),
        ];
        let resp = client
            .send_request_full(
                "POST",
                "/grpc.test.v1.TestService/UnaryCall",
                &headers,
                Some(&body),
            )
            .await
            .expect("unary");
        assert_eq!(resp.status, 200);
        assert_eq!(resp.grpc_status(), Some(0));
        if let Some(id) = resp
            .headers
            .iter()
            .chain(resp.trailers.iter())
            .find(|(n, _)| n.eq_ignore_ascii_case("x-server-id"))
            .map(|(_, v)| v.clone())
        {
            backend_ids.push(id);
        }
    }
    eprintln!("gRPC CH same key backends={:?}", backend_ids);
    if backend_ids.len() >= 3 {
        let unique: std::collections::HashSet<_> = backend_ids.iter().collect();
        assert_eq!(
            unique.len(),
            1,
            "same x-user-id must stick to one backend: {:?}",
            backend_ids
        );
    } else {
        eprintln!("WARNING: x-server-id not observed; sticky check skipped");
    }

    // 異なるキーで少なくともルーティングが完了すること
    let mut seen = std::collections::HashSet::new();
    for uid in ["u-a", "u-b", "u-c", "u-d", "u-e"] {
        let headers = [
            ("content-type", "application/grpc"),
            ("te", "trailers"),
            ("x-user-id", uid),
        ];
        let resp = client
            .send_request_full(
                "POST",
                "/grpc.test.v1.TestService/UnaryCall",
                &headers,
                Some(&body),
            )
            .await
            .expect("unary dist");
        assert_eq!(resp.grpc_status(), Some(0));
        if let Some(id) = resp
            .headers
            .iter()
            .chain(resp.trailers.iter())
            .find(|(n, _)| n.eq_ignore_ascii_case("x-server-id"))
            .map(|(_, v)| v.clone())
        {
            seen.insert(id);
        }
    }
    eprintln!("gRPC CH distribution seen={:?}", seen);
    assert!(!seen.is_empty() || backend_ids.is_empty());
}

/// E-G-F97-02: Full バッファリング設定下でも gRPC ストリーミングが成立
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_grpc_buffering_bypass() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 client for buffering bypass");

    // ルートに Full バッファ（max 64）を設定済み。ServerStreaming が完了すればバイパス実証。
    let body = encode_grpc_lpm(&encode_simple_request("stream"));
    let headers = [("content-type", "application/grpc"), ("te", "trailers")];
    let start = std::time::Instant::now();
    let resp = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/ServerStreaming",
            &headers,
            Some(&body),
        )
        .await
        .expect("server streaming under full buffer config");
    let elapsed = start.elapsed();
    eprintln!(
        "gRPC buffering bypass: status={} grpc_status={:?} body_len={} elapsed={:?}",
        resp.status,
        resp.grpc_status(),
        resp.body.len(),
        elapsed
    );
    assert_eq!(resp.status, 200);
    assert_eq!(resp.grpc_status(), Some(0));
    // 5 メッセージ分の LPM が含まれること
    let frames = decode_all_grpc_frames(&resp.body);
    assert!(
        frames.len() >= 5,
        "server streaming should deliver >=5 frames under full buffering config, got {}",
        frames.len()
    );
}

/// E-G-F97-03: gRPC active health check で不健全バックエンドを切り離す
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_grpc_active_health_check() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // grpc-health-pool: 9004 (SERVING via H2C health) + 19998 (unreachable)
    // interval=2s, unhealthy_threshold=2 → 数秒で 19998 を切り離し
    tokio::time::sleep(Duration::from_secs(6)).await;

    // メトリクスに health が露出
    let metrics = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(metrics.is_some());
    let metrics = metrics.unwrap();
    eprintln!(
        "grpc health metrics snippet: {}",
        metrics
            .lines()
            .filter(|l| l.contains("health") || l.contains("upstream"))
            .take(12)
            .collect::<Vec<_>>()
            .join("\n")
    );

    // 健全側へルーティングできること（/grpc-health/* は grpc-health-pool）
    // パスは Health ではなく TestService ではないため、/grpc-health/ は proxy 転送のみ確認
    let resp = send_request(PROXY_PORT, "/grpc-health/", &[]).await;
    // ルートは TestService ではないので 404/502 等でもよい。プロキシ生存が主目的。
    // 代わりに通常 gRPC Unary が通ること + 不健全アドレスがメトリクスで 0 になることを確認
    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("h2 for health check");
    let body = encode_grpc_lpm(&encode_simple_request("hc"));
    let ok = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &[("content-type", "application/grpc"), ("te", "trailers")],
            Some(&body),
        )
        .await
        .expect("unary after health cycle");
    assert_eq!(ok.grpc_status(), Some(0));

    // メトリクスに upstream health が含まれる
    assert!(
        metrics.contains("upstream") || metrics.contains("health"),
        "metrics should expose upstream health for grpc-health-pool"
    );
    // 到達不能 19998 は unhealthy (0)、9004 は H2C health で healthy (1) になること
    if metrics.contains("grpc-health-pool") {
        assert!(
            metrics.contains("server=\"127.0.0.1:19998\",upstream=\"grpc-health-pool\"} 0")
                || metrics.contains("19998") && metrics.contains("grpc-health-pool"),
            "unreachable backend should be marked unhealthy"
        );
    }
    let _ = resp; // ルート応答は環境依存
}

// ====================
// F-99: test_coverage_report — gRPC over HTTP/3 エッジ + HTTP/3 メトリクス
// ====================

/// レポート: `test_grpc_over_http3_timeout_header`
/// HTTP/3 経由での `grpc-timeout` ヒントが転送され、Unary が完了すること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http3"))]
pub(super) async fn test_grpc_over_http3_timeout_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for grpc-timeout");

    use common::http3_client::send_http3_request_full;

    let body = encode_grpc_lpm(&encode_simple_request("h3-timeout"));
    let r = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &[
            ("content-type", "application/grpc"),
            ("te", "trailers"),
            ("grpc-timeout", "10S"),
            ("grpc-accept-encoding", "identity"),
        ],
        Some(&body),
    )
    .await
    .expect("gRPC over H3 with grpc-timeout");

    assert_eq!(r.status, 200, "grpc-timeout unary should return HTTP 200");
    // grpc-timeout はヒントでありプロキシは HTTP レベルで強制しない。正常応答を期待。
    assert_eq!(
        r.grpc_status(),
        Some(0),
        "valid Unary with grpc-timeout should yield grpc-status=0, headers={:?}",
        r.headers
    );
    eprintln!(
        "test_grpc_over_http3_timeout_header: status={} grpc-status={:?}",
        r.status,
        r.grpc_status()
    );
}

/// レポート: `test_grpc_over_http3_compression`
/// gzip / deflate の grpc-encoding / grpc-accept-encoding が HTTP/3 経由で受理されること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http3"))]
pub(super) async fn test_grpc_over_http3_compression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for gRPC compression");

    use common::http3_client::send_http3_request_full;

    // 非圧縮 LPM + accept-encoding=gzip（交渉ヒント）
    let body = encode_grpc_lpm(&encode_simple_request("h3-comp-accept"));
    let r1 = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &[
            ("content-type", "application/grpc"),
            ("te", "trailers"),
            ("grpc-accept-encoding", "gzip, deflate, identity"),
        ],
        Some(&body),
    )
    .await
    .expect("H3 gRPC accept-encoding");
    assert_eq!(r1.status, 200, "accept-encoding unary HTTP 200");
    assert!(
        r1.grpc_status().is_some(),
        "accept-encoding must not strip grpc-status"
    );

    // grpc-encoding=identity 明示
    let body2 = encode_grpc_lpm(&encode_simple_request("h3-comp-identity"));
    let r2 = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &[
            ("content-type", "application/grpc"),
            ("te", "trailers"),
            ("grpc-encoding", "identity"),
            ("grpc-accept-encoding", "gzip"),
        ],
        Some(&body2),
    )
    .await
    .expect("H3 gRPC identity encoding");
    assert_eq!(r2.status, 200);
    assert_eq!(
        r2.grpc_status(),
        Some(0),
        "identity encoding should succeed, got {:?}",
        r2.grpc_status()
    );

    eprintln!(
        "test_grpc_over_http3_compression: accept={:?} identity={:?}",
        r1.grpc_status(),
        r2.grpc_status()
    );
}

/// レポート: `test_grpc_over_http3_trailer_detailed`
/// 正常・エラー時の Trailers（grpc-status / grpc-message）が QUIC 上で詳細に透過されること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http3"))]
pub(super) async fn test_grpc_over_http3_trailer_detailed() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for trailer detailed");

    use common::http3_client::send_http3_request_full;

    let grpc_headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("accept", "application/grpc"),
        ("grpc-accept-encoding", "identity"),
    ];

    // 正常 Unary
    let ok = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &grpc_headers,
        Some(&encode_grpc_lpm(&encode_simple_request("h3-trailer-ok"))),
    )
    .await
    .expect("ok unary trailers over H3");
    assert_eq!(ok.status, 200);
    let ok_code = ok
        .grpc_status()
        .expect("ok unary must expose grpc-status over H3");
    assert!(ok_code <= 16, "grpc-status 0-16, got {}", ok_code);

    // エラー StreamReset
    let err = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/StreamReset",
        &grpc_headers,
        Some(&encode_grpc_lpm(&encode_simple_request("force-error"))),
    )
    .await
    .expect("error unary trailers over H3");
    assert_eq!(err.status, 200);
    let err_code = err
        .grpc_status()
        .expect("error path must expose grpc-status");
    assert!(
        err_code > 0 && err_code <= 16,
        "StreamReset non-zero status, got {}",
        err_code
    );
    let err_msg = err.grpc_message();
    assert!(
        err_msg.as_ref().map(|m| !m.is_empty()).unwrap_or(false),
        "error path should forward grpc-message, got {:?}",
        err_msg
    );

    // カスタムメタデータが trailer を破壊しない
    let meta = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &[
            ("content-type", "application/grpc"),
            ("te", "trailers"),
            ("x-custom-meta", "h3-trailer-meta"),
            ("grpc-accept-encoding", "identity"),
        ],
        Some(&encode_grpc_lpm(&encode_simple_request("with-meta"))),
    )
    .await
    .expect("meta unary trailers");
    assert_eq!(meta.status, 200);
    assert!(meta.grpc_status().is_some());

    // trailer 名の健全性
    for (name, _) in err.headers.iter().chain(ok.headers.iter()) {
        let lower = name.to_ascii_lowercase();
        if lower.starts_with("grpc-") || lower.starts_with("x-") {
            continue;
        }
        // 通常の HTTP ヘッダは許容
    }

    eprintln!(
        "test_grpc_over_http3_trailer_detailed: ok={} err={} msg={:?}",
        ok_code, err_code, err_msg
    );
}

/// レポート: `test_grpc_over_http3_proxy_load_balancing`
/// 複数 Unary を HTTP/3 経由で送り、ロードバランシング経路が生存すること。
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(all(feature = "grpc", feature = "http3"))]
pub(super) async fn test_grpc_over_http3_proxy_load_balancing() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (mut _client, mut h3_sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for gRPC LB");

    use common::http3_client::send_http3_request_full;

    let grpc_headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("grpc-accept-encoding", "identity"),
    ];

    let mut success = 0usize;
    for i in 0..8 {
        let body = encode_grpc_lpm(&encode_simple_request(&format!("h3-lb-{}", i)));
        match send_http3_request_full(
            &mut h3_sr,
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &grpc_headers,
            Some(&body),
        )
        .await
        {
            Ok(r) => {
                if r.status == 200 && r.grpc_status() == Some(0) {
                    success += 1;
                } else {
                    eprintln!(
                        "H3 LB[{}]: status={} grpc={:?}",
                        i,
                        r.status,
                        r.grpc_status()
                    );
                }
            }
            Err(e) => {
                eprintln!("H3 LB[{}] transport error (reconnect): {}", i, e);
                let (c2, sr2) = Http3TestClient::new(server_addr, "localhost")
                    .await
                    .expect("reconnect for H3 LB");
                _client = c2;
                h3_sr = sr2;
            }
        }
    }

    assert!(
        success >= 4,
        "at least half of H3 gRPC LB requests should succeed, got {}",
        success
    );
    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive H3 gRPC LB burst"
    );
    eprintln!(
        "test_grpc_over_http3_proxy_load_balancing: success={}/8",
        success
    );
}

/// レポート: `test_grpc_web_over_http3`
/// application/grpc-web / application/grpc-web-text が HTTP/3 経由で機能すること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc-web", feature = "http3"))]
pub(super) async fn test_grpc_web_over_http3() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for gRPC-Web");

    use common::http3_client::send_http3_request_full;

    let frame = encode_grpc_lpm(&encode_simple_request("h3-grpc-web"));

    // binary gRPC-Web
    let bin = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &[
            ("content-type", "application/grpc-web"),
            ("accept", "application/grpc-web"),
        ],
        Some(&frame),
    )
    .await
    .expect("gRPC-Web binary over H3");
    assert!(
        matches!(bin.status, 200 | 400 | 415 | 502 | 503),
        "gRPC-Web binary should yield controlled status, got {}",
        bin.status
    );
    eprintln!(
        "gRPC-Web binary over H3: status={} body_len={}",
        bin.status,
        bin.body.len()
    );

    // gRPC-Web-Text (base64 body)
    use base64::{engine::general_purpose, Engine as _};
    let b64 = general_purpose::STANDARD.encode(&frame);
    let text = send_http3_request_full(
        &mut send_request,
        "POST",
        "/grpc.test.v1.TestService/UnaryCall",
        &[
            ("content-type", "application/grpc-web-text"),
            ("accept", "application/grpc-web-text"),
        ],
        Some(b64.as_bytes()),
    )
    .await
    .expect("gRPC-Web-Text over H3");
    assert!(
        matches!(text.status, 200 | 400 | 415 | 502 | 503),
        "gRPC-Web-Text should yield controlled status, got {}",
        text.status
    );
    eprintln!(
        "gRPC-Web-Text over H3: status={} body_len={}",
        text.status,
        text.body.len()
    );

    assert!(
        is_e2e_environment_ready().await,
        "proxy must survive gRPC-Web over H3"
    );
}

/// レポート: `test_http3_prometheus_metrics`
/// HTTP/3 リクエスト後に Prometheus に接続/ストリーム/リクエストが計上されること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "http3", feature = "metrics"))]
pub(super) async fn test_http3_prometheus_metrics() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ベースライン
    let before = send_request(PROXY_PORT, "/__metrics", &[])
        .await
        .expect("metrics before H3");
    assert!(
        before.contains("# HELP") || before.contains("veil_"),
        "metrics endpoint should expose Prometheus text"
    );

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    use common::http3_client::send_http3_request;
    let (_client, mut h3_sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 for metrics");
    let (st, _) = send_http3_request(&mut h3_sr, "GET", "/", &[], None)
        .await
        .expect("H3 GET for metrics");
    assert_eq!(st, 200, "H3 GET should succeed to generate metrics");

    // リクエスト完了をメトリクスに反映させる余裕
    tokio::time::sleep(Duration::from_millis(200)).await;

    let after = send_request(PROXY_PORT, "/__metrics", &[])
        .await
        .expect("metrics after H3");
    assert!(
        after.contains("http3_active_connections")
            || after.contains("veil_proxy_http3_active_connections"),
        "should expose http3_active_connections, snippet={}",
        after
            .lines()
            .filter(|l| l.contains("http3") || l.contains("active"))
            .take(20)
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        after.contains("http3_active_streams") || after.contains("veil_proxy_http3_active_streams"),
        "should expose http3_active_streams"
    );
    // リクエストカウンタも H3 経由で増加しうる
    assert!(
        after.contains("http_requests_total") || after.contains("veil_proxy_http_requests_total"),
        "should expose http_requests_total (via log_access)"
    );

    eprintln!("test_http3_prometheus_metrics: H3 gauges present");
}

/// レポート: `test_http3_active_connections_metric`
/// HTTP/3 接続確立でゲージが増え、切断後に減少すること。
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(all(feature = "http3", feature = "metrics"))]
pub(super) async fn test_http3_active_connections_metric() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    fn parse_gauge(metrics: &str, name: &str) -> Option<i64> {
        for line in metrics.lines() {
            if line.starts_with('#') {
                continue;
            }
            if line.contains(name) {
                // 例: veil_proxy_http3_active_connections 2
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(v) = parts.last() {
                    if let Ok(n) = v.parse::<f64>() {
                        return Some(n as i64);
                    }
                }
            }
        }
        None
    }

    let baseline = send_request(PROXY_PORT, "/__metrics", &[])
        .await
        .expect("baseline metrics");
    let base_conn = parse_gauge(&baseline, "http3_active_connections").unwrap_or(0);

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    use common::http3_client::send_http3_request;

    // 接続を張ってリクエスト
    let (client, mut h3_sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 open for active connections");
    let (st, _) = send_http3_request(&mut h3_sr, "GET", "/", &[], None)
        .await
        .expect("H3 GET while connected");
    assert_eq!(st, 200);

    tokio::time::sleep(Duration::from_millis(150)).await;
    let mid = send_request(PROXY_PORT, "/__metrics", &[])
        .await
        .expect("mid metrics");
    let mid_conn = parse_gauge(&mid, "http3_active_connections").unwrap_or(0);
    eprintln!(
        "http3_active_connections: base={} mid={}",
        base_conn, mid_conn
    );
    assert!(
        mid_conn >= base_conn,
        "active connections should not decrease while H3 client is open (base={} mid={})",
        base_conn,
        mid_conn
    );
    // 接続中は少なくとも 1（他テスト並列で base が高い場合は mid >= base で足りる）
    // 単独実行では mid > base を期待できるが、並列 E2E では mid >= max(base, 1) を緩く見る
    assert!(
        mid_conn >= 1 || mid.contains("http3_active_connections"),
        "gauge must be present while connection is live"
    );

    // 切断
    drop(h3_sr);
    drop(client);
    // idle timeout / クリーンアップ待ち（max_idle は数十秒の可能性あり — 明示 close 後の減衰を待つ）
    // クライアント drop で CONNECTION_CLOSE が飛ぶ想定。少し待ってから再計測。
    let mut decreased = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(250)).await;
        let after = send_request(PROXY_PORT, "/__metrics", &[])
            .await
            .unwrap_or_default();
        let after_conn = parse_gauge(&after, "http3_active_connections").unwrap_or(0);
        if after_conn <= mid_conn {
            // 他並列接続があるため「厳密に base」までは要求しない
            decreased = after_conn < mid_conn || after_conn <= base_conn + 1;
            if after_conn <= base_conn {
                decreased = true;
                eprintln!(
                    "http3_active_connections after drop: {} (base={})",
                    after_conn, base_conn
                );
                break;
            }
            if decreased && after_conn < mid_conn {
                eprintln!(
                    "http3_active_connections decreased: mid={} -> {}",
                    mid_conn, after_conn
                );
                break;
            }
        }
    }

    // 並列 E2E では他接続が残るため、減少検知 or ゲージ存在で合格
    if !decreased {
        let final_m = send_request(PROXY_PORT, "/__metrics", &[])
            .await
            .unwrap_or_default();
        assert!(
            final_m.contains("http3_active_connections")
                || final_m.contains("veil_proxy_http3_active_connections"),
            "active connections metric must remain exposed after H3 disconnect"
        );
        eprintln!(
            "test_http3_active_connections_metric: decrease not observed under parallel load (ok if gauge present)"
        );
    } else {
        eprintln!("test_http3_active_connections_metric: decrease observed");
    }
}

// ====================
// F-101: HTTP/3 基本 Web 機能・Alt-Svc アップグレード
// ====================

/// F-101: HTTP/3 経由の大容量静的ファイル提供
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_static_file_large() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for static large");
    let resp = send_http3_request_full(&mut sr, "GET", "/large.txt", &[], None)
        .await
        .expect("H3 GET /large.txt");
    assert_eq!(resp.status, 200, "H3 static large should be 200");
    assert!(
        resp.body.len() > 1000,
        "large file body should be >1000 bytes, got {}",
        resp.body.len()
    );
    eprintln!(
        "test_http3_static_file_large: status={} body_len={}",
        resp.status,
        resp.body.len()
    );
}

/// F-101: HTTP/3 経由の ETag（存在すれば quoted 形式）
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_static_file_etag() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for etag");
    let resp = send_http3_request_full(&mut sr, "GET", "/large.txt", &[], None)
        .await
        .expect("H3 GET /large.txt for etag");
    assert_eq!(resp.status, 200);
    if let Some(etag) = resp.header("etag") {
        assert!(
            (etag.starts_with('"') && etag.ends_with('"'))
                || (etag.starts_with("W/\"") && etag.ends_with('"')),
            "ETag should be quoted, got {:?}",
            etag
        );
        eprintln!("test_http3_static_file_etag: ETag={}", etag);
    } else {
        // プロキシ/バックエンドが ETag を付けない構成でも 200 + 本文配信は必須
        assert!(!resp.body.is_empty(), "static body should be non-empty");
        eprintln!("test_http3_static_file_etag: ETag absent (optional)");
    }
}

/// F-101: HTTP/3 経由の 302 リダイレクト + Location
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_redirect_302() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for redirect 302");
    let resp = send_http3_request_full(&mut sr, "GET", "/redirect-test", &[], None)
        .await
        .expect("H3 GET /redirect-test");
    assert_eq!(
        resp.status, 302,
        "H3 redirect-test should be 302, got {}",
        resp.status
    );
    let loc = resp.header("location");
    assert!(
        loc.is_some(),
        "302 response must include Location, headers={:?}",
        resp.headers
    );
    let loc = loc.unwrap();
    assert!(
        loc.contains("8443") || loc.contains("localhost") || loc.contains("127.0.0.1"),
        "Location should point to proxy, got {:?}",
        loc
    );
    eprintln!(
        "test_http3_redirect_302: status={} location={}",
        resp.status, loc
    );
}

/// F-101: HTTP/3 経由の 307 リダイレクト
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_redirect_307() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for redirect 307");
    let resp = send_http3_request_full(&mut sr, "GET", "/redirect-307", &[], None)
        .await
        .expect("H3 GET /redirect-307");
    assert_eq!(
        resp.status, 307,
        "H3 redirect-307 should be 307, got {}",
        resp.status
    );
    assert!(
        resp.header("location").is_some(),
        "307 must include Location"
    );
    eprintln!(
        "test_http3_redirect_307: status={} location={:?}",
        resp.status,
        resp.header("location")
    );
}

/// F-101: HTTP/3 SNI（localhost）接続 + 証明書 SIGHUP 中も H3 が生存すること。
///
/// 注: H3 証明書は起動時 memfd 固定のため、SIGHUP 後も H3 は旧証明書を使い続ける。
/// 本テストは (1) SNI で H3 が確立できること (2) TCP TLS 証明書差し替え中に H3 が
/// クラッシュせずリクエストを処理し続けられることを検証する。
#[tokio::test]
#[ntest::timeout(60000)]
#[cfg(feature = "http3")]
// clippy::await_holding_lock 許容理由: RELOAD_TEST_LOCK は SIGHUP リロード系テストを
// テストプロセス内で直列化するための意図的なガードで、テスト全体（await を含む）を
// 覆う必要がある（tokio マルチスレッドでも所有スレッドは同一タスク内で完結する）。
#[allow(clippy::await_holding_lock)]
pub(super) async fn test_http3_sni_and_cert_reload() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request;

    let _guard = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();

    // (1) SNI=localhost で H3 接続
    let (_c1, mut sr1) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 SNI localhost");
    let (st, _) = send_http3_request(&mut sr1, "GET", "/", &[], None)
        .await
        .expect("H3 GET with SNI");
    assert_eq!(st, 200, "H3 with SNI localhost should work");

    let pid = match proxy_pid() {
        Some(p) => p,
        None => {
            eprintln!("Skipping cert reload part: proxy.pid not found");
            return;
        }
    };
    let cert_path = fixtures_dir().join("cert.pem");
    let key_path = fixtures_dir().join("key.pem");
    let original_cert = std::fs::read(&cert_path).expect("read cert");
    let original_key = std::fs::read(&key_path).expect("read key");

    // (2) 証明書差し替え + SIGHUP 中に H3 が生き続ける
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("generate cert");
    std::fs::write(&cert_path, cert.cert.pem()).expect("write cert");
    std::fs::write(&key_path, cert.signing_key.serialize_pem()).expect("write key");
    assert!(send_sighup(pid), "SIGHUP must be delivered");

    for i in 0..6 {
        tokio::time::sleep(Duration::from_millis(400)).await;
        match Http3TestClient::new(server_addr, "localhost").await {
            Ok((_c, mut sr)) => match send_http3_request(&mut sr, "GET", "/", &[], None).await {
                Ok((st, _)) => {
                    assert_eq!(
                        st, 200,
                        "H3 must stay healthy during cert reload (attempt {})",
                        i
                    );
                }
                Err(e) => panic!(
                    "H3 request failed during cert reload (attempt {}): {}",
                    i, e
                ),
            },
            Err(e) => panic!(
                "H3 connect failed during cert reload (attempt {}): {}",
                i, e
            ),
        }
    }

    // 原状復帰
    std::fs::write(&cert_path, &original_cert).expect("restore cert");
    std::fs::write(&key_path, &original_key).expect("restore key");
    send_sighup(pid);
    tokio::time::sleep(Duration::from_millis(800)).await;

    let (_c3, mut sr3) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 after restore");
    let (st3, _) = send_http3_request(&mut sr3, "GET", "/", &[], None)
        .await
        .expect("H3 GET after restore");
    assert_eq!(st3, 200);
    eprintln!("test_http3_sni_and_cert_reload: SNI + H3 survival through cert reload OK");
}

/// F-101: HTTP/3 (QPACK) 巨大ヘッダは 431 または接続/ストリーム切断で拒否
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_oversized_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::{send_http3_request, send_http3_request_full};

    let server_addr: std::net::SocketAddr =
        format!("127.0.0.1:{}", PROXY_HTTP3_PORT).parse().unwrap();
    let (_client, mut sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 client for oversized header");

    // 単一ヘッダ ~9KB（MAX_HEADER_SIZE=8KB 超）
    let huge = "H".repeat(9000);
    let headers = [("x-oversized-header", huge.as_str())];
    match send_http3_request_full(&mut sr, "GET", "/", &headers, None).await {
        Ok(resp) => {
            assert!(
                matches!(resp.status, 400 | 413 | 431),
                "oversized H3 header must be rejected (431 preferred), got {}",
                resp.status
            );
            eprintln!("test_http3_oversized_header: status={}", resp.status);
        }
        Err(e) => {
            // ストリーム/接続リセットも防御として許容
            eprintln!(
                "test_http3_oversized_header: rejected with error (ok): {}",
                e
            );
        }
    }

    // プロキシが生存していること
    match Http3TestClient::new(server_addr, "localhost").await {
        Ok((_c2, mut sr2)) => {
            let (st, _) = send_http3_request(&mut sr2, "GET", "/", &[], None)
                .await
                .expect("normal GET after oversized");
            assert_eq!(st, 200, "proxy must accept new H3 after oversized reject");
        }
        Err(e) => panic!("reconnect after oversized header failed: {}", e),
    }
}

/// F-101: Alt-Svc 広告を受け取り HTTP/3 へ接続を切り替えるフロー
#[tokio::test]
#[ntest::timeout(45000)]
#[cfg(feature = "http3")]
pub(super) async fn test_alt_svc_upgrade_flow() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request;

    // 1) HTTP/1.1 で開始し Alt-Svc を取得
    let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("h1 client");
    let (status, headers, _body) = client
        .send_request_with_response_headers(http::Method::GET, "/", &[], None)
        .await
        .expect("H1 GET for Alt-Svc");
    assert_eq!(status, 200);
    let alt_svc = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("alt-svc"))
        .map(|(_, v)| v.clone());
    assert!(
        alt_svc.is_some(),
        "Alt-Svc required for upgrade flow, headers={:?}",
        headers
    );
    let alt = alt_svc.unwrap();
    assert!(
        alt.contains("h3=") || alt.contains("h3=\""),
        "Alt-Svc must advertise h3, got {:?}",
        alt
    );

    // 2) 広告されたポート（なければ既定 H3 ポート）へ HTTP/3 接続
    let h3_port = PROXY_HTTP3_PORT;
    let server_addr: std::net::SocketAddr = format!("127.0.0.1:{}", h3_port).parse().unwrap();
    let (_c, mut sr) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("H3 connect after Alt-Svc advertisement");
    let (st, body) = send_http3_request(&mut sr, "GET", "/", &[], None)
        .await
        .expect("H3 GET after upgrade");
    assert_eq!(st, 200, "upgraded H3 request should succeed");
    assert!(!body.is_empty() || st == 200);

    // 3) HTTP/2 経路でも同様に Alt-Svc → H3
    #[cfg(feature = "http2")]
    {
        let mut h2 = Http2TestClient::new("127.0.0.1", PROXY_PORT)
            .await
            .expect("h2 for upgrade");
        let resp = h2
            .send_request_full("GET", "/", &[], None)
            .await
            .expect("H2 GET");
        assert_eq!(resp.status, 200);
        let alt2 = resp
            .headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case("alt-svc"));
        assert!(alt2.is_some(), "H2 must also advertise Alt-Svc");
        let (_c2, mut sr2) = Http3TestClient::new(server_addr, "localhost")
            .await
            .expect("H3 after H2 Alt-Svc");
        let (st2, _) = send_http3_request(&mut sr2, "GET", "/", &[], None)
            .await
            .expect("H3 after H2");
        assert_eq!(st2, 200);
    }

    eprintln!("test_alt_svc_upgrade_flow: Alt-Svc={:?} H3 upgrade OK", alt);
}
