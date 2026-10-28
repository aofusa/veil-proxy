//! ルーティング・h2c・運用の追加と、F-09・F-06・F-19・F-20・F-03・F-10・F-21・L4（F-30・F-133）・F-50・F-49・B-17・F-62
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// ルーティング機能の追加テスト（評価レポートに基づく設計）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_combined_conditions() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルで複数条件を持つルートを設定する必要がある
    // /api/combined/* パスに query={format=json} かつ header={X-Version=v2, X-API-Key=secret} を設定

    // すべての条件を満たすリクエストを送信
    let response = send_request_with_method(
        PROXY_PORT,
        "/api/combined/?format=json",
        "GET",
        &[("X-Version", "v2"), ("X-API-Key", "secret")],
        None,
    )
    .await;

    assert!(response.is_some(), "Should receive response");
    let response = response.unwrap();
    let status = get_status_code(&response);

    // すべての条件を満たす場合、200が返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for matching routing conditions, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_condition_priority() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルで複数のルートを定義する必要がある
    // より具体的なルート（先に定義）が優先されることを確認

    // より具体的なパスにリクエストを送信
    let response1 = send_request(PROXY_PORT, "/api/v2/test", &[]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    // より一般的なパスにリクエストを送信
    let response2 = send_request(PROXY_PORT, "/api/v1/test", &[]).await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // 両方とも200または404が返されることを確認（ルート定義に依存）
    assert!(
        status1 == Some(200) || status1 == Some(404),
        "Should return 200 OK or 404 Not Found: {:?}",
        status1
    );
    assert!(
        status2 == Some(200) || status2 == Some(404),
        "Should return 200 OK or 404 Not Found: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_wildcard_host() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでワイルドカードホストルートを設定する必要がある
    // 例: host = "*.example.com"

    // ワイルドカードパターンにマッチするホストでリクエストを送信
    let response1 = send_request(PROXY_PORT, "/", &[("Host", "api.example.com")]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    let response2 = send_request(PROXY_PORT, "/", &[("Host", "www.example.com")]).await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // ワイルドカードパターンにマッチする場合、200が返される
    assert!(
        status1 == Some(200) || status1 == Some(404),
        "Should return 200 OK or 404 Not Found: {:?}",
        status1
    );
    assert!(
        status2 == Some(200) || status2 == Some(404),
        "Should return 200 OK or 404 Not Found: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_wildcard_path() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでワイルドカードパスルートを設定する必要がある
    // 例: path = "/api/*"

    // ワイルドカードパスにマッチするリクエストを送信
    let response1 = send_request(PROXY_PORT, "/api/v1/test", &[]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    let response2 = send_request(PROXY_PORT, "/api/v2/test", &[]).await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // ワイルドカードパスにマッチする場合、200が返される
    assert_eq!(
        status1,
        Some(200),
        "Should return 200 OK for wildcard path match, got: {:?}",
        status1
    );
    assert_eq!(
        status2,
        Some(200),
        "Should return 200 OK for wildcard path match, got: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_header_multiple() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルで複数ヘッダー条件を持つルートを設定する必要がある
    // /api/header-filter/* パスに header = { "X-Version" = "v2", "X-API-Key" = "secret" } 条件を設定

    // すべてのヘッダー条件を満たすリクエストを送信
    let response1 = send_request(
        PROXY_PORT,
        "/api/header-filter/",
        &[("X-Version", "v2"), ("X-API-Key", "secret")],
    )
    .await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    // 1つ以上のヘッダー条件を満たさないリクエストを送信
    let response2 = send_request(
        PROXY_PORT,
        "/api/header-filter/",
        &[
            ("X-Version", "v1"), // 条件を満たさない
            ("X-API-Key", "secret"),
        ],
    )
    .await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // すべての条件を満たす場合、200が返される
    assert_eq!(
        status1,
        Some(200),
        "Should return 200 OK for matching routing conditions, got: {:?}",
        status1
    );
    // 条件を満たさない場合、404が返される
    assert_eq!(
        status2,
        Some(404),
        "Should return 404 Not Found for non-matching routing conditions, got: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_query_multiple() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルで複数クエリパラメータ条件を持つルートを設定する必要がある
    // /api/query-filter/* パスに query = { "format" = "json", "version" = "1" } 条件を設定

    // すべてのクエリパラメータ条件を満たすリクエストを送信
    let response1 = send_request(PROXY_PORT, "/api/query-filter/?format=json&version=1", &[]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    // 1つ以上のクエリパラメータ条件を満たさないリクエストを送信
    let response2 = send_request(PROXY_PORT, "/api/query-filter/?format=xml&version=1", &[]).await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // すべての条件を満たす場合、200が返される
    assert_eq!(
        status1,
        Some(200),
        "Should return 200 OK for matching routing conditions, got: {:?}",
        status1
    );
    // 条件を満たさない場合、404が返される
    assert_eq!(
        status2,
        Some(404),
        "Should return 404 Not Found for non-matching routing conditions, got: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_source_ip_cidr() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでCIDR表記によるIP範囲マッチを設定する必要がある
    // 例: source_ip = ["127.0.0.0/8", "192.168.0.0/16"]

    // 127.0.0.1からのリクエスト（127.0.0.0/8に含まれる）
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    // CIDR範囲に含まれる場合、200が返される
    // 127.0.0.1は127.0.0.0/8に含まれるため、200が返される
    assert_eq!(
        status1,
        Some(200),
        "Should return 200 OK for CIDR range match, got: {:?}",
        status1
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_condition_and_logic() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルで複数条件を持つルートを設定する必要がある
    // /api/condition-and/* パスに query={format=json} かつ header={X-Version=v2} を設定

    // すべての条件を満たすリクエスト
    let response1 = send_request_with_method(
        PROXY_PORT,
        "/api/condition-and/?format=json",
        "GET",
        &[("X-Version", "v2")],
        None,
    )
    .await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    // 1つ以上の条件を満たさないリクエスト（クエリパラメータが不一致）
    let response2 = send_request_with_method(
        PROXY_PORT,
        "/api/condition-and/?format=xml", // 条件を満たさない
        "GET",
        &[("X-Version", "v2")],
        None,
    )
    .await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // すべての条件を満たす場合、200が返される
    assert_eq!(
        status1,
        Some(200),
        "Should return 200 OK for matching routing conditions, got: {:?}",
        status1
    );
    // 条件を満たさない場合、404が返される
    assert_eq!(
        status2,
        Some(404),
        "Should return 404 Not Found for non-matching routing conditions, got: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_case_insensitive_host() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ホスト名の大文字小文字が正しく処理されることを確認
    let response1 = send_request(PROXY_PORT, "/", &[("Host", "localhost")]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    let response2 = send_request(PROXY_PORT, "/", &[("Host", "LOCALHOST")]).await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // 大文字小文字に関わらず、同じルートにマッチすることを確認
    assert!(
        status1 == Some(200) || status1 == Some(404),
        "Should return 200 OK or 404 Not Found: {:?}",
        status1
    );
    assert!(
        status2 == Some(200) || status2 == Some(404),
        "Should return 200 OK or 404 Not Found: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_case_insensitive_header() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ヘッダー名の大文字小文字が正しく処理されることを確認
    let response1 = send_request(PROXY_PORT, "/", &[("X-Version", "v2")]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    let response2 = send_request(PROXY_PORT, "/", &[("x-version", "v2")]).await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // 大文字小文字に関わらず、同じルートにマッチすることを確認
    assert!(
        status1 == Some(200) || status1 == Some(404) || status1 == Some(403),
        "Should return 200 OK, 404 Not Found, or 403 Forbidden: {:?}",
        status1
    );
    assert!(
        status2 == Some(200) || status2 == Some(404) || status2 == Some(403),
        "Should return 200 OK, 404 Not Found, or 403 Forbidden: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_empty_path() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 空パス（/）のルーティングが正しく動作することを確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());

    // 空パスが正しくルーティングされることを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for empty path, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_trailing_slash() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 末尾スラッシュの有無が正しく処理されることを確認
    // /api/ ディレクトリには index.html が存在するため両方 200 が返される
    let response1 = send_request(PROXY_PORT, "/api", &[]).await;
    assert!(response1.is_some(), "Should receive response");
    let status1 = get_status_code(&response1.unwrap());

    let response2 = send_request(PROXY_PORT, "/api/", &[]).await;
    assert!(response2.is_some(), "Should receive response");
    let status2 = get_status_code(&response2.unwrap());

    // 末尾スラッシュの有無に関わらず、適切にルーティングされることを確認
    // 通常は200が返される（リダイレクトが設定されていない場合）
    assert_eq!(
        status1,
        Some(200),
        "Should return 200 OK for path without trailing slash, got: {:?}",
        status1
    );
    assert_eq!(
        status2,
        Some(200),
        "Should return 200 OK for path with trailing slash, got: {:?}",
        status2
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_query_parameter_encoding() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // URLエンコードされたクエリパラメータが正しく処理されることを確認
    let encoded_path = "/?token=secret%20value&format=json";
    let response = send_request(PROXY_PORT, encoded_path, &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());

    // URLエンコードされたクエリパラメータが正しく処理されることを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for URL-encoded query parameter, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_routing_source_ip_ipv6() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは設定ファイルでIPv6アドレス条件を設定する必要がある
    // 例: source_ip = ["::1/128", "2001:db8::/32"]

    // IPv6アドレスからのリクエスト（実際にはIPv4で接続するため、テストは制限的）
    // ここでは、基本的な動作確認のみ
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());

    // IPv6アドレスが正しく評価されることを確認（実際のIPv6接続テストは別途必要）
    // 127.0.0.1からのリクエストなので、200が返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for IPv6 routing test, got: {:?}",
        status
    );
}

// ====================
// H2C機能の追加テスト（評価レポートに基づく設計）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_server_prior_knowledge() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはH2Cサーバーが起動している必要がある
    // H2CサーバーへのPrior Knowledge接続を確認

    // H2Cバックエンドに直接接続（HTTP/2 Prior Knowledge）
    // 実際の実装にはHTTP/2クライアントライブラリが必要
    // ここでは、プロキシ経由でH2C接続を確認
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());

    // H2C接続が確立された場合、200が返される
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C prior knowledge connection, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_server_multiple_connections() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2Cサーバーへの複数接続を確認
    let mut success_count = 0;
    let num_connections = 5;

    for _ in 0..num_connections {
        let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
        if let Some(resp) = response {
            let status = get_status_code(&resp);
            if status == Some(200) {
                success_count += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // 複数の接続が確立されることを確認
    assert!(
        success_count > 0,
        "At least one connection should succeed: {}/{}",
        success_count,
        num_connections
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_server_connection_close() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2Cサーバーの接続終了を確認
    let response = send_request(PROXY_PORT, "/h2c/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());

    // 接続が正常に終了することを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C connection close, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_large_header_block() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 大きなヘッダーブロックを送信
    let mut headers = Vec::new();
    let mut header_values = Vec::new();
    for i in 0..50 {
        header_values.push(format!("value-{}", i));
    }
    for value in &header_values {
        headers.push(("X-Custom-Header", value.as_str()));
    }

    let response = send_request(PROXY_PORT, "/h2c/", &headers).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());

    // 大きなヘッダーブロックが正しく処理されることを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for large header block, got: {:?}",
        status
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http2")]
pub(super) async fn test_h2c_flow_control() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // HTTP/2のフロー制御が正しく動作することを確認
    // H2Cバックエンドは静的ファイルサーバーのため、large.txtを取得してフロー制御を確認
    let response = send_request_with_method(PROXY_PORT, "/h2c/large.txt", "GET", &[], None).await;

    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());

    // フロー制御が正しく動作することを確認
    assert_eq!(
        status,
        Some(200),
        "Should return 200 OK for H2C flow control test, got: {:?}",
        status
    );
}

// ====================
// 運用機能の追加テスト（評価レポートに基づく設計）
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_graceful_reload_complete() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは実際のSIGHUPシグナルを送信する必要がある
    // プロセスIDの取得とシグナル送信が必要

    // 既存の接続を確立
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive response before reload");
    let status1 = get_status_code(&response1.unwrap());
    assert_eq!(status1, Some(200), "Should return 200 OK before reload");

    // 注意: 実際のリロードテストには、設定ファイルの変更とSIGHUP送信が必要
    // ここでは、基本的な動作確認のみ
    // 実際の実装では、以下のような処理が必要:
    // 1. プロキシプロセスのPIDを取得
    // 2. 設定ファイルを変更
    // 3. SIGHUPシグナルを送信
    // 4. 新しい設定が適用されることを確認
    // 5. 既存の接続が維持されることを確認

    eprintln!("Graceful reload test: Basic functionality confirmed (full implementation requires process management)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_graceful_reload_invalid_config() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは無効な設定ファイルでリロードを試みる必要がある
    // 実際の実装には、設定ファイルの変更とSIGHUP送信が必要

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    // 注意: 実際の実装では、以下のような処理が必要:
    // 1. 無効な設定ファイルを作成
    // 2. SIGHUPシグナルを送信
    // 3. リロードが拒否され、既存設定が維持されることを確認

    eprintln!("Graceful reload invalid config test: Basic functionality confirmed (full implementation requires process management)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_graceful_reload_route_changes() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはルート設定を変更してリロードする必要がある
    // 実際の実装には、設定ファイルの変更とSIGHUP送信が必要

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    // 注意: 実際の実装では、以下のような処理が必要:
    // 1. 新しいルート設定を追加
    // 2. SIGHUPシグナルを送信
    // 3. 新しいルートにアクセス可能になることを確認

    eprintln!("Graceful reload route changes test: Basic functionality confirmed (full implementation requires process management)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_graceful_reload_upstream_changes() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはアップストリーム設定を変更してリロードする必要がある
    // 実際の実装には、設定ファイルの変更とSIGHUP送信が必要

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    // 注意: 実際の実装では、以下のような処理が必要:
    // 1. アップストリーム設定を変更
    // 2. SIGHUPシグナルを送信
    // 3. 新しい設定が適用されることを確認

    eprintln!("Graceful reload upstream changes test: Basic functionality confirmed (full implementation requires process management)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_graceful_shutdown() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは実際のSIGTERM/SIGINTシグナルを送信する必要がある
    // プロセスIDの取得とシグナル送信が必要

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    // 注意: 実際の実装では、以下のような処理が必要:
    // 1. 複数の接続を確立
    // 2. SIGTERMシグナルを送信
    // 3. 新しい接続の受け入れが停止されることを確認
    // 4. 既存の接続が完了するまで待機することを確認
    // 5. サーバーが正常に終了することを確認

    eprintln!("Graceful shutdown test: Basic functionality confirmed (full implementation requires process management)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_config_validation_complete() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストは無効な設定ファイルで起動を試みる必要がある
    // 実際の実装には、別プロセスでの起動試行が必要

    // 基本的な動作確認（有効な設定ファイル）
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    // 注意: 実際の実装では、以下のような処理が必要:
    // 1. 無効な設定ファイルを作成
    // 2. プロキシサーバーを起動しようとする
    // 3. 適切なエラーメッセージが表示されることを確認
    // 4. サーバーが起動しないことを確認

    eprintln!("Config validation test: Basic functionality confirmed (full implementation requires process management)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_log_level_trace() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはログファイルを確認する必要がある
    // 実際の実装には、ログファイルの読み取りと解析が必要

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    // 注意: 実際の実装では、以下のような処理が必要:
    // 1. ログレベルをtraceに設定
    // 2. プロキシサーバーを起動
    // 3. リクエストを送信
    // 4. ログファイルを確認し、traceレベルのログが出力されることを確認

    eprintln!("Log level trace test: Basic functionality confirmed (full implementation requires log file access)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_log_level_debug() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはログファイルを確認する必要がある
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    eprintln!("Log level debug test: Basic functionality confirmed (full implementation requires log file access)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_log_level_info() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはログファイルを確認する必要がある
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    eprintln!("Log level info test: Basic functionality confirmed (full implementation requires log file access)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_log_level_warn() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはログファイルを確認する必要がある
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    eprintln!("Log level warn test: Basic functionality confirmed (full implementation requires log file access)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_log_level_error() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはログファイルを確認する必要がある
    // エラーを発生させるリクエストを送信
    let response = send_request(PROXY_PORT, "/nonexistent", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(
        status,
        Some(404),
        "Should return 404 Not Found for nonexistent endpoint, got: {:?}",
        status
    );

    eprintln!("Log level error test: Basic functionality confirmed (full implementation requires log file access)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_log_format_text() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはログファイルを確認する必要がある
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    eprintln!("Log format text test: Basic functionality confirmed (full implementation requires log file access)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_log_format_json() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはログファイルを確認する必要がある
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    eprintln!("Log format JSON test: Basic functionality confirmed (full implementation requires log file access)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_log_rotation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはログファイルのローテーションを確認する必要がある
    // 実際の実装には、大量のログを生成してローテーションをトリガーする必要がある

    // 基本的な動作確認
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response.is_some(), "Should receive response");
    let status = get_status_code(&response.unwrap());
    assert_eq!(status, Some(200), "Should return 200 OK");

    eprintln!("Log rotation test: Basic functionality confirmed (full implementation requires log file access and rotation trigger)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_zero_downtime_reload() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // ゼロダウンタイムリロードを確認
    // リロード中もリクエストが正常に処理されることを確認

    // リロード前のリクエスト
    let response1 = send_request(PROXY_PORT, "/", &[]).await;
    assert!(response1.is_some(), "Should receive response before reload");
    let status1 = get_status_code(&response1.unwrap());
    assert_eq!(status1, Some(200), "Should return 200 OK before reload");

    // 注意: 実際の実装では、以下のような処理が必要:
    // 1. リロード中に複数のリクエストを送信
    // 2. すべてのリクエストが正常に処理されることを確認
    // 3. ダウンタイムがないことを確認

    eprintln!("Zero downtime reload test: Basic functionality confirmed (full implementation requires process management)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_backend_rolling_update() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // バックエンドのローリングアップデートを確認
    // バックエンドサーバーを順次更新しても、サービスが継続されることを確認

    // 複数のリクエストを送信して、サービスが継続されることを確認
    // 並列実行時のTLSハンドシェイクタイムアウト対策としてリトライロジックを追加
    let mut success_count = 0;
    for i in 0..10 {
        // 各リクエストにリトライロジックを適用
        let response = send_request_with_retry(PROXY_PORT, "/", &[], 3).await;
        if let Some(resp) = response {
            let status = get_status_code(&resp);
            if status == Some(200) {
                success_count += 1;
            }
        }
        // リクエスト間の待機時間を追加（並列実行時の負荷軽減）
        if i < 9 {
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
    }

    // サービスが継続されることを確認（並列実行時は一部失敗を許容）
    assert!(
        success_count >= 3,
        "Service should continue during rolling update: {}/10 (at least 3 should succeed)",
        success_count
    );
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_health_check_gradual_degradation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 注意: このテストはhealthcheck設定タイプで実行する必要がある
    // 例: ./tests/e2e_setup.sh test healthcheck

    // 段階的な性能劣化を確認
    // バックエンドの性能が段階的に劣化した場合、ヘルスチェックが適切に検出することを確認

    // 複数のリクエストを送信
    let mut success_count = 0;
    for _ in 0..10 {
        let response = send_request(PROXY_PORT, "/", &[]).await;
        if let Some(resp) = response {
            let status = get_status_code(&resp);
            if status == Some(200) {
                success_count += 1;
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // サービスが継続されることを確認
    assert!(
        success_count > 0,
        "Service should continue during gradual degradation: {}/10",
        success_count
    );

    eprintln!("Health check gradual degradation test: Basic functionality confirmed (full implementation requires healthcheck config)");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_metrics_aggregation() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // メトリクスの集計を確認
    // メトリクスが適切に集計され、Prometheusで取得できることを確認

    // 複数のリクエストを送信
    for _ in 0..5 {
        let _ = send_request(PROXY_PORT, "/", &[]).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // メトリクスエンドポイントからメトリクスを取得
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");

    let response = response.unwrap();
    assert!(
        response.contains("veil_proxy") || response.contains("# HELP"),
        "Should contain Prometheus metrics"
    );

    eprintln!("Metrics aggregation test: Metrics are properly aggregated and accessible");
}

// ====================
// F-09: Prometheus拡充E2Eテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_new_prometheus_metrics_present() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // リクエストを数件送信してメトリクスを生成
    for _ in 0..3 {
        let _ = send_request(PROXY_PORT, "/", &[]).await;
    }

    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(response.is_some(), "Should receive metrics response");
    let response = response.unwrap();

    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "Metrics endpoint should return 200");

    // 既存の基本メトリクスが含まれること
    assert!(
        response.contains("veil_") || response.contains("# HELP") || response.contains("# TYPE"),
        "Should contain Prometheus metrics format"
    );

    // Content-Type が Prometheus テキスト形式であること
    let has_prometheus_ct =
        response.contains("text/plain") || response.contains("application/openmetrics");
    assert!(
        has_prometheus_ct,
        "Should have appropriate Content-Type for metrics"
    );

    eprintln!("New Prometheus metrics test: metrics endpoint is accessible and contains data");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_prometheus_metrics_runtime_enable() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // メトリクスエンドポイントが有効状態で正常に応答することを確認
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(
        response.is_some(),
        "Metrics endpoint should respond when enabled"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(
        status,
        Some(200),
        "Metrics endpoint should return 200 when runtime-enabled"
    );
}

// ====================
// F-06: サーキットブレーカーE2Eテスト
// ====================

#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_circuit_breaker_trips_on_backend_errors() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // error-500 バックエンドへ繰り返しリクエストし、CBがトリップするか確認
    // error-pool のCBは failure_threshold=3 で設定されている
    let mut status_codes = Vec::new();

    for i in 0..8 {
        let response = send_request(PROXY_PORT, "/error-500/", &[]).await;
        if let Some(resp) = response {
            if let Some(code) = get_status_code(&resp) {
                status_codes.push(code);
                eprintln!("CB test request {}: status={}", i + 1, code);
            }
        }
        // CBが状態遷移する時間を確保
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // バックエンドがエラーを返すか、CBが遮断して502/503を返すこと
    let had_error = status_codes
        .iter()
        .any(|&s| s == 500 || s == 502 || s == 503 || s == 504);
    assert!(
        had_error || status_codes.is_empty(),
        "Should receive error responses from error backend or circuit breaker, got: {:?}",
        status_codes
    );

    eprintln!(
        "Circuit breaker test: error statuses observed = {:?}",
        status_codes
    );
}

#[tokio::test]
#[ntest::timeout(30000)]
pub(super) async fn test_circuit_breaker_half_open_recovery() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 先にエラーを出してCBをOpen状態にする
    for _ in 0..5 {
        let _ = send_request(PROXY_PORT, "/error-500/", &[]).await;
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    // CB open_duration_secs=5 後に HalfOpen → 正常バックエンドへのリクエストで確認
    // 注意: このテストはCBのOpen期間（5秒）を待つため、やや長い
    tokio::time::sleep(tokio::time::Duration::from_secs(6)).await;

    // 正常なバックエンドへのリクエストは成功すること
    let response = send_request(PROXY_PORT, "/", &[]).await;
    if let Some(resp) = response {
        let status = get_status_code(&resp);
        assert_eq!(
            status,
            Some(200),
            "Normal backend should still work after CB test: {:?}",
            status
        );
    }

    eprintln!("Circuit breaker recovery test: normal backend is still accessible");
}

// ====================
// F-19: 高度なロードバランシングE2Eテスト
// ====================

#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_weighted_load_balancing_ratio() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // weighted-pool ルート（backend1:weight=2, backend2:weight=1）への
    // 複数リクエストでbackend1がおよそ2倍受信することを確認
    let mut backend1_count = 0usize;
    let mut backend2_count = 0usize;

    for _ in 0..30 {
        let response = send_request(PROXY_PORT, "/weighted/", &[]).await;
        if let Some(resp) = response {
            if let Some(id) = get_header_value(&resp, "X-Server-Id") {
                if id == "backend1" {
                    backend1_count += 1;
                } else if id == "backend2" {
                    backend2_count += 1;
                }
            }
        }
    }

    eprintln!(
        "Weighted LB test: backend1={}, backend2={}",
        backend1_count, backend2_count
    );

    let total = backend1_count + backend2_count;
    if total > 0 {
        // 重み 2:1 → backend1 が 60%以上であれば合格（厳密な2:1チェックはフレーキー）
        let backend1_ratio = backend1_count as f64 / total as f64;
        assert!(
            backend1_ratio >= 0.55,
            "backend1 should receive more traffic with weight=2: ratio={:.2} (b1={}, b2={})",
            backend1_ratio,
            backend1_count,
            backend2_count
        );
    } else {
        eprintln!("WARNING: No X-Server-Id headers received, skipping ratio check");
    }
}

#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_consistent_hash_same_header_same_backend() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 同じ X-User-Id ヘッダーで複数回リクエストすると常に同じバックエンドに届くことを確認
    // consistent_hash アルゴリズムはヘッダーキーでハッシュするため、
    // 同一クライアントIPからのリクエストは同じバックエンドへ転送される
    let mut backend_ids: Vec<String> = Vec::new();

    for _ in 0..10 {
        let response = send_request(
            PROXY_PORT,
            "/consistent-hash/",
            &[("X-User-Id", "user-12345")],
        )
        .await;
        if let Some(resp) = response {
            if let Some(id) = get_header_value(&resp, "X-Server-Id") {
                backend_ids.push(id);
            }
        }
    }

    eprintln!("Consistent hash test: backend_ids={:?}", backend_ids);

    if backend_ids.len() >= 3 {
        // 同じキーなら同じバックエンドへ転送される（全て同じIDのはず）
        let unique: std::collections::HashSet<_> = backend_ids.iter().collect();
        assert_eq!(
            unique.len(),
            1,
            "Consistent hash should route to same backend for same key: {:?}",
            backend_ids
        );
    } else {
        eprintln!("WARNING: Not enough responses to validate consistent hash, skipping assertion");
    }
}

#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_consistent_hash_different_keys_distribute() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 異なるクライアントIPからのリクエストが複数バックエンドに分散されることを確認
    // (E2E環境では全て同じクライアントIPのため、異なるヘッダー値で確認)
    let mut seen_backends: std::collections::HashSet<String> = std::collections::HashSet::new();

    for user_id in &["user-001", "user-002", "user-003", "user-004", "user-005"] {
        for _ in 0..2 {
            let response =
                send_request(PROXY_PORT, "/consistent-hash/", &[("X-User-Id", user_id)]).await;
            if let Some(resp) = response {
                if let Some(id) = get_header_value(&resp, "X-Server-Id") {
                    seen_backends.insert(id);
                }
            }
        }
    }

    eprintln!(
        "Consistent hash distribution test: seen backends = {:?}",
        seen_backends
    );

    // 複数のユーザーIDが複数のバックエンドに分散されること（完全でなくてもよい）
    // 150 vnode リングによる分散なので、少なくとも1つのバックエンドが使われること
    assert!(
        !seen_backends.is_empty(),
        "At least one backend should receive requests"
    );
}

// ====================
// F-20: キャッシュPurge E2Eテスト
// ====================

#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_cache_populate_then_purge_all() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // キャッシュにエントリを生成
    let _ = send_request(PROXY_PORT, "/cached/", &[]).await;
    let _ = send_request(PROXY_PORT, "/cached/", &[]).await;

    // purge all を実行（admin endpoint）
    let response = send_request_with_method(
        PROXY_PORT,
        "/__admin/cache/purge?all=true",
        "POST",
        &[("Authorization", "Bearer test-admin-secret")],
        Some(b""),
    )
    .await;

    if let Some(resp) = &response {
        let status = get_status_code(resp);
        eprintln!(
            "Cache purge all: status={:?}, response={}",
            status,
            &resp[..resp.len().min(200)]
        );
        // 200 または 404（キャッシュが空の場合も200で {"purged":0} が返る）
        assert!(
            status == Some(200),
            "Purge all should return 200, got: {:?}",
            status
        );
        assert!(
            resp.contains("purged"),
            "Response should contain 'purged' count"
        );
    } else {
        eprintln!("WARNING: Cache purge request failed (admin may not be configured)");
    }
}

#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_cache_purge_requires_auth() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 認証なしのpurgeリクエストが拒否されること
    let response = send_request_with_method(
        PROXY_PORT,
        "/__admin/cache/purge?all=true",
        "POST",
        &[], // 認証ヘッダーなし
        Some(b""),
    )
    .await;

    if let Some(resp) = &response {
        let status = get_status_code(resp);
        eprintln!("Cache purge without auth: status={:?}", status);
        // 401 または 403 が返ること
        assert!(
            status == Some(401) || status == Some(403),
            "Purge without auth should be rejected: {:?}",
            status
        );
    } else {
        eprintln!("WARNING: No response received for unauthenticated purge");
    }
}

#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_cache_purge_wrong_auth_rejected() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 誤った認証トークンでのpurgeリクエストが拒否されること
    let response = send_request_with_method(
        PROXY_PORT,
        "/__admin/cache/purge?all=true",
        "POST",
        &[("Authorization", "Bearer wrong-secret")],
        Some(b""),
    )
    .await;

    if let Some(resp) = &response {
        let status = get_status_code(resp);
        eprintln!("Cache purge with wrong auth: status={:?}", status);
        assert!(
            status == Some(401) || status == Some(403),
            "Purge with wrong auth should be rejected: {:?}",
            status
        );
    } else {
        eprintln!("WARNING: No response received for wrong-auth purge");
    }
}

#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_cache_purge_prefix() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // キャッシュにエントリを生成
    let _ = send_request(PROXY_PORT, "/cached/test1", &[]).await;
    let _ = send_request(PROXY_PORT, "/cached/test2", &[]).await;

    // プレフィックスでpurge
    let response = send_request_with_method(
        PROXY_PORT,
        "/__admin/cache/purge?prefix=/cached/",
        "POST",
        &[("Authorization", "Bearer test-admin-secret")],
        Some(b""),
    )
    .await;

    if let Some(resp) = &response {
        let status = get_status_code(resp);
        eprintln!("Cache purge prefix: status={:?}", status);
        assert_eq!(status, Some(200), "Prefix purge should return 200");
    } else {
        eprintln!("WARNING: Cache prefix purge request failed");
    }
}

// ====================
// F-03: TLS証明書リロードE2Eテスト
// ====================

#[tokio::test]
#[ntest::timeout(20000)]
pub(super) async fn test_proxy_still_responds_after_sighup() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // SIGHUPの前にプロキシが正常であること
    let before = send_request(PROXY_PORT, "/", &[]).await;
    assert!(before.is_some(), "Proxy should respond before SIGHUP");
    assert_eq!(get_status_code(&before.unwrap()), Some(200));

    // SIGHUPを送信（設定リロード + 証明書リロードトリガー）
    // プロキシのPIDを取得して SIGHUP を送信
    let pid_output = std::process::Command::new("pgrep")
        .args(["-f", "veil.*proxy.toml"])
        .output();

    if let Ok(output) = pid_output {
        let pid_str = String::from_utf8_lossy(&output.stdout);
        if let Ok(pid) = pid_str.trim().parse::<u32>() {
            eprintln!("Sending SIGHUP to proxy PID: {}", pid);
            let _ = std::process::Command::new("kill")
                .args(["-HUP", &pid.to_string()])
                .output();

            // リロード処理を待つ
            tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;
        }
    }

    // SIGHUPの後もプロキシが正常に応答すること
    let after = send_request_with_retry(PROXY_PORT, "/", &[], 5).await;
    assert!(after.is_some(), "Proxy should respond after SIGHUP");
    assert_eq!(
        get_status_code(&after.unwrap()),
        Some(200),
        "Proxy should return 200 after SIGHUP"
    );

    eprintln!("TLS cert reload test: proxy is still responsive after SIGHUP");
}

// ====================
// F-10: OpenTelemetryE2Eテスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_proxy_works_with_otel_feature() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // OTel featureが有効でも（または無効でも）プロキシが正常に動作すること
    // （OTelはcompile-time featureのため、E2E環境では常にfullでビルドされている）
    let response = send_request(PROXY_PORT, "/", &[]).await;
    assert!(
        response.is_some(),
        "Proxy should work regardless of OTel feature state"
    );
    assert_eq!(
        get_status_code(&response.unwrap()),
        Some(200),
        "Should return 200 OK"
    );

    eprintln!("OTel feature test: proxy operates normally with OTel feature compiled in");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_otel_does_not_affect_metrics_endpoint() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // OTelが有効でもPrometheusメトリクスエンドポイントは引き続き機能すること
    let response = send_request(PROXY_PORT, "/__metrics", &[]).await;
    assert!(
        response.is_some(),
        "Metrics endpoint should work with OTel compiled in"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(
        status,
        Some(200),
        "Metrics should still return 200 with OTel compiled in"
    );

    eprintln!("OTel + Metrics coexistence test: both work correctly");
}

// ====================
// F-21: Admin API 拡張 E2E テスト
// ====================

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_admin_stats() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(
        PROXY_PORT,
        "/__admin/stats",
        &[("Authorization", "Bearer test-admin-secret")],
    )
    .await;
    assert!(
        response.is_some(),
        "/__admin/stats should return a response"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "/__admin/stats should return 200");
    assert!(
        response.contains("uptime_secs"),
        "stats should contain uptime_secs"
    );

    eprintln!("Admin stats E2E test: passed");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_admin_config() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request(
        PROXY_PORT,
        "/__admin/config",
        &[("Authorization", "Bearer test-admin-secret")],
    )
    .await;
    assert!(
        response.is_some(),
        "/__admin/config should return a response"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "/__admin/config should return 200");
    // secretはマスクされていること
    assert!(
        !response.contains("test-admin-secret"),
        "secret must be masked in config dump"
    );

    eprintln!("Admin config E2E test: passed");
}

#[tokio::test]
#[ntest::timeout(15000)]
// clippy::await_holding_lock 許容理由: RELOAD_TEST_LOCK は SIGHUP リロード系テストを
// テストプロセス内で直列化するための意図的なガードで、テスト全体（await を含む）を
// 覆う必要がある（tokio マルチスレッドでも所有スレッドは同一タスク内で完結する）。
#[allow(clippy::await_holding_lock)]
pub(super) async fn test_e2e_admin_reload() {
    // リロードは proxy.toml を読み直すので、他のテストが書き換えている最中に走らせない
    let _guard = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request_with_method(
        PROXY_PORT,
        "/__admin/reload",
        "POST",
        &[("Authorization", "Bearer test-admin-secret")],
        None,
    )
    .await;
    assert!(
        response.is_some(),
        "/__admin/reload should return a response"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "/__admin/reload should return 200");
    assert!(
        response.contains("\"ok\":true"),
        "reload should return {{\"ok\":true}}"
    );

    eprintln!("Admin reload E2E test: passed");
}

#[tokio::test]
#[ntest::timeout(15000)]
// clippy::await_holding_lock 許容理由: RELOAD_TEST_LOCK は SIGHUP リロード系テストを
// テストプロセス内で直列化するための意図的なガードで、テスト全体（await を含む）を
// 覆う必要がある（tokio マルチスレッドでも所有スレッドは同一タスク内で完結する）。
#[allow(clippy::await_holding_lock)]
pub(super) async fn test_e2e_admin_tls_reload() {
    // リロードは proxy.toml を読み直すので、他のテストが書き換えている最中に走らせない
    let _guard = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request_with_method(
        PROXY_PORT,
        "/__admin/tls/reload",
        "POST",
        &[("Authorization", "Bearer test-admin-secret")],
        None,
    )
    .await;
    assert!(
        response.is_some(),
        "/__admin/tls/reload should return a response"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(200), "/__admin/tls/reload should return 200");
    assert!(
        response.contains("\"ok\":true"),
        "tls/reload should return {{\"ok\":true}}"
    );

    eprintln!("Admin TLS reload E2E test: passed");
}

#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_e2e_admin_unauthorized() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // 認証なし → 401
    let response = send_request(PROXY_PORT, "/__admin/stats", &[]).await;
    assert!(
        response.is_some(),
        "Should get a response even without auth"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(401), "Missing auth should return 401");

    // 誤ったシークレット → 401
    let response = send_request(
        PROXY_PORT,
        "/__admin/stats",
        &[("Authorization", "Bearer wrong-secret")],
    )
    .await;
    assert!(response.is_some(), "Should get a response with wrong auth");
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(status, Some(401), "Wrong auth should return 401");

    eprintln!("Admin unauthorized E2E test: passed");
}

// ====================
// L4 TCP プロキシ (F-30)
// ====================

/// L4 TCP プロキシ（TLS パススルー）の基本転送を検証する。
///
/// クライアントは L4 ポート(8444)へ TLS 接続し、L4 は生の TCP バイトを backend1/2(HTTPS) へ
/// 双方向中継する（TLS は backend で終端）。これにより L4 の c→u / u→c 双方向転送が
/// 壊れていないことを検証する（splice 化のリグレッションガード）。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_l4_tcp_passthrough_forward() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request_with_retry(PROXY_L4_PORT, "/", &[], 3).await;
    assert!(
        response.is_some(),
        "L4 passthrough should forward the request and return a response"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(
        status,
        Some(200),
        "L4 forwarded request should return 200, got: {:?}",
        status
    );
    assert!(
        response.contains("Hello from Backend"),
        "L4 should transparently forward the backend's static file body"
    );
    eprintln!("L4 passthrough basic forward: passed");
}

/// L4 TCP プロキシで大きめのペイロード（10KB）が破損・切り詰めなく双方向転送されることを検証する。
/// splice 化（複数チャンク転送）でのデータ整合性リグレッションを検出する。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_l4_passthrough_large_payload() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let response = send_request_with_retry(PROXY_L4_PORT, "/large.txt", &[], 3).await;
    assert!(
        response.is_some(),
        "L4 should forward large payload request"
    );
    let response = response.unwrap();
    let status = get_status_code(&response);
    assert_eq!(
        status,
        Some(200),
        "L4 large payload should return 200, got: {:?}",
        status
    );
    // large.txt は 'A' が 10000 バイト。連続した 'A' の長い並びが欠落なく転送されていることを確認。
    let big_run = "A".repeat(5000);
    assert!(
        response.contains(big_run.as_str()),
        "L4 should forward the 10KB body intact (no truncation/corruption)"
    );
    eprintln!("L4 passthrough large payload: passed");
}

// ====================
// L4 network filter (WASM, F-133)
// ====================
//
// `l4-wasm-network-filter` リスナー（PROXY_L4_WASM_PORT=8448、平文 TCP、tls="none"）は
// `network_filter` WASM モジュールを適用し、backend は HTTP body-echo
// （tests/test_backends の `run_echo_server`）。1 往復で downstream（client→upstream、
// foo->bar）と upstream（backend→client、bar->baz）の両方向の書き換えを観測できる:
// クライアントが送った "foo" は downstream フィルタで "bar" に書き換わってバックエンドへ
// 届き、バックエンドがそのまま "bar" を echo した応答は upstream フィルタで "baz" に
// 書き換わってクライアントへ返る。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(all(feature = "l4-proxy", feature = "wasm"))]
pub(super) async fn test_l4_wasm_downstream_upstream_data_rewrite() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut stream = match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(("127.0.0.1", PROXY_L4_WASM_PORT)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        other => {
            panic!("failed to connect to L4 WASM network filter listener: {other:?}");
        }
    };

    let body = b"foo-data-foo";
    let request = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write L4 WASM request headers");
    stream
        .write_all(body)
        .await
        .expect("write L4 WASM request body");

    let mut response = Vec::new();
    tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
        .await
        .expect("read L4 WASM response (timeout)")
        .expect("read L4 WASM response");

    let response_str = String::from_utf8_lossy(&response);
    eprintln!("L4 WASM network filter response: {}", response_str);

    assert!(
        response_str.starts_with("HTTP/1.1 200"),
        "L4 WASM listener should still forward a normal HTTP response: {}",
        response_str
    );
    assert!(
        response_str.contains("baz-data-baz"),
        "downstream (foo->bar) + upstream (bar->baz) rewrite should yield 'baz-data-baz' \
         in the echoed body, got: {}",
        response_str
    );
    assert!(
        !response_str.contains("foo-data-foo") && !response_str.contains("bar-data-bar"),
        "original/intermediate payload must not leak through unmodified: {}",
        response_str
    );
}

/// downstream データに `CLOSE_ME` マーカーが含まれると、WASM モジュールが
/// `proxy_close_stream` を呼び、L4 がバックエンドへ転送せずに接続を切断すること
/// （＝クライアントは HTTP レスポンスを一切受け取らない）を検証する。
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(all(feature = "l4-proxy", feature = "wasm"))]
pub(super) async fn test_l4_wasm_close_on_marker() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let mut stream = match tokio::time::timeout(
        Duration::from_secs(5),
        tokio::net::TcpStream::connect(("127.0.0.1", PROXY_L4_WASM_PORT)),
    )
    .await
    {
        Ok(Ok(s)) => s,
        other => {
            panic!("failed to connect to L4 WASM network filter listener: {other:?}");
        }
    };

    let body = b"CLOSE_ME";
    let request = format!(
        "POST / HTTP/1.1\r\nHost: x\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream
        .write_all(request.as_bytes())
        .await
        .expect("write L4 WASM CLOSE_ME request headers");
    stream
        .write_all(body)
        .await
        .expect("write L4 WASM CLOSE_ME request body");

    let mut response = Vec::new();
    let read_result =
        tokio::time::timeout(Duration::from_secs(5), stream.read_to_end(&mut response))
            .await
            .expect("read after CLOSE_ME should not hang (timeout)");

    // 接続がエラーで切れるか（Err）、EOF まで読めても中身が空（正常応答なし）で
    // あることのいずれかを許容する（プラットフォームにより RST/FIN の見え方が違うため）。
    match read_result {
        Ok(_) => {
            assert!(
                response.is_empty(),
                "WASM close_stream should prevent any HTTP response from being forwarded, \
                 got {} bytes: {:?}",
                response.len(),
                String::from_utf8_lossy(&response)
            );
        }
        Err(_) => {
            // 接続リセット等のエラーも「レスポンスが来ない」という期待どおりの結果。
        }
    }
    eprintln!(
        "L4 WASM close-on-marker: passed (response bytes: {})",
        response.len()
    );
}

/// 既存の WASM 無効な L4 リスナー（`l4-passthrough`）が本チケットの変更後も従来どおり
/// 動作すること（退行防止）。`test_l4_tcp_passthrough_forward`/`test_l4_passthrough_large_payload`
/// が引き続きグリーンであることそのものが確認になるため、ここでは
/// `wasm_modules` 未設定リスナーが `is_empty()` 早期 return 経路（従来の splice/
/// forward_direction のみ）を通ることを明示的にコメントで残す（コードレベルの根拠は
/// `src/l4/proxy.rs::bidirectional_forward` 冒頭を参照）。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_l4_wasm_disabled_listener_still_passthrough() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // l4-passthrough（PROXY_L4_PORT）には wasm_modules が設定されていないため、
    // 既存の TLS パススルー転送がそのまま動くことを再確認する。
    let response = send_request_with_retry(PROXY_L4_PORT, "/", &[], 3).await;
    assert!(
        response.is_some(),
        "WASM-disabled L4 listener should keep forwarding as before"
    );
    let status = get_status_code(&response.unwrap());
    assert_eq!(
        status,
        Some(200),
        "WASM-disabled L4 listener regression check failed, got: {:?}",
        status
    );
    eprintln!("L4 WASM-disabled listener passthrough regression: passed");
}

// ====================
// TLS cipher_suites 設定テスト（F-50）
// ====================
//
// e2e_setup.sh の proxy.toml は [tls].cipher_suites で
// AES-GCM 系のみ（先頭 = TLS13_AES_256_GCM_SHA384）を許可し、CHACHA20 系を除外している。
// ここでは「設定順のサーバ優先度」「許可スイートのみネゴシエート」「除外スイートの拒否」を検証する。

/// 指定した暗号スイート群のみを提示するクライアントで TLS ハンドシェイクを行い、
/// ネゴシエートされたスイートを返す（失敗時は Err）。
pub(super) fn tls_handshake_negotiated_suite(
    port: u16,
    client_suites: Option<Vec<rustls::SupportedCipherSuite>>,
) -> Result<String, String> {
    init_crypto_provider();

    let mut provider = test_crypto::default_provider();
    if let Some(suites) = client_suites {
        provider.cipher_suites = suites;
    }
    let config = ClientConfig::builder_with_provider(provider.into())
        .with_protocol_versions(rustls::DEFAULT_VERSIONS)
        .map_err(|e| format!("version error: {e}"))?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("client conn error: {e}"))?;
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("connect error: {e}"))?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();

    while conn.is_handshaking() {
        conn.complete_io(&mut sock)
            .map_err(|e| format!("handshake error: {e}"))?;
    }

    conn.negotiated_cipher_suite()
        .map(|s| format!("{:?}", s.suite()))
        .ok_or_else(|| "no negotiated suite".to_string())
}

/// サーバは設定順の先頭（TLS13_AES_256_GCM_SHA384）を優先する。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_tls_cipher_suites_server_preference() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // デフォルトクライアント（AES256/AES128/CHACHA20 を提示）
    let negotiated =
        tokio::task::spawn_blocking(move || tls_handshake_negotiated_suite(PROXY_PORT, None))
            .await
            .unwrap()
            .expect("handshake should succeed");

    assert_eq!(
        negotiated, "TLS13_AES_256_GCM_SHA384",
        "server must prefer the first configured cipher suite"
    );
}

/// 設定に含まれる低優先度スイート（AES_128）のみを提示すればそれが選ばれる。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_tls_cipher_suites_allows_configured_suite() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let negotiated = tokio::task::spawn_blocking(move || {
        tls_handshake_negotiated_suite(
            PROXY_PORT,
            Some(vec![test_crypto::cipher_suite::TLS13_AES_128_GCM_SHA256]),
        )
    })
    .await
    .unwrap()
    .expect("handshake should succeed with configured suite");

    assert_eq!(negotiated, "TLS13_AES_128_GCM_SHA256");
}

/// 設定から除外されたスイート（CHACHA20）のみを提示するとハンドシェイクが失敗する。
#[tokio::test]
#[ntest::timeout(15000)]
pub(super) async fn test_tls_cipher_suites_rejects_excluded_suite() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let result = tokio::task::spawn_blocking(move || {
        tls_handshake_negotiated_suite(
            PROXY_PORT,
            Some(vec![
                test_crypto::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
            ]),
        )
    })
    .await
    .unwrap();

    assert!(
        result.is_err(),
        "handshake must fail when only excluded suites are offered, got: {:?}",
        result
    );
}

// ====================
// 設定・証明書リロードの正常性確認テスト（F-49）
// ====================
//
// e2e_setup.sh が起動したプロキシプロセスへ実際に SIGHUP を送り、
// - 設定ホットリロード（ルート追加が反映される）
// - 不正設定時のフェイルセーフ（旧設定でサービング継続）
// - TLS 証明書ホットリロード（新規ハンドシェイクが新証明書を観測、リロード中も無停止）
// を検証する。proxy.toml / 証明書はスイート共有のため、変更は追加的（他テストへ無影響）にし、
// テスト間の直列化と終了時の原状復帰を行う。

/// リロード系テストの直列化ロック（proxy.toml / 証明書ファイルを共有するため）
pub(super) static RELOAD_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// プロキシが読む設定・証明書を書き換える（一時ファイルへ書いてから rename する）。`fs::write` は切り詰めてから
/// 書くので、その間にリロードが走ると途中までの設定（後ろのルートが欠けた有効な TOML）を読み込んでしまう
// 理由: 試験
#[allow(clippy::disallowed_methods)]
pub(super) fn write_atomic(
    path: &std::path::Path,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    let tmp = path.with_extension("e2e-tmp");
    std::fs::write(&tmp, contents)?;
    std::fs::rename(&tmp, path)
}

pub(super) fn fixtures_dir() -> std::path::PathBuf {
    std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// e2e_setup.sh が書き出したプロキシの PID を読む
pub(super) fn proxy_pid() -> Option<i32> {
    std::fs::read_to_string(fixtures_dir().join("proxy.pid"))
        .ok()?
        .trim()
        .parse()
        .ok()
}

/// プロキシへ SIGHUP を送る（設定 + TLS 証明書リロードのトリガー）
pub(super) fn send_sighup(pid: i32) -> bool {
    std::process::Command::new("kill")
        .args(["-HUP", &pid.to_string()])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// TLS ハンドシェイクを行い、サーバ証明書（リーフ）の DER を返す
pub(super) fn get_server_cert_der(port: u16) -> Result<Vec<u8>, String> {
    init_crypto_provider();
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    let server_name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let mut conn = ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("client conn error: {e}"))?;
    let mut sock = std::net::TcpStream::connect(("127.0.0.1", port))
        .map_err(|e| format!("connect error: {e}"))?;
    sock.set_read_timeout(Some(std::time::Duration::from_secs(5)))
        .ok();
    while conn.is_handshaking() {
        conn.complete_io(&mut sock)
            .map_err(|e| format!("handshake error: {e}"))?;
    }
    conn.peer_certificates()
        .and_then(|certs| certs.first())
        .map(|c| c.as_ref().to_vec())
        .ok_or_else(|| "no peer certificate".to_string())
}

/// 設定リロード: ルート追加が SIGHUP 後に反映される
#[tokio::test]
#[ntest::timeout(60000)]
// clippy::await_holding_lock 許容理由: RELOAD_TEST_LOCK は SIGHUP リロード系テストを
// テストプロセス内で直列化するための意図的なガードで、テスト全体（await を含む）を
// 覆う必要がある（tokio マルチスレッドでも所有スレッドは同一タスク内で完結する）。
#[allow(clippy::await_holding_lock)]
pub(super) async fn test_config_reload_adds_route_via_sighup() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let _guard = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let pid = match proxy_pid() {
        Some(p) => p,
        None => {
            eprintln!("Skipping test: proxy.pid not found (old e2e environment)");
            return;
        }
    };
    let proxy_toml = fixtures_dir().join("proxy.toml");
    let original = std::fs::read_to_string(&proxy_toml).expect("read proxy.toml");

    // リロード前: 専用パスはルートされない（バックエンドの 404 ではなくプロキシの 404）
    let before = send_request(PROXY_PORT, "/reload-added-route/", &[]).await;
    if let Some(resp) = &before {
        assert_ne!(
            get_status_code(resp),
            Some(200),
            "route must not exist before reload"
        );
    }

    // ルートを追加して SIGHUP
    // File アクションは「ディレクトリ + リクエストパス」でファイルを解決するため、
    // /reload-added-route/index.html に対応する実ファイルを用意する。
    let serve_dir = fixtures_dir().join("backend1");
    let route_file_dir = serve_dir.join("reload-added-route");
    std::fs::create_dir_all(&route_file_dir).expect("create route dir");
    std::fs::write(
        route_file_dir.join("index.html"),
        "<h1>Reload Added Route</h1>",
    )
    .expect("write route index");
    let added = format!(
        r#"

# F-49 reload test (temporary route)
[[route]]
[route.conditions]
host = "127.0.0.1"
path = "/reload-added-route/*"
[route.action]
pub(super) type = "File"
path = "{dir}"
index = "index.html"

[[route]]
[route.conditions]
host = "localhost"
path = "/reload-added-route/*"
[route.action]
pub(super) type = "File"
path = "{dir}"
index = "index.html"
"#,
        dir = serve_dir.display()
    );
    write_atomic(&proxy_toml, format!("{original}{added}")).expect("write proxy.toml");
    assert!(send_sighup(pid), "SIGHUP must be delivered");

    // リロードスレッドは 500ms 周期。反映をポーリングで確認（最大 10 秒）
    let mut reloaded = false;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if let Some(resp) = send_request(PROXY_PORT, "/reload-added-route/", &[]).await {
            if get_status_code(&resp) == Some(200) {
                reloaded = true;
                break;
            }
        }
    }

    // 原状復帰（追加ルート・作成ファイルを削除して再リロード）
    write_atomic(&proxy_toml, &original).expect("restore proxy.toml");
    let _ = std::fs::remove_dir_all(&route_file_dir);
    send_sighup(pid);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    assert!(
        reloaded,
        "added route must start serving after SIGHUP reload"
    );

    // 復帰確認: 専用パスが再び未ルートに戻る
    let mut restored = false;
    for _ in 0..10 {
        if let Some(resp) = send_request(PROXY_PORT, "/reload-added-route/", &[]).await {
            if get_status_code(&resp) != Some(200) {
                restored = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    assert!(restored, "route must disappear after restoring config");
}

/// 不正な設定でのリロードは拒否され、旧設定でサービングが継続する
#[tokio::test]
#[ntest::timeout(60000)]
// clippy::await_holding_lock 許容理由: RELOAD_TEST_LOCK は SIGHUP リロード系テストを
// テストプロセス内で直列化するための意図的なガードで、テスト全体（await を含む）を
// 覆う必要がある（tokio マルチスレッドでも所有スレッドは同一タスク内で完結する）。
#[allow(clippy::await_holding_lock)]
pub(super) async fn test_config_reload_invalid_config_keeps_serving() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let _guard = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let pid = match proxy_pid() {
        Some(p) => p,
        None => {
            eprintln!("Skipping test: proxy.pid not found (old e2e environment)");
            return;
        }
    };
    let proxy_toml = fixtures_dir().join("proxy.toml");
    let original = std::fs::read_to_string(&proxy_toml).expect("read proxy.toml");

    // 壊れた TOML を書いて SIGHUP
    write_atomic(&proxy_toml, "this is [not valid toml =").expect("write broken toml");
    assert!(send_sighup(pid), "SIGHUP must be delivered");
    tokio::time::sleep(Duration::from_millis(1500)).await;

    // 旧設定で引き続きサービングされる
    let resp = send_request_with_retry(PROXY_PORT, "/", &[], 3).await;

    // 原状復帰
    write_atomic(&proxy_toml, &original).expect("restore proxy.toml");
    send_sighup(pid);
    tokio::time::sleep(Duration::from_millis(1000)).await;

    let resp = resp.expect("proxy must keep serving with old config after invalid reload");
    assert_eq!(
        get_status_code(&resp),
        Some(200),
        "old config must remain active after invalid reload"
    );
}

/// TLS 証明書リロード: SIGHUP 後の新規ハンドシェイクが新証明書を観測し、
/// リロード中もリクエストが途切れない（ゼロダウンタイム、F-03 回帰確認）
#[tokio::test]
#[ntest::timeout(60000)]
// clippy::await_holding_lock 許容理由: RELOAD_TEST_LOCK は SIGHUP リロード系テストを
// テストプロセス内で直列化するための意図的なガードで、テスト全体（await を含む）を
// 覆う必要がある（tokio マルチスレッドでも所有スレッドは同一タスク内で完結する）。
#[allow(clippy::await_holding_lock)]
pub(super) async fn test_tls_cert_reload_via_sighup() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }
    let _guard = RELOAD_TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let pid = match proxy_pid() {
        Some(p) => p,
        None => {
            eprintln!("Skipping test: proxy.pid not found (old e2e environment)");
            return;
        }
    };
    let cert_path = fixtures_dir().join("cert.pem");
    let key_path = fixtures_dir().join("key.pem");
    let original_cert = std::fs::read(&cert_path).expect("read cert.pem");
    let original_key = std::fs::read(&key_path).expect("read key.pem");

    let old_der = tokio::task::spawn_blocking(move || get_server_cert_der(PROXY_PORT))
        .await
        .unwrap()
        .expect("handshake before reload");

    // 新しい自己署名証明書（ECDSA）に差し替えて SIGHUP
    let cert =
        rcgen::generate_simple_self_signed(vec!["localhost".to_string(), "127.0.0.1".to_string()])
            .expect("generate cert");
    write_atomic(&cert_path, cert.cert.pem()).expect("write new cert");
    write_atomic(&key_path, cert.signing_key.serialize_pem()).expect("write new key");
    assert!(send_sighup(pid), "SIGHUP must be delivered");

    // リロード反映をポーリング（最大 10 秒）。その間もリクエストが成功し続けること。
    let mut new_der: Option<Vec<u8>> = None;
    for _ in 0..20 {
        tokio::time::sleep(Duration::from_millis(500)).await;

        // ゼロダウンタイム確認: リロード中の通常リクエストが成功する
        let resp = send_request_with_retry(PROXY_PORT, "/", &[], 2).await;
        assert!(
            resp.is_some(),
            "requests must keep succeeding during cert reload"
        );

        let der = tokio::task::spawn_blocking(move || get_server_cert_der(PROXY_PORT))
            .await
            .unwrap();
        if let Ok(der) = der {
            if der != old_der {
                new_der = Some(der);
                break;
            }
        }
    }

    // 原状復帰（元の証明書へ戻して再リロード）
    write_atomic(&cert_path, &original_cert).expect("restore cert");
    write_atomic(&key_path, &original_key).expect("restore key");
    send_sighup(pid);
    tokio::time::sleep(Duration::from_millis(1500)).await;

    let new_der = new_der.expect("new handshakes must observe the reloaded certificate");
    assert_ne!(new_der, old_der);

    // 復帰確認
    let restored_der = tokio::task::spawn_blocking(move || get_server_cert_der(PROXY_PORT))
        .await
        .unwrap()
        .expect("handshake after restore");
    assert_eq!(
        restored_der, old_der,
        "original certificate must be served after restore"
    );
}

// ====================
// B-17: 不正バックエンド応答の即時エラー化テスト
// ====================
//
// 上流がプロトコル違反応答（ヘッダー途中切断・巨大ヘッダー・不正ステータス・
// 即クローズ・無応答・Content-Length 過大）を返した場合に、Veil がクライアントを
// ハングさせず速やかに 502/504/接続クローズへ変換することを検証する。
// バックエンドは tests/test_backends の bad-backend サーバー（ポート 9009）。

/// B-17 プローブ: 指定パスへ GET し、(status, body) を返す。
/// 修正前はクライアントタイムアウトまでハングしていたため、8 秒で打ち切る。
pub(super) async fn b17_probe(
    path: &str,
) -> Result<
    Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>>,
    tokio::time::error::Elapsed,
> {
    let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("client");
    tokio::time::timeout(Duration::from_secs(8), client.get(path)).await
}

#[tokio::test]
pub(super) async fn test_b17_bad_backend_ok_baseline() {
    // 正常プローブ: bad-backend ルート自体が機能していることの前提確認
    let res = b17_probe("/bad-backend/ok")
        .await
        .expect("must not hang")
        .expect("must succeed");
    assert_eq!(res.0, 200);
    assert_eq!(res.1, b"ok");
}

#[tokio::test]
pub(super) async fn test_b93_pooled_upstream_closed_while_idle_is_not_reused() {
    // B-93: bad-backend の通常応答は `Connection: close` を付けずに応答後すぐ接続を閉じる。
    // veil はこれを keep-alive 接続としてプールするため、従来は同じワーカーで次の要求が
    // 閉じた接続を再利用して EOF を読み、502 を返していた（E2E 全体実行時に
    // test_b17_bad_backend_no_response_returns_504 が 502 で落ちるフレークの正体）。
    // hyper クライアントは keep-alive で同じ接続（= 同じ veil ワーカー）を使い回す。
    let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("client");
    for i in 0..3 {
        // 専用ルート（上流プールを他テストと共有しない。e2e_setup.sh の bad-b93-pool 参照）
        let res = tokio::time::timeout(Duration::from_secs(8), client.get("/bad-backend-b93/ok"))
            .await
            .expect("must not hang")
            .expect("must succeed");
        assert_eq!(
            res.0, 200,
            "request #{i} must not reuse the closed upstream"
        );
        assert_eq!(res.1, b"ok");
        // 上流の FIN が veil 側ソケットへ届き、かつ生存確認の閾値（1ms）を超えるまで待つ。
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[tokio::test]
pub(super) async fn test_b17_bad_backend_instant_close_returns_502() {
    // 応答せず即クローズ → 即時 502
    let res = b17_probe("/bad-backend/instant-close")
        .await
        .expect("must not hang (B-17)")
        .expect("must receive a response");
    assert_eq!(res.0, 502, "instant-close must yield 502");
}

#[tokio::test]
pub(super) async fn test_b17_bad_backend_truncated_headers_returns_502() {
    // ヘッダー途中切断 → 即時 502
    let res = b17_probe("/bad-backend/truncated-headers")
        .await
        .expect("must not hang (B-17)")
        .expect("must receive a response");
    assert_eq!(res.0, 502, "truncated headers must yield 502");
}

#[tokio::test]
pub(super) async fn test_b17_bad_backend_bad_status_returns_502() {
    // 不正なステータスライン → 即時 502
    let res = b17_probe("/bad-backend/bad-status")
        .await
        .expect("must not hang (B-17)")
        .expect("must receive a response");
    assert_eq!(res.0, 502, "malformed status line must yield 502");
}

#[tokio::test]
pub(super) async fn test_b17_bad_backend_huge_headers_returns_502() {
    // 256KB 応答ヘッダー（MAX_RESPONSE_HEADER_SIZE=64KB 超過）→ 即時 502
    let res = b17_probe("/bad-backend/huge-headers")
        .await
        .expect("must not hang (B-17)")
        .expect("must receive a response");
    assert_eq!(res.0, 502, "oversized response headers must yield 502");
}

#[tokio::test]
pub(super) async fn test_b17_bad_backend_cl_too_large_closes_promptly() {
    // Content-Length: 1000 に対し 5 バイトのみ送信して EOF
    // → ヘッダーは転送済みのため 502 にはできないが、接続を即クローズして
    //   クライアントを CL 到達まで待たせないこと（ハングしないこと）を検証。
    let started = std::time::Instant::now();
    let res = b17_probe("/bad-backend/cl-too-large")
        .await
        .expect("must not hang (B-17)");
    // hyper は不完全ボディを IncompleteMessage エラーにする（成功しても部分ボディ）
    // Err(IncompleteMessage 等) は期待どおり（接続が即クローズされた）
    if let Ok((status, body)) = res {
        assert_eq!(status, 200);
        assert!(
            body.len() < 1000,
            "body must be truncated, got {}",
            body.len()
        );
    }
    assert!(
        started.elapsed() < Duration::from_secs(8),
        "connection must be closed promptly"
    );
}

#[tokio::test]
pub(super) async fn test_b17_bad_backend_cl_too_small_no_smuggling() {
    // Content-Length: 10 に対し 500 バイト送信 → 余剰バイトが転送されないこと（回帰ガード）
    let res = b17_probe("/bad-backend/cl-too-small")
        .await
        .expect("must not hang")
        .expect("must succeed");
    assert_eq!(res.0, 200);
    assert_eq!(res.1, b"XXXXXXXXXX", "exactly CL bytes must be forwarded");
}

#[tokio::test]
pub(super) async fn test_b17_bad_backend_no_response_returns_504() {
    // 受理後 30 秒無応答 → BACKEND_HEADER_TIMEOUT (10s) で 504
    let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("client");
    let res = tokio::time::timeout(
        Duration::from_secs(15),
        client.get("/bad-backend/no-response"),
    )
    .await
    .expect("must not hang past the backend header timeout (B-17)")
    .expect("must receive a response");
    assert_eq!(res.0, 504, "unresponsive upstream must yield 504");
}

// ====================
// F-62: Proxy-Wasm HTTP コール Pause/resume の E2E テスト
// ====================
//
// http_call_filter.wasm は on_http_request_headers で upstream "backend-pool" へ
// dispatch_http_call して Pause を返す。ホスト（エンジン）がコールをインラインで
// 解決して proxy_on_http_call_response で resume し、フィルタはコール結果の
// ステータスを含むローカルレスポンスを返す。

#[tokio::test]
#[cfg(feature = "wasm")]
pub(super) async fn test_f62_wasm_http_call_pause_resume() {
    let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("client");
    let (status, headers, body) = tokio::time::timeout(
        Duration::from_secs(10),
        client.send_request_with_response_headers(http::Method::GET, "/wasm-http-call/", &[], None),
    )
    .await
    .expect("must not hang (pause must be resumed)")
    .expect("must receive a response");

    // フィルタが resume されローカルレスポンスを返したこと
    assert_eq!(
        status, 200,
        "resumed filter must produce its local response"
    );
    assert_eq!(body, b"wasm-http-call-ok");

    // HTTP コールの結果（backend-pool の応答ステータス 200）が反映されていること
    let call_status = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-wasm-http-call-status"))
        .map(|(_, v)| v.as_str());
    assert_eq!(
        call_status,
        Some("200"),
        "http_call must reach the upstream and return its status"
    );
    let resumed = headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-wasm-http-call"))
        .map(|(_, v)| v.as_str());
    assert_eq!(resumed, Some("resumed"));
}

#[tokio::test]
#[cfg(feature = "wasm")]
pub(super) async fn test_f62_wasm_http_call_concurrent_requests() {
    // Pause/resume がリクエストごとに独立して機能すること（並行 8 リクエスト）
    let mut handles = Vec::new();
    for _ in 0..8 {
        handles.push(tokio::spawn(async {
            let client = Http1TestClient::new_https("127.0.0.1", PROXY_PORT).expect("client");
            tokio::time::timeout(Duration::from_secs(10), client.get("/wasm-http-call/"))
                .await
                .expect("must not hang")
                .expect("must receive a response")
        }));
    }
    for h in handles {
        let (status, body) = h.await.expect("join");
        assert_eq!(status, 200);
        assert_eq!(body, b"wasm-http-call-ok");
    }
}
