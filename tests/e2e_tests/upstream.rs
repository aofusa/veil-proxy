//! 静的コンテンツキャッシュ（F-150）・UDS の上流（F-170）・上流の TLS の ALPN（B-83・F-175）
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// F-150: 静的コンテンツキャッシュ（kTLS 無効の HTTP/1.1 ユーザー空間 TLS 経路）
// ====================
//
// プロキシの kTLS は E2E 既定で無効なので、`/f150-cache/*` ルート（`e2e_setup.sh` が
// `[route.static_file_cache] enabled = true` を付けて生成する）への静的配信は
// `sendfile(2)` に乗らないユーザー空間 rustls 経路になる。F-150 はこの経路に
// 静的コンテンツキャッシュを適用し、リクエストごとの `pread(2)` を排除した。
//
// 配信ファイルは 40000 バイト（16KB の TLS レコードを 3 個ぶんまたぐ）で、
// `writev(2)` 直結送出（F-150 改修 1）と `Bytes::slice` による Range 切り出しを
// レコード境界をまたぐ条件で検証できる。

/// F-150 で配信するフィクスチャの内容（`e2e_setup.sh` が `tr '\0' 'F'` で生成する）。
pub(super) const F150_CACHED_LEN: usize = 40000;

/// レスポンス文字列からボディだけを取り出す。ヘッダとボディの区切りは最初の
/// 空行なので `splitn(2, ..)` を使う（ボディ側に区切り列が現れても壊れない）。
pub(super) fn f150_body(response: &str) -> &str {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .unwrap_or("")
}

/// F-150: キャッシュ有効時、**2 回連続の GET が両方ともバイト単位で一致**すること。
///
/// 1 回目はキャッシュミス（開いている fd から `read_exact_at` で全体を読んで登録）、
/// 2 回目はヒット（`Bytes::clone()` + `Bytes::slice()` のみ）と**別々のコード経路**を
/// 通るため、両方を突き合わせることで登録・取り出しの往復が壊れていないことを検証する。
#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_f150_static_content_cache_http1_repeat_get() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let first = send_request(PROXY_PORT, "/f150-cache/cached.txt", &[]).await;
    assert!(first.is_some(), "1 回目の GET が応答すること");
    let first = first.unwrap();
    assert_eq!(
        get_status_code(&first),
        Some(200),
        "1 回目は 200 を返すこと: {:?}",
        get_status_code(&first)
    );
    let first_body = f150_body(&first);
    assert_eq!(
        first_body.len(),
        F150_CACHED_LEN,
        "1 回目（キャッシュミス経路）のボディ長がファイル全体と一致すること"
    );
    assert!(
        first_body.bytes().all(|b| b == b'F'),
        "1 回目のボディ内容がフィクスチャと一致すること"
    );

    let second = send_request(PROXY_PORT, "/f150-cache/cached.txt", &[]).await;
    assert!(second.is_some(), "2 回目の GET が応答すること");
    let second = second.unwrap();
    assert_eq!(
        get_status_code(&second),
        Some(200),
        "2 回目も 200 を返すこと"
    );
    let second_body = f150_body(&second);

    // キャッシュヒット経路（Bytes::clone + slice）とミス経路（fd から読み込み）で
    // 1 バイトも差が出ないこと。
    assert_eq!(
        second_body, first_body,
        "キャッシュヒット時のボディがミス時と完全に一致すること"
    );
}

/// F-150: キャッシュ経路の Range リクエストが 206 で正しい部分バイト列を返すこと。
///
/// キャッシュヒット時の送出は `Bytes::slice(start..end)`（参照カウントのみ・コピーなし）
/// で行うため、オフセット計算を誤ると静かに壊れた範囲を返す。TLS レコード境界
/// （16384 バイト）をまたぐ範囲を指定して検証する。
#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_f150_static_content_cache_http1_range() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 先に 1 回取得してキャッシュへ載せる（ヒット経路の slice を検証したいため）。
    let _ = send_request(PROXY_PORT, "/f150-cache/cached.txt", &[]).await;

    // 16384（TLS レコード境界）をまたぐ範囲を要求する。
    let start = 16_000usize;
    let end = 16_999usize; // inclusive
    let expected_len = end - start + 1;
    let response = send_request(
        PROXY_PORT,
        "/f150-cache/cached.txt",
        &[("Range", &format!("bytes={}-{}", start, end))],
    )
    .await;
    assert!(response.is_some(), "Range リクエストが応答すること");
    let response = response.unwrap();

    assert_eq!(
        get_status_code(&response),
        Some(206),
        "Range リクエストは 206 Partial Content を返すこと"
    );

    let content_range = get_header_value(&response, "Content-Range");
    assert!(
        content_range.is_some(),
        "206 には Content-Range ヘッダーが付くこと"
    );
    let content_range = content_range.unwrap();
    assert!(
        content_range.contains(&format!("{}-{}", start, end)),
        "Content-Range が要求範囲を反映すること: {}",
        content_range
    );

    let body = f150_body(&response);
    assert_eq!(
        body.len(),
        expected_len,
        "206 のボディ長が要求範囲の長さと一致すること（Bytes::slice のオフセット検証）"
    );
    assert!(
        body.bytes().all(|b| b == b'F'),
        "206 のボディ内容がフィクスチャと一致すること"
    );
}

// ====================
// F-170: Unix ドメインソケット（UDS）バックエンド接続
// ====================
//
// バックエンドは veil 自身を UDS で listen させたもの（tests/e2e_setup.sh の
// backend_uds.toml）。プロキシ側の upstream URL は nginx 互換の
// `http(s)://unix:<socket-path>[:<path-prefix>]` 表記を使う。
// UDS バックエンドへ送る Host ヘッダは既定で "localhost" になる。

/// HTTP/1.1 クライアント → TLS over UDS バックエンドへ中継できること。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_uds_backend_tls() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request(PROXY_PORT, "/uds-tls/", &[]).await;
    assert!(resp.is_some(), "UDS(TLS) route should respond");
    let resp = resp.unwrap();
    assert_eq!(get_status_code(&resp), Some(200));
    assert!(
        resp.contains("Hello from UDS Backend"),
        "UDS(TLS) upstream should forward the backend body: {}",
        resp
    );
}

/// HTTP/1.1 クライアント → h2c over UDS バックエンドへ中継できること。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_uds_backend_h2c() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request(PROXY_PORT, "/uds-h2c/", &[]).await;
    assert!(resp.is_some(), "UDS(h2c) route should respond");
    let resp = resp.unwrap();
    assert_eq!(get_status_code(&resp), Some(200));
    assert!(
        resp.contains("Hello from UDS Backend"),
        "UDS(h2c) upstream should forward the backend body: {}",
        resp
    );
}

/// `http://unix:<path>:/api` のパスプレフィックスが上流パスへ前置されること
/// （`/uds-prefix/v1/test` → バックエンドの `/api/v1/test`）。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_uds_backend_path_prefix() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request(PROXY_PORT, "/uds-prefix/v1/test", &[]).await;
    assert!(resp.is_some(), "UDS prefix route should respond");
    let resp = resp.unwrap();
    assert_eq!(get_status_code(&resp), Some(200));
    assert!(
        resp.contains("uds prefix v1 test"),
        "path prefix should be prepended to the upstream path: {}",
        resp
    );
}

/// 存在しないソケットパスへの UDS バックエンド接続は 502 になること
/// （ハングせずエラー応答へ変換されること）。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_uds_backend_missing_socket_returns_502() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request(PROXY_PORT, "/uds-missing/", &[]).await;
    assert!(
        resp.is_some(),
        "missing UDS socket should still return an HTTP response"
    );
    let status = get_status_code(&resp.unwrap());
    assert!(
        matches!(status, Some(502) | Some(503) | Some(504)),
        "missing UDS socket should map to 502/503/504, got {:?}",
        status
    );
}

/// UDS 上流に対する TCP ヘルスチェックが healthy 判定になり、中継が成功すること
/// （`unix:<path>` 形式のアドレスで同期プローブが接続できること）。
#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_e2e_uds_backend_tcp_health_check() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ヘルスチェック間隔（2 秒）を 1 周以上またいでから確認する。
    tokio::time::sleep(Duration::from_secs(3)).await;

    let resp = send_request(PROXY_PORT, "/uds-health/", &[]).await;
    assert!(resp.is_some(), "UDS health-checked route should respond");
    let resp = resp.unwrap();
    assert_eq!(
        get_status_code(&resp),
        Some(200),
        "UDS upstream must stay healthy under TCP health checks: {}",
        resp
    );
}

/// HTTP/2 クライアント → UDS バックエンド（h2c）の中継。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_e2e_uds_backend_via_http2_client() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = match Http2TestClient::new("127.0.0.1", PROXY_PORT).await {
        Ok(c) => c,
        Err(e) => panic!("Failed to establish HTTP/2 connection to proxy: {}", e),
    };

    let (status, body) = client
        .send_request("GET", "/uds-h2c/", &[("host", "localhost")], None)
        .await
        .expect("HTTP/2 request to UDS backend failed");

    assert_eq!(status, 200, "HTTP/2 → UDS backend should return 200");
    assert!(
        String::from_utf8_lossy(&body).contains("Hello from UDS Backend"),
        "HTTP/2 → UDS backend body mismatch"
    );
}

/// HTTP/3 クライアント → UDS バックエンド（h2c）の中継
/// （下流 QUIC / 上流 UDS の組み合わせ）。
///
/// B-84 修正済み。HTTP/3 のストリーミングバックエンド経路は、上流が h2c の場合
/// サーバ選択直後にバッファ経路（`handle_request` -> `proxy_to_h2c_backend_async`）へ
/// 回すようになったため、HTTP/3 → h2c 上流が通ることを検証する。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(feature = "http3")]
pub(super) async fn test_e2e_uds_backend_via_http3_client() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!(
            "Failed to create HTTP/3 client for {}: {} (HTTP/3 may not be enabled)",
            server_addr, e
        ),
    };

    let (status, body) = http3_get(&mut send_request, "/uds-h2c/")
        .await
        .expect("HTTP/3 request to UDS backend failed");

    assert_eq!(status, 200, "HTTP/3 → UDS backend should return 200");
    assert!(
        String::from_utf8_lossy(&body).contains("Hello from UDS Backend"),
        "HTTP/3 → UDS backend body mismatch"
    );
}

/// HTTP/3 クライアント → UDS バックエンド（TLS）の中継
/// （下流 QUIC / 上流 TLS-over-UDS の組み合わせ）。
///
/// h2c 上流とは別経路（ストリーミング経路をそのまま通る TLS 上流）のカバレッジを
/// 維持するため、h2c 版とは別関数として残す。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(feature = "http3")]
pub(super) async fn test_e2e_uds_backend_via_http3_client_tls() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    let (_client, mut send_request) = match Http3TestClient::new(server_addr, "localhost").await {
        Ok(c) => c,
        Err(e) => panic!(
            "Failed to create HTTP/3 client for {}: {} (HTTP/3 may not be enabled)",
            server_addr, e
        ),
    };

    let (status, body) = http3_get(&mut send_request, "/uds-tls/")
        .await
        .expect("HTTP/3 request to UDS backend failed");

    assert_eq!(status, 200, "HTTP/3 → UDS backend should return 200");
    assert!(
        String::from_utf8_lossy(&body).contains("Hello from UDS Backend"),
        "HTTP/3 → UDS backend body mismatch"
    );
}

// ====================
// B-83: 上流 TLS の ALPN が h2 を広告しても中継が壊れないこと
// ====================

/// B-83 回帰テスト: 上流が ALPN で h2 を広告しても中継が壊れないこと。
///
/// `backend_h2c.toml` は `http2_enabled = true` のため TLS リスナー
/// （`BACKEND_H2C_TLS_PORT`）が ALPN で `h2, http/1.1` を広告する。修正前は
/// 上流クライアントの ALPN にも `h2` が含まれていたため、上流が h2 を選択して
/// veil が HTTP/1.1 のバイト列を送りつけ、中継が 502 になっていた。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_alpn_h2_upstream_regression() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let resp = send_request(PROXY_PORT, "/alpn-h2/", &[]).await;
    assert!(resp.is_some(), "ALPN h2 upstream route should respond");
    let resp = resp.unwrap();
    assert_eq!(
        get_status_code(&resp),
        Some(200),
        "ALPN h2 upstream should not 502: {}",
        resp
    );
    assert!(
        resp.contains("H2C Backend"),
        "ALPN h2 upstream should forward the backend body: {}",
        resp
    );
}

/// B-97: HTTP/3 のバッファ経路（`buffering = "full"`）の要求が上流を待っている間も、
/// 同じ QUIC 接続の他の要求が進む。
///
/// 従来はメインループがバッファ経路の要求を await しており、上流の応答待ち（ここでは
/// echo バックエンドの `x-delay-ms` で 1.5 秒）の間、そのワーカーの全接続が止まっていた。
/// 同じ接続の 2 本目のストリームを使うので、必ず同じワーカーで処理される。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(feature = "http3")]
pub(super) async fn test_b97_http3_buffered_request_does_not_block_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 connect");

    let mut slow_sr = send_request.clone();
    let slow = tokio::spawn(async move {
        let started = std::time::Instant::now();
        let r = send_http3_request_full(
            &mut slow_sr,
            "GET",
            "/echo-full/b97-slow",
            &[("x-delay-ms", "1500")],
            None,
        )
        .await
        .map(|r| r.status)
        .map_err(|e| e.to_string());
        (r, started.elapsed())
    });
    // 遅い要求が上流で待ちに入ってから速い要求を出す。
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    let mut fast_sr = send_request.clone();
    let started = std::time::Instant::now();
    let fast = send_http3_request_full(&mut fast_sr, "GET", "/echo-full/b97-fast", &[], None)
        .await
        .expect("fast HTTP/3 request");
    let fast_elapsed = started.elapsed();
    assert_eq!(fast.status, 200);

    let (slow_status, slow_elapsed) = slow.await.expect("slow task");
    assert_eq!(slow_status.expect("slow HTTP/3 request"), 200);
    assert!(
        slow_elapsed >= std::time::Duration::from_millis(1400),
        "slow request should wait for the delayed upstream: {:?}",
        slow_elapsed
    );
    assert!(
        fast_elapsed < std::time::Duration::from_millis(1000),
        "fast request on the same connection must not wait for the slow buffered request: {:?}",
        fast_elapsed
    );
}

/// B-104: HTTP/3 で受けた要求の上流接続がプールで再利用されること（平文・TLS）。
///
/// echo バックエンドは接続ごとの通し番号を `x-backend-conn-id` で返す。同じ QUIC 接続
/// （＝同じ HTTP/3 ワーカー）から順に送った要求は、同じ上流接続で処理されるはず。
/// 従来は 1 要求 1 接続（`Connection: close`）で、毎回番号が変わっていた。
#[tokio::test]
#[ntest::timeout(60000)]
#[cfg(feature = "http3")]
pub(super) async fn test_b104_http3_reuses_upstream_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    for prefix in ["/echo-upload", "/echo-upload-tls"] {
        let (mut _client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
            .await
            .expect("HTTP/3 connect");
        let mut ids = std::collections::BTreeSet::new();
        for i in 0..6 {
            let path = format!("{}/b104-{}", prefix, i);
            let resp = send_http3_request_full(&mut send_request, "GET", &path, &[], None)
                .await
                .unwrap_or_else(|e| panic!("GET {} failed: {}", path, e));
            assert_eq!(resp.status, 200, "GET {}", path);
            let id = resp
                .header("x-backend-conn-id")
                .unwrap_or_else(|| panic!("{}: x-backend-conn-id missing", path))
                .to_string();
            ids.insert(id);
        }
        // 他の HTTP/3 テスト（test_b104_http3_head_on_keepalive_upstream 等）が同じ上流へ
        // 並行に要求すると、その間はプールの接続が貸し出し中で 2 本目が正当に開く。
        // 1 要求 1 接続（B-104 以前）なら 6 本になるので、半分以下なら再利用できている。
        assert!(
            ids.len() <= 3,
            "{}: sequential HTTP/3 requests must reuse upstream connections, got {:?}",
            prefix,
            ids
        );
    }
}

/// B-104: HEAD への応答は Content-Length があっても本文を読まない（キープアライブの上流は
/// 閉じないため、読みに行くとタイムアウトまで止まる）。続く GET も同じ接続で通ること。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http3")]
pub(super) async fn test_b104_http3_head_on_keepalive_upstream() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use common::http3_client::send_http3_request_full;
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (mut _client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 connect");
    let started = std::time::Instant::now();
    let head = send_http3_request_full(
        &mut send_request,
        "HEAD",
        "/echo-head-ka/b104-head",
        &[],
        None,
    )
    .await
    .expect("HEAD");
    assert_eq!(head.status, 200);
    assert!(head.body.is_empty(), "HEAD must not carry a body");
    assert!(
        started.elapsed() < std::time::Duration::from_secs(5),
        "HEAD must not wait for a body that never comes ({:?})",
        started.elapsed()
    );
    let get = send_http3_request_full(
        &mut send_request,
        "GET",
        "/echo-head-ka/b104-after-head",
        &[],
        None,
    )
    .await
    .expect("GET after HEAD");
    assert_eq!(get.status, 200);
    assert_eq!(
        head.header("x-backend-conn-id"),
        get.header("x-backend-conn-id"),
        "the connection used for HEAD must be reusable"
    );
}

/// B-106 / F-174: 上流 HTTP/2 のフロー制御。接続ウィンドウ（tonic/hyper の既定 1MB）を超える
/// gRPC 要求（1.5MB）を HTTP/2 で受けて h2c 上流へ中継しても、WINDOW_UPDATE を待って送り切る。
///
/// 旧 `H2cClient` は接続ウィンドウを超えると待たずに "Send window exhausted" で失敗し、
/// ストリーム単位のウィンドウは見ていなかった。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_b106_grpc_large_request_respects_upstream_flow_control() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    // SimpleRequest { message = "x" * N }（field 1, wire type 2, varint 長）
    const N: usize = 1536 * 1024;
    let mut msg = vec![0x0a];
    let mut len = N;
    while len >= 0x80 {
        msg.push((len as u8 & 0x7f) | 0x80);
        len >>= 7;
    }
    msg.push(len as u8);
    msg.extend(std::iter::repeat_n(b'x', N));
    let lpm = encode_grpc_lpm(&msg);

    let mut client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("HTTP/2 client");
    let headers = [
        ("content-type", "application/grpc"),
        ("te", "trailers"),
        ("x-user-id", "b106"),
    ];
    let resp = client
        .send_request_full(
            "POST",
            "/grpc.test.v1.TestService/UnaryCall",
            &headers,
            Some(&lpm),
        )
        .await
        .expect("large gRPC request");
    assert_eq!(resp.status, 200, "headers={:?}", resp.headers);
    assert_eq!(
        resp.grpc_status(),
        Some(0),
        "headers={:?} trailers={:?}",
        resp.headers,
        resp.trailers
    );
    let frames = decode_all_grpc_frames(&resp.body);
    assert_eq!(frames.len(), 1);
    assert!(
        frames[0].data.len() >= N,
        "echoed message must be complete: {} bytes",
        frames[0].data.len()
    );
}

/// F-171: HTTP/3 → h2c 上流の全二重中継。双方向ストリーミング gRPC で、要求ストリームを
/// 閉じる前に 1 通目の応答メッセージが届く（旧実装は要求本文を全部受けてから上流へ送るため、
/// 要求を閉じるまで何も返らずタイムアウトする）。続けて 2 通目を送り、閉じたあとに
/// grpc-status 0 のトレーラーで終わる。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "http3", feature = "grpc", feature = "http2"))]
pub(super) async fn test_f171_http3_grpc_bidi_streaming_is_full_duplex() {
    use bytes::Buf;
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 connect");
    let req = http::Request::builder()
        .method("POST")
        .uri("https://localhost/grpc.test.v1.TestService/BidirectionalStreaming")
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .unwrap();
    let mut stream = send_request.send_request(req).await.expect("open stream");

    // 1 通目を送り、要求を閉じずに応答を待つ。
    stream
        .send_data(bytes::Bytes::from(encode_grpc_lpm(&encode_simple_request(
            "first",
        ))))
        .await
        .expect("send first");
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), stream.recv_response())
        .await
        .expect("response head must arrive before the request stream is finished")
        .expect("recv_response");
    assert_eq!(resp.status(), 200);

    let mut buf: Vec<u8> = Vec::new();
    // LPM 1 通ぶん（5 バイトのヘッダ + 本体）が揃うまで読む。
    async fn read_one<S: h3::quic::RecvStream>(
        stream: &mut h3::client::RequestStream<S, bytes::Bytes>,
        buf: &mut Vec<u8>,
    ) -> Vec<u8> {
        loop {
            if buf.len() >= 5 {
                let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                if buf.len() >= 5 + len {
                    let msg = buf[5..5 + len].to_vec();
                    buf.drain(..5 + len);
                    return msg;
                }
            }
            let mut chunk =
                tokio::time::timeout(std::time::Duration::from_secs(5), stream.recv_data())
                    .await
                    .expect("echoed message must arrive while the request stream is open")
                    .expect("recv_data")
                    .expect("response ended early");
            while chunk.has_remaining() {
                let c = chunk.chunk().to_vec();
                chunk.advance(c.len());
                buf.extend_from_slice(&c);
            }
        }
    }
    let first = read_one(&mut stream, &mut buf).await;
    assert!(
        first.ends_with(b"first"),
        "first echo: {:?}",
        String::from_utf8_lossy(&first)
    );

    stream
        .send_data(bytes::Bytes::from(encode_grpc_lpm(&encode_simple_request(
            "second",
        ))))
        .await
        .expect("send second");
    let second = read_one(&mut stream, &mut buf).await;
    assert!(second.ends_with(b"second"));

    stream.finish().await.expect("finish request");
    while let Some(mut rest) = stream.recv_data().await.expect("recv tail") {
        rest.advance(rest.remaining());
    }
    let trailers = stream
        .recv_trailers()
        .await
        .expect("recv_trailers")
        .expect("grpc trailers");
    assert_eq!(
        trailers.get("grpc-status").map(|v| v.to_str().unwrap()),
        Some("0"),
        "trailers={:?}",
        trailers
    );
}

/// F-171: HTTP/2 → h2c 上流の全二重中継（HTTP/3 版と同じ確認を HTTP/2 で行う）。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(all(feature = "grpc", feature = "http2"))]
pub(super) async fn test_f171_http2_grpc_bidi_streaming_is_full_duplex() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let client = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("HTTP/2 client");
    let mut sender = client.raw_sender().ready().await.expect("h2 ready");
    let req = http::Request::builder()
        .method("POST")
        .uri("https://127.0.0.1/grpc.test.v1.TestService/BidirectionalStreaming")
        .header("content-type", "application/grpc")
        .header("te", "trailers")
        .body(())
        .unwrap();
    let (resp_fut, mut send) = sender.send_request(req, false).expect("open stream");
    send.send_data(
        bytes::Bytes::from(encode_grpc_lpm(&encode_simple_request("first"))),
        false,
    )
    .expect("send first");
    let resp = tokio::time::timeout(std::time::Duration::from_secs(5), resp_fut)
        .await
        .expect("response head must arrive before the request stream is finished")
        .expect("response");
    assert_eq!(resp.status(), 200);
    let mut body = resp.into_body();

    let mut buf: Vec<u8> = Vec::new();
    async fn read_one(body: &mut h2::RecvStream, buf: &mut Vec<u8>) -> Vec<u8> {
        loop {
            if buf.len() >= 5 {
                let len = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;
                if buf.len() >= 5 + len {
                    let msg = buf[5..5 + len].to_vec();
                    buf.drain(..5 + len);
                    return msg;
                }
            }
            let chunk = tokio::time::timeout(std::time::Duration::from_secs(5), body.data())
                .await
                .expect("echoed message must arrive while the request stream is open")
                .expect("response ended early")
                .expect("data");
            let _ = body.flow_control().release_capacity(chunk.len());
            buf.extend_from_slice(&chunk);
        }
    }
    let first = read_one(&mut body, &mut buf).await;
    assert!(first.ends_with(b"first"));

    send.send_data(
        bytes::Bytes::from(encode_grpc_lpm(&encode_simple_request("second"))),
        true,
    )
    .expect("send second");
    let second = read_one(&mut body, &mut buf).await;
    assert!(second.ends_with(b"second"));

    while let Some(chunk) = body.data().await {
        let chunk = chunk.expect("tail");
        let _ = body.flow_control().release_capacity(chunk.len());
    }
    let trailers = body
        .trailers()
        .await
        .expect("trailers")
        .expect("grpc trailers");
    assert_eq!(
        trailers.get("grpc-status").map(|v| v.to_str().unwrap()),
        Some("0")
    );
}

// ====================
// F-175: 上流 HTTPS の HTTP/2（ALPN）
// ====================

/// F-175: `http2 = "on"` の HTTPS 上流（ALPN で h2 を選ぶ veil）へ、HTTP/1.1 で受けた要求を
/// HTTP/2 で中継できる。"on" は上流が h2 を選ばなければ 502 なので、200 なら h2 で話している。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_f175_https_upstream_h2_from_http1_client() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    for path in ["/alpn-h2-on/", "/alpn-h2-off/", "/alpn-h2/"] {
        for _ in 0..3 {
            let resp = send_request(PROXY_PORT, path, &[])
                .await
                .unwrap_or_else(|| panic!("{} should respond", path));
            assert_eq!(get_status_code(&resp), Some(200), "{}: {}", path, resp);
            assert!(resp.contains("H2C Backend"), "{}: {}", path, resp);
        }
    }
}

/// F-175: HTTP/2・HTTP/3 で受けた要求も `http2 = "on"` の HTTPS 上流へ HTTP/2 で中継できる。
#[tokio::test]
#[ntest::timeout(20000)]
#[cfg(all(feature = "http2", feature = "http3"))]
pub(super) async fn test_f175_https_upstream_h2_from_h2_and_h3_clients() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let mut h2 = Http2TestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("HTTP/2 client");
    for _ in 0..3 {
        let (status, body) = h2
            .send_request("GET", "/alpn-h2-on/", &[], None)
            .await
            .expect("HTTP/2 request");
        assert_eq!(status, 200);
        assert!(String::from_utf8_lossy(&body).contains("H2C Backend"));
    }

    use common::http3_client::send_http3_request_full;
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");
    let (_client, mut send_request) = Http3TestClient::new(server_addr, "localhost")
        .await
        .expect("HTTP/3 connect");
    for _ in 0..3 {
        let resp = send_http3_request_full(&mut send_request, "GET", "/alpn-h2-on/", &[], None)
            .await
            .expect("HTTP/3 request");
        assert_eq!(resp.status, 200);
        assert!(String::from_utf8_lossy(&resp.body).contains("H2C Backend"));
    }
}

/// F-175: HTTP/2 を話さない HTTPS 上流に `http2 = "on"` を指定すると 502 になる
/// （"auto" なら HTTP/1.1 に切り替える。既存の HTTPS 上流の E2E がその経路を通る）。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_f175_http2_on_rejects_http1_only_upstream() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let resp = send_request(PROXY_PORT, "/h1-only-h2-on/", &[])
        .await
        .expect("should respond");
    assert_eq!(get_status_code(&resp), Some(502), "{}", resp);
}

/// F-177: プールへ戻した上流接続が次の要求の前に切れていても（上流が `Connection: close` を付けずに
/// 応答直後に閉じる）、冪等な要求は新しい接続で 1 回だけ再送されて 200 になる。
///
/// B-93 の生存確認はアイドル 1ms 未満の接続を省くため、同じクライアント接続から間を空けずに
/// 続けて送ると、死んだ接続を引いて応答前に EOF になる。旧実装はその要求が 502 になっていた。
#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_f177_idempotent_retry_on_dead_pooled_connection() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let connector = tokio_rustls::TlsConnector::from(create_client_config());
    let tcp = tokio::net::TcpStream::connect(("127.0.0.1", PROXY_PORT))
        .await
        .expect("TCP connect");
    let mut stream = connector
        .connect(ServerName::try_from("localhost").unwrap(), tcp)
        .await
        .expect("TLS connect");
    let req = b"GET /echo-upload/f177 HTTP/1.1\r\nHost: localhost\r\nx-close-silently: 1\r\n\r\n";
    for i in 0..30 {
        stream.write_all(req).await.expect("write");
        // 応答 1 つぶん（ヘッダ + Content-Length 0）を読む。
        let mut buf = Vec::new();
        let mut tmp = [0u8; 4096];
        loop {
            let n = tokio::time::timeout(std::time::Duration::from_secs(5), stream.read(&mut tmp))
                .await
                .expect("response in time")
                .expect("read");
            assert!(n > 0, "connection closed at request {}", i);
            buf.extend_from_slice(&tmp[..n]);
            if buf.windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = String::from_utf8_lossy(&buf);
        assert!(head.starts_with("HTTP/1.1 200"), "request {}: {}", i, head);
    }
}

/// 平文の HTTP/1.1 で応答を 1 つ読む（Content-Length の本文まで）。ステータス行を返す。
pub(super) fn f205_read_response(stream: &mut TcpStream, carry: &mut Vec<u8>) -> String {
    let mut buf = [0u8; 4096];
    loop {
        if let Some(end) = carry.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&carry[..end]).to_string();
            let len = head
                .lines()
                .find_map(|l| {
                    let l = l.to_ascii_lowercase();
                    l.strip_prefix("content-length:")
                        .map(|v| v.trim().parse::<usize>().unwrap())
                })
                .unwrap_or(0);
            while carry.len() < end + 4 + len {
                let n = stream.read(&mut buf).expect("read body");
                assert!(n > 0, "connection closed in the body");
                carry.extend_from_slice(&buf[..n]);
            }
            carry.drain(..end + 4 + len);
            return head.lines().next().unwrap_or_default().to_string();
        }
        let n = stream.read(&mut buf).expect("read head");
        assert!(n > 0, "connection closed before a response");
        carry.extend_from_slice(&buf[..n]);
    }
}

/// F-205: キープアライブの次の要求を蓄積バッファへ直接読む経路と、期限（既定 20ms）を過ぎて従来の待ち方へ
/// 戻る経路の両方で、同じ接続の要求が正しく処理される（すぐ・期限の後・パイプライン・分割・大きな本文）。
#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_f205_keepalive_direct_read_and_fallback() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let mut s = TcpStream::connect(format!("127.0.0.1:{}", PROXY_H2C_PORT)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut carry = Vec::new();
    let req = b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n";
    // 1. 接続直後・応答の直後（直接読む経路）
    for _ in 0..20 {
        s.write_all(req).unwrap();
        assert!(f205_read_response(&mut s, &mut carry).contains(" 200"));
    }
    // 2. 期限を過ぎてから（読みを取り消して従来の待ち方へ戻った後）
    for wait_ms in [30u64, 150, 400] {
        std::thread::sleep(Duration::from_millis(wait_ms));
        s.write_all(req).unwrap();
        assert!(
            f205_read_response(&mut s, &mut carry).contains(" 200"),
            "after {}ms",
            wait_ms
        );
    }
    // 3. パイプライン（1 回の書き込みに 3 要求）
    s.write_all(&req.repeat(3)).unwrap();
    for _ in 0..3 {
        assert!(f205_read_response(&mut s, &mut carry).contains(" 200"));
    }
    // 4. 要求を 2 回に分けて送る（直接読んだ断片の後を従来の読みで受ける）
    s.write_all(b"GET / HTTP/1.1\r\nHo").unwrap();
    std::thread::sleep(Duration::from_millis(50));
    s.write_all(b"st: localhost\r\n\r\n").unwrap();
    assert!(f205_read_response(&mut s, &mut carry).contains(" 200"));
    // 5. 蓄積バッファ（8KB）を超える本文
    let body = vec![b'a'; 20000];
    let mut post = format!(
        "POST / HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
        body.len()
    )
    .into_bytes();
    post.extend_from_slice(&body);
    s.write_all(&post).unwrap();
    let status = f205_read_response(&mut s, &mut carry);
    assert!(status.starts_with("HTTP/1.1 "), "{}", status);
    // 接続はまだ使える
    s.write_all(req).unwrap();
    assert!(f205_read_response(&mut s, &mut carry).contains(" 200"));
}
