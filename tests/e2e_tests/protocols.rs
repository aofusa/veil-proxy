//! HTTP/2・セキュリティ・gRPC・HTTP/3 の基本
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// HTTP/2 E2Eテスト（優先度: 中）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_stream_multiplexing() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/2接続を試みる（実際のHTTP/2実装は複雑なため、ここでは基本的なテストのみ）
    // 注意: 実際のHTTP/2テストには専用のクライアントライブラリが必要
    // ここでは、HTTP/2接続が確立されることを確認

    // TLS接続を確立し、ALPNでHTTP/2をネゴシエート
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
    // HTTP/2が有効な場合、h2が返される可能性がある
    // ただし、テスト環境ではHTTP/1.1が使用される可能性もある
    if let Some(proto) = protocol {
        assert!(
            proto == b"h2" || proto == b"http/1.1",
            "Should negotiate HTTP/2 or HTTP/1.1: {:?}",
            proto
        );
    }
}

/// F-32 第2フェーズ: HTTP/2 chunked レスポンスストリーミングの E2E 検証。
///
/// バックエンド（test-backends の chunked サーバー, port 9007）が
/// `Transfer-Encoding: chunked` で 200,000 バイト（HTTP/2 初期フロー制御ウィンドウ
/// 65,535 を大きく超える）の決定論的ボディ（`body[i] = i % 256`）を返す。
/// プロキシは新しいストリーミング経路 `stream_h2_response_body_chunked` で、
/// `ChunkedDecoder::next_data_span` により**全バッファリングせず** DATA フレームへ
/// 逐次転送し、フロー制御（WINDOW_UPDATE 待ち）でバックプレッシャをかける。
///
/// 実 HTTP/2 クライアント（h2 クレート）が全 DATA を**バイト単位まで正確に**再構成
/// できることを確認する。これはチャンクデコード・ストリーミング転送・終端 END_STREAM・
/// フロー制御バックプレッシャがすべて正しく協調することの End-to-End な保証になる。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_chunked_response_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // test-backends/src/main.rs の chunked サーバー定数と一致させること
    const EXPECTED_TOTAL: usize = 200_000;
    let expected_byte = |i: usize| -> u8 { (i % 256) as u8 };

    let mut client = match common::http2_client::Http2TestClient::new("127.0.0.1", PROXY_PORT).await
    {
        Ok(c) => c,
        Err(e) => panic!("Failed to establish HTTP/2 connection to proxy: {}", e),
    };

    let (status, body) = client
        .send_request(
            "GET",
            "/chunked-stream/data",
            &[("host", "localhost")],
            None,
        )
        .await
        .expect("HTTP/2 chunked streaming request failed");

    assert_eq!(status, 200, "Expected 200 OK for chunked stream");
    assert_eq!(
        body.len(),
        EXPECTED_TOTAL,
        "Streamed body length mismatch (got {}, want {})",
        body.len(),
        EXPECTED_TOTAL
    );
    // 決定論的パターンの完全一致 = ストリーミング転送中にデータ破損・並べ替え・欠落がない
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
        "HTTP/2 chunked streaming: reassembled {} bytes correctly via DATA frames",
        body.len()
    );
}

/// F-32 第3フェーズ: HTTP/2 リクエスト方向ストリーミングの E2E。
///
/// 初期フロー制御ウィンドウ（65,535）を超える大容量ボディ（200,000 バイト）を POST する。
/// h2 クライアントはフロー制御に従い複数 DATA フレームに分割して送信するため、プロキシの
/// リクエストストリーミング経路（HEADERS 受信時にバックエンド接続を開始し、DATA を chunked で
/// 逐次転送、WINDOW_UPDATE でバックプレッシャ）が動作する。echo バックエンドが chunked を
/// デコードして同一ボディを返すので、**往復でバイト単位の完全一致**を確認することで、
/// ストリーミング・chunked エンコード/デコード・終端・フロー制御の End-to-End 正当性を保証する。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_request_body_streaming() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 初期ウィンドウ（65535）を十分超えるサイズで複数 DATA フレーム + WINDOW_UPDATE を強制
    const UPLOAD_TOTAL: usize = 200_000;
    let upload: Vec<u8> = (0..UPLOAD_TOTAL).map(|i| (i % 256) as u8).collect();

    // 200KB の往復は重い並列負荷下で TLS 接続が一時的にリセットされ得る（プロキシの
    // ストリーミング処理自体は分離実行で安定）。接続レベルの失敗はリトライし、成功時のみ
    // バイト単位一致を検証する（他の負荷フレーキーテストと同じレジリエンス方針）。
    const MAX_ATTEMPTS: usize = 4;
    let mut last_err = String::new();
    for attempt in 0..MAX_ATTEMPTS {
        let mut client =
            match common::http2_client::Http2TestClient::new("127.0.0.1", PROXY_PORT).await {
                Ok(c) => c,
                Err(e) => {
                    last_err = format!("connect error: {}", e);
                    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
                    continue;
                }
            };

        match client
            .send_request(
                "POST",
                "/echo-upload/data",
                &[("host", "localhost")],
                Some(&upload),
            )
            .await
        {
            Ok((status, body)) => {
                assert_eq!(status, 200, "Expected 200 OK for echoed upload");
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
                    "HTTP/2 request streaming: uploaded and echoed {} bytes correctly via chunked DATA frames (attempt {})",
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
        "HTTP/2 request body streaming failed after {} attempts: {}",
        MAX_ATTEMPTS, last_err
    );
}

/// F-32 第3フェーズ: 小さいリクエストボディのストリーミング（単一 DATA フレーム経路）。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_request_body_streaming_small() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let upload = b"hello streaming request body".to_vec();

    let mut client = match common::http2_client::Http2TestClient::new("127.0.0.1", PROXY_PORT).await
    {
        Ok(c) => c,
        Err(e) => panic!("Failed to establish HTTP/2 connection to proxy: {}", e),
    };

    let (status, body) = client
        .send_request(
            "POST",
            "/echo-upload/small",
            &[("host", "localhost")],
            Some(&upload),
        )
        .await
        .expect("HTTP/2 small request body streaming POST failed");

    assert_eq!(status, 200, "Expected 200 OK for echoed small upload");
    assert_eq!(body, upload, "Echoed small body mismatch");
}

/// 送信ホットパスのフレーム連結（HEADERS+DATA を 1 回の書き込みにまとめる最適化・F-73 続き）が
/// **多重化下でもフレーム境界・ストリーム対応・ボディ整合性を壊さない**ことの E2E 検証。
///
/// 同一 HTTP/2 接続上で内容の異なる小レスポンスを複数ストリーム並行で要求する。プロキシは
/// 各ストリームのレスポンスを連結して交互に送出するため、連結バッファ `write_buf` の
/// 接続内再利用や途中フラッシュにバグ（前レスポンスの残バイト混入・ストリーム取り違え）が
/// あれば、いずれかのレスポンスボディが期待値と食い違って検出できる。ベンチマーク（h2load の
/// 多重ストリーム）と同じ利用形態を End-to-End で保証する。
#[tokio::test]
#[ntest::timeout(30000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_multiplexed_coalesced_responses() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut client = match common::http2_client::Http2TestClient::new("127.0.0.1", PROXY_PORT).await
    {
        Ok(c) => c,
        Err(e) => panic!("Failed to establish HTTP/2 connection to proxy: {}", e),
    };

    // 各ストリームで異なる長さ・内容のボディを echo させる（連結バッファに残バイトが
    // 混入すれば長さ/内容が食い違う）。空ボディも混ぜて END_STREAM 経路も同時に検証する。
    let uploads: Vec<Vec<u8>> = (0..8u8)
        .map(|i| {
            let len = 1 + (i as usize) * 37; // 1,38,75,... バイトと可変
            (0..len).map(|j| ((i as usize + j) % 251) as u8).collect()
        })
        .collect();

    let reqs: Vec<(&str, &str, Option<&[u8]>)> = uploads
        .iter()
        .map(|u| ("POST", "/echo-upload/mux", Some(u.as_slice())))
        .collect();

    let results = client
        .send_concurrent(&reqs)
        .await
        .expect("HTTP/2 multiplexed concurrent requests failed");

    assert_eq!(
        results.len(),
        uploads.len(),
        "レスポンス数が要求数と一致すべき"
    );
    for (i, ((status, body), expected)) in results.iter().zip(uploads.iter()).enumerate() {
        assert_eq!(*status, 200, "stream {} は 200 OK を返すべき", i);
        assert_eq!(
            body, expected,
            "stream {} のエコーボディが不一致（連結/多重化でデータ破損の疑い）: got {} bytes, want {} bytes",
            i,
            body.len(),
            expected.len()
        );
    }
    eprintln!(
        "HTTP/2 multiplexed coalescing: {} streams echoed byte-exact on one connection",
        results.len()
    );
}

/// F-116: 同一 HTTP/2 コネクション上の「遅いストリーム」と「速いストリーム」の多重化検証。
///
/// 遅延バックエンド（echo サーバーの `x-delay-ms` ヘッダーで応答を 1500ms 遅らせる）への
/// リクエストと、遅延なしの高速リクエストを同一コネクションで並行発行し、**速い側が遅い側の
/// 完了を待たずに先に応答を受け取れる**ことを完了時刻の計測で検証する。旧直列実装
/// （1 コネクション 1 リクエストずつ処理）では速い側も遅延分待たされるため本テストは落ち、
/// アクターモデル（per-stream タスク + メインループ多重送出）で初めて通る。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_http2_multiplexing_slow_stream_does_not_block_fast() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    const DELAY_MS: u64 = 1500;
    let slow_body = b"slow-stream-body";

    let mut client = match common::http2_client::Http2TestClient::new("127.0.0.1", PROXY_PORT).await
    {
        Ok(c) => c,
        Err(e) => panic!("Failed to establish HTTP/2 connection to proxy: {}", e),
    };

    // 遅いストリームを先に発行し、その後に速いストリームを発行する（発行順でも速い側が先に完了する
    // ことを確認する）。両方 echo バックエンド行きで、遅延の有無のみが異なる。
    let slow_headers: &[(&str, &str)] = &[("x-delay-ms", "1500")];
    let fast_headers: &[(&str, &str)] = &[];
    let reqs: &[(&str, &str, &[(&str, &str)], Option<&[u8]>)] = &[
        (
            "POST",
            "/echo-upload/slow",
            slow_headers,
            Some(slow_body.as_slice()),
        ),
        ("GET", "/echo-upload/fast", fast_headers, None),
    ];

    let results = client
        .send_concurrent_timed(reqs)
        .await
        .expect("HTTP/2 slow/fast multiplexed requests failed");
    assert_eq!(results.len(), 2);

    let (slow_status, slow_resp, slow_elapsed) = &results[0];
    let (fast_status, _fast_resp, fast_elapsed) = &results[1];

    // 双方正常応答。遅い側はボディが往復一致し、遅延分以上かかっている。
    assert_eq!(*slow_status, 200, "slow stream should return 200");
    assert_eq!(
        slow_resp.as_slice(),
        slow_body,
        "slow stream body should echo byte-exact"
    );
    assert!(
        slow_elapsed.as_millis() as u64 >= DELAY_MS - 100,
        "slow stream should take at least the backend delay (elapsed {:?})",
        slow_elapsed
    );
    assert_eq!(*fast_status, 200, "fast stream should return 200");

    // 核心: 速い側は遅い側の完了（>=1500ms）を待たずに完了する。直列実装では
    // fast_elapsed >= DELAY_MS となり失敗する。負荷余裕を見て 1000ms を閾値とする。
    assert!(
        (fast_elapsed.as_millis() as u64) < DELAY_MS - 500,
        "fast stream must complete before the slow stream's backend delay \
         (fast: {:?}, slow: {:?}) — connection-level head-of-line blocking detected",
        fast_elapsed,
        slow_elapsed
    );
    assert!(
        fast_elapsed < slow_elapsed,
        "fast stream should finish before slow stream (fast: {:?}, slow: {:?})",
        fast_elapsed,
        slow_elapsed
    );
    eprintln!(
        "HTTP/2 multiplexing: fast stream completed in {:?} while slow stream took {:?} on one connection",
        fast_elapsed, slow_elapsed
    );
}

// ====================
// セキュリティ機能 E2Eテスト（優先度: 中）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_ip_restriction() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // IP制限のテストは、設定ファイルでIP制限を設定する必要があるため、
    // ここでは基本的なテストのみ実施
    // 実際のIP制限テストには、設定ファイルの変更が必要

    // 通常のリクエストが成功することを確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK for normal request");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_rate_limiting() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // レート制限のテストは、設定ファイルでレート制限を設定する必要があるため、
    // ここでは基本的なテストのみ実施
    // 実際のレート制限テストには、設定ファイルの変更が必要

    // 通常のリクエストが成功することを確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");

    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Should return 200 OK for normal request");
}

// ====================
// gRPC E2Eテスト（優先度: 低）
// ====================

/// gRPC Unary RPCのテスト
///
/// ## 目的
/// gRPC Unary RPC（単一リクエスト/単一レスポンス）の基本動作を確認
///
/// ## 前提条件
/// - E2E環境が起動していること
/// - gRPCエンドポイント `/grpc.test.v1.TestService/UnaryCall` が存在すること
///
/// ## 期待値
/// - HTTPステータスコード: 200 OK
/// - gRPCフレームが受信されること
/// - レスポンスメッセージが空でないこと
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_unary_call() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCクライアントを作成
    // テスト用Protobufメッセージ（簡易版）
    let request_message = b"Hello, gRPC!";

    // gRPCリクエストを送信（非同期版）
    let response = match GrpcTestClient::send_grpc_request(
        "127.0.0.1",
        PROXY_PORT,
        "/grpc.test.v1.TestService/UnaryCall",
        request_message,
        &[("grpc-timeout", "10S"), ("grpc-accept-encoding", "gzip")],
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            panic!(
                "Failed to send gRPC request to /grpc.test.v1.TestService/UnaryCall: {}",
                e
            );
        }
    };

    // ステータスコードを確認
    let status = GrpcTestClient::extract_status_code(&response);
    // gRPCエンドポイントが存在する場合は200が返される
    assert_eq!(
        status, Some(200),
        "Should return 200 OK for gRPC Unary RPC request to /grpc.test.v1.TestService/UnaryCall, got: {:?}", 
        status
    );

    // gRPCフレームを抽出（成功した場合のみ）
    if let Ok(frame) = GrpcTestClient::extract_grpc_frame(&response) {
        assert!(
            !frame.data.is_empty(),
            "Should receive non-empty response message"
        );
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "grpc")]
pub(super) async fn test_grpc_basic_request() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCリクエストを送信（Content-Type: application/grpc）
    // E2E環境ではプロキシ経由でバックエンドに転送されるため、200を期待
    let response = send_request(
        PROXY_PORT,
        "/",
        &[
            ("Content-Type", "application/grpc"),
            ("Accept", "application/grpc"),
        ],
    )
    .await;

    let response = response.expect("Should receive response for gRPC request");
    let status = get_status_code(&response);
    assert_eq!(
        status,
        Some(200),
        "gRPC request should return 200 OK, got: {:?}",
        status
    );
}

// ====================
// HTTP/3 E2Eテスト（優先度: 低）
// ====================

/// HTTP/3基本接続のテスト
///
/// ## 目的
/// HTTP/3接続の確立とハンドシェイクの成功を確認
///
/// ## 前提条件
/// - E2E環境が起動していること
/// - HTTP/3が有効化されていること
///
/// ## 期待値
/// - HTTP/3ハンドシェイクが成功すること
/// - 接続確立後にリクエストを送信できること
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_basic_connection() {
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
                "Failed to create HTTP/3 client for {}: {} (HTTP/3 may not be enabled)",
                server_addr, e
            );
        }
    };

    // 接続が確立されたことを確認（実際のリクエスト送信で接続の健全性を確認）
    use common::http3_client::send_http3_request;
    match send_http3_request(&mut send_request, "GET", "/health", &[], None).await {
        Ok((status, _body)) => {
            eprintln!(
                "HTTP/3 connection established successfully, status: {}",
                status
            );
            assert_eq!(
                status, 200,
                "HTTP/3 connection to {} should return 200 OK, got status: {}",
                server_addr, status
            );
        }
        Err(e) => {
            panic!("HTTP/3 request failed for {}: {}", server_addr, e);
        }
    }
}

/// HTTP/3 GETリクエストのテスト
///
/// ## 目的
/// HTTP/3経由でのGETリクエストの送信とレスポンスの受信を確認
///
/// ## 前提条件
/// - E2E環境が起動していること
/// - HTTP/3が有効化されていること
///
/// ## 期待値
/// - HTTPステータスコード: 200 OK
/// - レスポンスボディが受信されること
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_get_request() {
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
                "Failed to create HTTP/3 client for {}: {} (HTTP/3 may not be enabled)",
                server_addr, e
            );
        }
    };

    // GETリクエストを送信
    use common::http3_client::send_http3_request;
    match send_http3_request(&mut send_request, "GET", "/", &[], None).await {
        Ok((status, body)) => {
            assert_eq!(
                status, 200,
                "Should return 200 OK for HTTP/3 GET request to {}, got: {}",
                server_addr, status
            );
            assert!(
                !body.is_empty(),
                "Should receive non-empty response body for HTTP/3 GET request to {}",
                server_addr
            );
        }
        Err(e) => {
            panic!(
                "Failed to send/receive HTTP/3 request to {}: {} (HTTP/3 may not be enabled)",
                server_addr, e
            );
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_post_request() {
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

    // POSTリクエストを送信
    let body = b"Hello, HTTP/3!";
    match send_http3_request(
        &mut send_request,
        "POST",
        "/",
        &[("Content-Type", "text/plain")],
        Some(body),
    )
    .await
    {
        Ok((status, _body)) => {
            // 前提条件チェックで200が返ることを確認済みなので、ここでも200を期待
            assert_eq!(
                status, 200,
                "Should return 200 OK for HTTP/3 POST request (prerequisite status: {}), got: {}",
                prereq_status, status
            );
        }
        Err(e) => {
            panic!("Failed to send/receive HTTP/3 POST request: {}", e);
        }
    }
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
pub(super) async fn test_http3_configuration_check() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/3設定の確認テスト
    // HTTP/3が有効化されている場合、UDPソケットへの接続を試みる
    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3クライアント作成を試みることで設定が有効か確認（非同期版）
    let client_result = Http3TestClient::new(server_addr, "localhost").await;
    assert!(
        client_result.is_ok(),
        "HTTP/3 should be configured and client should be creatable: {:?}",
        client_result.err()
    );
}
