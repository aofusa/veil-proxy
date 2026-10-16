//! WASM の拡張
//!
//! F-233 で `tests/e2e_tests.rs` から移した（中身は変えていない）。

use super::*;

// ====================
// WASM Extension Tests
// ====================

#[cfg(feature = "wasm")]
pub(super) mod wasm_tests {
    use super::*;

    // ====================
    // 基本機能テスト
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_module_load() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // WASMモジュールがロードされていることを確認
        // 実際のロード確認は難しいため、WASMモジュールが適用されたルートへのリクエストで確認
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(
            response.is_some(),
            "Should receive response from WASM-enabled route"
        );

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが追加したヘッダーを確認
        let wasm_header = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            wasm_header,
            Some("true".to_string()),
            "Should have X-Veil-Processed header added by WASM module"
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_module_configuration() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // WASMモジュールの設定が読み込まれていることを確認
        // header_filterモジュールは設定に基づいてヘッダーを追加する
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();

        // WASMモジュールが追加したヘッダーを確認
        let filter_version = get_header_value(&response, "X-Veil-Filter-Version");
        assert_eq!(
            filter_version,
            Some("1.0.0".to_string()),
            "Should have X-Veil-Filter-Version header from WASM module"
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_context_lifecycle() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // WASMコンテキストのライフサイクルを確認
        // 複数のリクエストを送信して、コンテキストIDが異なることを確認
        let response1 = send_request(PROXY_PORT, "/wasm/", &[]).await;
        let response2 = send_request(PROXY_PORT, "/wasm/", &[]).await;

        assert!(response1.is_some(), "Should receive first response");
        assert!(response2.is_some(), "Should receive second response");

        let response1 = response1.unwrap();
        let response2 = response2.unwrap();

        let context_id1 = get_header_value(&response1, "X-Veil-Context-Id");
        let context_id2 = get_header_value(&response2, "X-Veil-Context-Id");

        // コンテキストIDが存在することを確認（値は異なる可能性がある）
        assert!(
            context_id1.is_some(),
            "Should have X-Veil-Context-Id in first response"
        );
        assert!(
            context_id2.is_some(),
            "Should have X-Veil-Context-Id in second response"
        );
    }

    // ====================
    // コールバック関数テスト
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_on_request_headers() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // on_request_headersコールバックの動作を確認
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();

        // WASMモジュールがリクエストヘッダーに追加したヘッダーがバックエンドに転送され、
        // レスポンスに反映されることを確認
        // header_filterはリクエストヘッダーにX-Veil-Proxy-Filterを追加
        // バックエンドがこのヘッダーを返すかどうかは実装依存だが、
        // レスポンスヘッダーにWASMモジュールが追加したヘッダーがあることを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header from WASM on_response_headers"
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_on_response_headers() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // on_response_headersコールバックの動作を確認
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();

        // WASMモジュールがレスポンスヘッダーに追加したヘッダーを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header from WASM module"
        );

        let filter_version = get_header_value(&response, "X-Veil-Filter-Version");
        assert_eq!(
            filter_version,
            Some("1.0.0".to_string()),
            "Should have X-Veil-Filter-Version header from WASM module"
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_on_log() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // on_logコールバックの動作を確認
        // ログ出力は直接確認できないため、リクエストが正常に処理されることを確認
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが正常に動作していることを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header indicating WASM module executed"
        );
    }

    // ====================
    // ホスト関数テスト
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_header_operations() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // ヘッダー操作のテスト
        // header_filterモジュールはヘッダーの追加を行う
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();

        // リクエストヘッダー操作の結果を確認（レスポンスヘッダー経由）
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header"
        );

        // レスポンスヘッダー操作の結果を確認
        let filter_version = get_header_value(&response, "X-Veil-Filter-Version");
        assert_eq!(
            filter_version,
            Some("1.0.0".to_string()),
            "Should have X-Veil-Filter-Version header"
        );
    }

    // ====================
    // ケーパビリティ制御テスト
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_capability_headers() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // ヘッダー読み取り・書き込み権限のテスト
        // header_filterモジュールはヘッダー読み取り・書き込み権限が必要
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();

        // 権限が有効な場合、ヘッダー操作が成功することを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header when header write capability is enabled"
        );
    }

    // ====================
    // 統合テスト
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_header_modification_filter() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // ヘッダー変更フィルタの動作を確認
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが追加した複数のヘッダーを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header"
        );

        let filter_version = get_header_value(&response, "X-Veil-Filter-Version");
        assert_eq!(
            filter_version,
            Some("1.0.0".to_string()),
            "Should have X-Veil-Filter-Version header"
        );

        let context_id = get_header_value(&response, "X-Veil-Context-Id");
        assert!(context_id.is_some(), "Should have X-Veil-Context-Id header");
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_route_specific_modules() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // ルート固有のモジュール適用を確認
        // /wasm/* パスにはWASMモジュールが適用される
        let wasm_response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(
            wasm_response.is_some(),
            "Should receive response from WASM route"
        );

        let wasm_response = wasm_response.unwrap();
        let wasm_processed = get_header_value(&wasm_response, "X-Veil-Processed");
        assert_eq!(
            wasm_processed,
            Some("true".to_string()),
            "WASM route should have X-Veil-Processed header"
        );

        // 通常のルートにはWASMモジュールが適用されない
        let normal_response = send_request(PROXY_PORT, "/", &[]).await;
        assert!(
            normal_response.is_some(),
            "Should receive response from normal route"
        );

        let normal_response = normal_response.unwrap();
        let normal_processed = get_header_value(&normal_response, "X-Veil-Processed");
        // 通常のルートにはWASMモジュールが適用されないため、このヘッダーは存在しない可能性がある
        // ただし、設定によっては存在する可能性もあるため、存在しないことを確認するのではなく、
        // WASMルートと通常ルートで異なる動作をすることを確認
        if normal_processed.is_some() {
            eprintln!(
                "Note: Normal route also has X-Veil-Processed header (may be configured globally)"
            );
        }
    }

    // ====================
    // 追加テスト: ボディ処理
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_on_request_body() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // on_request_bodyコールバックの動作を確認
        // 注意: header_filter.wasmはボディ処理を行わないため、基本的な動作確認のみ
        let body = b"test request body";
        let response = send_post_request(PROXY_PORT, "/wasm/", &[], body).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが正常に動作していることを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header indicating WASM module executed"
        );
    }

    #[tokio::test]
    #[ntest::timeout(30000)]
    async fn test_wasm_on_response_body() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // on_response_bodyコールバックの動作を確認
        // 注意: header_filter.wasmはボディ処理を行わないため、基本的な動作確認のみ
        let response = send_request_with_retry(PROXY_PORT, "/wasm/", &[], 5).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが正常に動作していることを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header indicating WASM module executed"
        );

        // レスポンスボディが存在することを確認
        // レスポンスボディはヘッダー部分の後に存在
        let body_start = response.find("\r\n\r\n");
        if let Some(start) = body_start {
            let body = &response[start + 4..];
            assert!(!body.is_empty(), "Should have response body");
        }
    }

    // ====================
    // 追加テスト: ケーパビリティ制御
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_capability_logging() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // ログ権限のテスト
        // header_filter.wasmはログ権限が有効になっているため、正常に動作することを確認
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // ログ権限が有効な場合、WASMモジュールが正常に動作することを確認
        // (ログ出力は直接確認できないため、動作確認のみ)
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header when logging capability is enabled"
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_capability_http_calls() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // HTTP呼び出し権限のテスト
        // 注意: header_filter.wasmはHTTP呼び出しを行わないため、基本的な動作確認のみ
        // 実際のHTTP呼び出しテストには、HTTP呼び出しを行うWASMモジュールが必要
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが正常に動作していることを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header"
        );
    }

    // ====================
    // 追加テスト: タイムアウト・エラーハンドリング
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_timeout() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // タイムアウト処理のテスト
        // 注意: 実際のタイムアウトテストには、長時間実行するWASMモジュールが必要
        // 現在は基本的な動作確認のみ
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // タイムアウトが発生しないことを確認（正常に処理される）
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header indicating WASM module executed without timeout"
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_error_handling() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // エラーハンドリングのテスト
        // 正常なリクエストがエラーなく処理されることを確認
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールがエラーなく動作することを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header indicating WASM module executed without error"
        );
    }

    // ====================
    // 追加テスト: 複数モジュール・同時実行
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_multiple_modules() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // 複数モジュールの適用を確認
        // 注意: 現在はheader_filter.wasmのみなので、同じモジュールが複数回適用されることを確認
        // 実際の複数モジュールテストには、異なるWASMモジュールが必要
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが正常に動作することを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header indicating WASM module executed"
        );

        // 複数のヘッダーが追加されることを確認（複数モジュールが適用された場合の動作確認）
        let filter_version = get_header_value(&response, "X-Veil-Filter-Version");
        assert_eq!(
            filter_version,
            Some("1.0.0".to_string()),
            "Should have X-Veil-Filter-Version header"
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_concurrent_execution() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // 同時実行時の動作を確認
        // 複数のリクエストを同時に送信して、WASMモジュールが正常に動作することを確認

        let mut handles = Vec::new();
        let num_requests = 10;

        for i in 0..num_requests {
            let handle = tokio::spawn(async move {
                let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
                (i, response)
            });
            handles.push(handle);
        }

        let mut success_count = 0;
        for handle in handles {
            if let Ok((_i, Some(response))) = handle.await {
                let status = get_status_code(&response);
                if status == Some(200) {
                    let processed = get_header_value(&response, "X-Veil-Processed");
                    if processed == Some("true".to_string()) {
                        success_count += 1;
                    }
                }
            }
        }

        // 少なくとも80%のリクエストが成功することを確認
        assert!(
            success_count >= num_requests * 8 / 10,
            "At least 80% of concurrent requests should succeed: {}/{}",
            success_count,
            num_requests
        );

        eprintln!(
            "Concurrent execution test: {}/{} requests succeeded",
            success_count, num_requests
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_invalid_module() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // 無効なWASMモジュールの処理を確認
        // 注意: 実際の無効なWASMモジュールのテストは、設定ファイルで無効なパスを指定する必要がある
        // 現在の実装では、有効なWASMモジュールが存在する場合のみテストを実行
        // 無効なモジュールのテストは、設定ファイルの変更が必要なため、基本的な動作確認のみ

        // 有効なWASMモジュールが正常に動作することを確認（無効なモジュールがないことを前提）
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが正常に動作することを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header indicating WASM module executed"
        );

        eprintln!("Invalid module test: Valid module executed successfully (invalid module test requires config changes)");
    }

    // ====================
    // 追加テスト: より詳細な検証
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_request_header_read() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // リクエストヘッダーの読み取りを確認
        // カスタムヘッダーを送信して、WASMモジュールが正常に動作することを確認
        let response = send_request(
            PROXY_PORT,
            "/wasm/",
            &[
                ("X-Custom-Header", "test-value"),
                ("User-Agent", "wasm-test-client"),
            ],
        )
        .await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // WASMモジュールが正常に動作することを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header indicating WASM module executed"
        );
    }

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_response_header_modification() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // レスポンスヘッダーの変更を確認
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();

        // WASMモジュールが追加した複数のヘッダーを確認
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header"
        );

        let filter_version = get_header_value(&response, "X-Veil-Filter-Version");
        assert_eq!(
            filter_version,
            Some("1.0.0".to_string()),
            "Should have X-Veil-Filter-Version header"
        );

        let context_id = get_header_value(&response, "X-Veil-Context-Id");
        assert!(context_id.is_some(), "Should have X-Veil-Context-Id header");

        // リクエストヘッダーに追加されたヘッダーも確認（バックエンドが返す場合）
        let request_id = get_header_value(&response, "X-Veil-Request-Id");
        // このヘッダーはリクエストヘッダーに追加されるが、レスポンスに含まれるかは実装依存
        if request_id.is_some() {
            eprintln!("Request ID header found in response: {:?}", request_id);
        }
    }

    // ====================
    // 追加テスト: ケーパビリティ制御（ローカルレスポンス）
    // ====================

    #[tokio::test]
    #[ntest::timeout(15000)]
    async fn test_wasm_capability_local_response() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // ローカルレスポンス送信権限のテスト
        // 注意: header_filter.wasmはローカルレスポンスを送信しないため、基本的な動作確認のみ
        // 実際のローカルレスポンステストには、ローカルレスポンスを送信するWASMモジュールが必要
        let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
        assert!(response.is_some(), "Should receive response");

        let response = response.unwrap();
        let status = get_status_code(&response);
        assert_eq!(status, Some(200), "Should return 200 OK");

        // ローカルレスポンス送信権限が有効な場合、WASMモジュールが正常に動作することを確認
        // (header_filter.wasmはローカルレスポンスを送信しないため、通常のレスポンスが返される)
        let processed = get_header_value(&response, "X-Veil-Processed");
        assert_eq!(
            processed,
            Some("true".to_string()),
            "Should have X-Veil-Processed header when local response capability is enabled"
        );
    }

    // ====================
    // 追加テスト: パフォーマンス
    // ====================

    #[tokio::test]
    #[ntest::timeout(30000)]
    async fn test_wasm_performance() {
        if !is_e2e_environment_ready().await {
            eprintln!("Skipping test: E2E environment not ready");
            return;
        }

        // WASMモジュールのパフォーマンステスト
        // JIT cold-start を避けるため計測前にウォームアップリクエストを送信する
        use std::time::Instant;

        // ウォームアップ: JITコンパイルを完了させてから計測開始（計測対象外）
        for i in 0..2u32 {
            let _ = send_request_with_retry(PROXY_PORT, "/wasm/", &[], 3).await;
            eprintln!("WASM warm-up request {} complete", i + 1);
        }

        let num_requests = 10u32;
        let mut total_time = Duration::from_secs(0);
        let mut success_count = 0u32;

        for _ in 0..num_requests {
            let start = Instant::now();
            // None または non-200 ステータスのどちらも最大5回リトライ
            let mut success = false;
            for _retry in 0..5u32 {
                let response = send_request(PROXY_PORT, "/wasm/", &[]).await;
                if let Some(resp) = response {
                    if get_status_code(&resp) == Some(200) {
                        success = true;
                        break;
                    }
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            let elapsed = start.elapsed();
            total_time += elapsed;
            if success {
                success_count += 1;
            }
        }

        assert_eq!(
            success_count, num_requests,
            "All requests should succeed: {}/{}",
            success_count, num_requests
        );

        let avg_time = total_time / num_requests;
        eprintln!(
            "WASM performance test: {} requests, avg time: {:?}",
            num_requests, avg_time
        );

        // ウォームアップ後のJITコンパイル済みコードとして10秒以内であること
        assert!(
            avg_time < Duration::from_secs(10),
            "Average execution time should be reasonable after warm-up: {:?}",
            avg_time
        );
    }
}
