//! E2E（End-to-End）テスト
//!
//! プロキシの完全な動作を検証するテストです。
//!
//! ## 実行方法
//!
//! ### 方法1: セットアップスクリプトを使用（推奨）
//! ```bash
//! ./tests/e2e_setup.sh test
//! ```
//!
//! 並列化による高速化（Phase 1実装済み）:
//! - デフォルト: CPUコア数または4（小さい方）で並列実行
//! - カスタム並列数: `PARALLEL_JOBS=8 ./tests/e2e_setup.sh test`
//!
//! ### 方法2: 手動で環境を準備
//! ```bash
//! # 1. 環境を起動
//! ./tests/e2e_setup.sh start
//!
//! # 2. テストを実行（並列実行）
//! cargo test --test e2e_tests -- --test-threads=4
//!
//! # 3. 環境を停止
//! ./tests/e2e_setup.sh stop
//! ```
//!
//! ## テスト対象
//! - HTTP/HTTPS リクエスト転送
//! - ロードバランシング（Round Robin）
//! - ヘッダー操作（追加・削除）
//! - レスポンス圧縮
//! - ヘルスチェック

// 理由付き allow: テストハーネス・E2E クライアントは同期 I/O / sleep / std::net を
// 意図的に使用する（プロキシ本体のデータプレーンとは別プロセス・別スレッド）。
// F-88 の disallowed-methods はデータプレーン向け規則のため、テストではファイル単位で許容する。
#![allow(clippy::disallowed_methods)]

// F-122 / B-51: テストクライアントの rustls 暗号プロバイダは本体（src/tls_provider.rs）と
// 同じ target 別選択にする。OpenBSD で aws-lc-rs を使うと aws-lc-sys の curve25519
// （s2n-bignum アセンブリ）で SIGSEGV し、テストバイナリごと落ちる。
// F-140: NetBSD も OpenBSD と同じく rustls は ring を使う（Cargo.toml の
// target 別 dev-dependencies 参照）。
#[cfg(not(any(target_os = "openbsd", target_os = "netbsd")))]
use rustls::crypto::aws_lc_rs as test_crypto;
#[cfg(any(target_os = "openbsd", target_os = "netbsd"))]
use rustls::crypto::ring as test_crypto;

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

#[path = "../common/mod.rs"]
mod common;
use serial_test::serial;

use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;
use rustls::{ClientConfig, ClientConnection};

// 参照元は LPM ヘルパ（`grpc` かつ HTTP/2 か HTTP/3 が有効なときだけ存在する）と
// gRPC-Web 形式テスト（`grpc-web`）の双方。参照元と同じ条件で cfg しないと、
// `--no-default-features --features grpc` のように「grpc だけで HTTP/2・HTTP/3 が無い」
// 構成で未使用インポート警告になる。
#[cfg(any(
    all(feature = "grpc", any(feature = "http2", feature = "http3")),
    feature = "grpc-web"
))]
use common::grpc_client::GrpcFrame;

// 新しい非同期テストクライアント（hyper + tokio）
use common::http1_client::Http1TestClient;

// 新しい非同期HTTP/3テストクライアント（h3 + quinn）
#[cfg(feature = "http3")]
use common::http3_client::{http3_get, Http3TestClient};

// 新しい非同期gRPCテストクライアント（tonic）
use common::grpc_client::GrpcTestClient;

// HTTP/2 クライアント（gRPC framing / trailers 詳細検証）
#[cfg(feature = "http2")]
use common::http2_client::Http2TestClient;

/// SimpleRequest { message } の最小 protobuf エンコード（field 1, length-delimited）。
/// メッセージ長は 127 バイト未満を想定。F-92/F-93/F-112 gRPC 詳細 E2E 専用。
#[cfg(feature = "grpc")]
fn encode_simple_request(msg: &str) -> Vec<u8> {
    let bytes = msg.as_bytes();
    assert!(
        bytes.len() < 128,
        "encode_simple_request: message too long for single-byte length"
    );
    let mut out = Vec::with_capacity(2 + bytes.len());
    out.push(0x0a); // field 1, wire type 2
    out.push(bytes.len() as u8);
    out.extend_from_slice(bytes);
    out
}

/// gRPC LPM（Length-Prefixed Message）を組み立てる。F-92/F-93/F-112 gRPC 詳細 E2E 専用。
///
/// 呼び出し側のテストはいずれも `grpc` に加えて `http2` か `http3` を要求する
/// （gRPC は HTTP/2 か HTTP/3 の上でしか流れない）。cfg を `grpc` だけにすると
/// `--no-default-features --features grpc` で未使用となり dead_code 警告が出る。
#[cfg(all(feature = "grpc", any(feature = "http2", feature = "http3")))]
fn encode_grpc_lpm(message: &[u8]) -> Vec<u8> {
    GrpcFrame::new(message.to_vec()).encode()
}

/// ボディから複数 gRPC LPM を順に抽出。F-92/F-93 gRPC 詳細 E2E 専用。
///
/// cfg を `grpc` だけにしない理由は [`encode_grpc_lpm`] と同じ。
#[cfg(all(feature = "grpc", any(feature = "http2", feature = "http3")))]
fn decode_all_grpc_frames(body: &[u8]) -> Vec<GrpcFrame> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while offset + 5 <= body.len() {
        match GrpcFrame::decode(&body[offset..]) {
            Ok((frame, consumed)) => {
                frames.push(frame);
                offset += consumed;
            }
            Err(_) => break,
        }
    }
    frames
}

// E2E環境のポート設定（e2e_setup.shと一致させる）
const PROXY_PORT: u16 = 8443; // プロキシHTTPSポート
const PROXY_H2C_PORT: u16 = 8081; // H2C (HTTP/2 Cleartext) ポート
const PROXY_L4_PORT: u16 = 8444; // L4 TCP プロキシ（TLS パススルー、F-30）
const PROXY_L4_LEAST_CONN_PORT: u16 = 8445; // L4 Least Connection
const PROXY_L4_TERMINATE_PORT: u16 = 8446; // L4 TLS 終端
const PROXY_L4_UDP_PORT: u16 = 8447; // L4 UDP プロキシ（セッションテーブル方式、F-124）

// L4 network filter（WASM、F-133）。参照元のテストが l4-proxy + wasm 双方を要求するため、
// 同じ条件で cfg しないと、`wasm` を含まない feature 構成で dead_code 警告になる。
#[cfg(all(feature = "l4-proxy", feature = "wasm"))]
const PROXY_L4_WASM_PORT: u16 = 8448;
// http3 を外したビルドでは参照元が全て消えるため、近隣の PROXY_L4_WASM_PORT と同じく
// feature でゲートする。
#[cfg(feature = "http3")]
const PROXY_HTTP3_PORT: u16 = 8443; // HTTP/3ポート（デフォルトではHTTPSポートと同じ）
const BACKEND1_PORT: u16 = 9001;
const BACKEND2_PORT: u16 = 9002;

/// E2E環境が起動しているか確認（非同期版）
async fn is_e2e_environment_ready() -> bool {
    use tokio::net::TcpStream;
    use tokio::time::{timeout, Duration};

    // プロキシHTTPSポートへの接続確認（TCPレベル）
    match timeout(
        Duration::from_secs(2),
        TcpStream::connect(format!("127.0.0.1:{}", PROXY_PORT)),
    )
    .await
    {
        Ok(Ok(_)) => {}
        _ => {
            eprintln!(
                "E2E environment not ready: Proxy not running on port {}",
                PROXY_PORT
            );
            eprintln!("Please run: ./tests/e2e_setup.sh start");
            return false;
        }
    }

    // バックエンドへの接続確認（TCPレベルで十分）
    // 注意: バックエンドはTLS必須だが、TCP接続成功=ポート開放を確認
    match timeout(
        Duration::from_secs(2),
        TcpStream::connect(format!("127.0.0.1:{}", BACKEND1_PORT)),
    )
    .await
    {
        Ok(Ok(_)) => {}
        _ => {
            eprintln!(
                "E2E environment not ready: Backend 1 not running on port {}",
                BACKEND1_PORT
            );
            return false;
        }
    }

    match timeout(
        Duration::from_secs(2),
        TcpStream::connect(format!("127.0.0.1:{}", BACKEND2_PORT)),
    )
    .await
    {
        Ok(Ok(_)) => {}
        _ => {
            eprintln!(
                "E2E environment not ready: Backend 2 not running on port {}",
                BACKEND2_PORT
            );
            return false;
        }
    }

    true
}

/// rustlsのCryptoProviderを初期化（一度だけ実行）
fn init_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = CryptoProvider::install_default(test_crypto::default_provider());
    });
}

/// 証明書検証をスキップするカスタム検証器
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        test_crypto::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
            .to_vec()
    }
}

/// TLSクライアント設定を作成（自己署名証明書を許可）
fn create_client_config() -> Arc<ClientConfig> {
    init_crypto_provider();

    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    Arc::new(config)
}

/// HTTPS リクエストを送信してレスポンスを取得（GETメソッド、非同期版）
async fn send_request(port: u16, path: &str, headers: &[(&str, &str)]) -> Option<String> {
    send_request_with_method(port, path, "GET", headers, None).await
}

/// リトライロジック付きでリクエストを送信（並列実行時の接続エラー対策、非同期版）
async fn send_request_with_retry(
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
    max_retries: usize,
) -> Option<String> {
    use tokio::time::{sleep, Duration};

    for attempt in 0..max_retries {
        if let Some(response) = send_request(port, path, headers).await {
            return Some(response);
        }

        // 最後の試行でない場合、待機してからリトライ
        if attempt < max_retries - 1 {
            let backoff = Duration::from_millis(100 * (attempt + 1) as u64);
            sleep(backoff).await;
        }
    }
    None
}

/// HTTPS POSTリクエストを送信してレスポンスを取得（非同期版）
///
/// 参照元は http2（H2C POST）と wasm（ボディフィルタ）のテストのみ。
/// 双方無効な構成では未使用関数の警告になるため cfg する。
#[cfg(any(feature = "http2", feature = "wasm"))]
async fn send_post_request(
    port: u16,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Option<String> {
    send_request_with_method(port, path, "POST", headers, Some(body)).await
}

/// HTTPS リクエストを送信してレスポンスを取得（メソッドとボディ指定可能、非同期版）
async fn send_request_with_method(
    port: u16,
    path: &str,
    method: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Option<String> {
    use http::Method;

    // 非同期版のHTTP/1.1クライアントを使用
    let client = match Http1TestClient::new_https("127.0.0.1", port) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[send_request] Failed to create HTTP client: {}", e);
            return None;
        }
    };

    // メソッドの変換
    let method_enum = match Method::from_bytes(method.as_bytes()) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("[send_request] Invalid HTTP method: {}", e);
            return None;
        }
    };

    // レスポンスヘッダーも含めて取得するように修正（5秒タイムアウト付き）
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        client.send_request_with_response_headers(method_enum, path, headers, body),
    )
    .await;
    let result = match result {
        Ok(r) => r,
        Err(_) => {
            eprintln!("[send_request] Request timed out after 5s");
            return None;
        }
    };
    match result {
        Ok((status, resp_headers, body_bytes)) => {
            let response_body = String::from_utf8_lossy(&body_bytes).to_string();

            // ステータス行
            let mut full_response = format!("HTTP/1.1 {} OK\r\n", status);

            // ヘッダーを追加
            for (name, value) in resp_headers {
                full_response.push_str(&format!("{}: {}\r\n", name, value));
            }

            // ヘッダーとボディの間の空行
            full_response.push_str("\r\n");
            full_response.push_str(&response_body);

            Some(full_response)
        }
        Err(e) => {
            eprintln!("[send_request] Request failed: {}", e);
            None
        }
    }
}

/// Content-Lengthヘッダーから値を取得
fn get_content_length_from_headers(headers: &[u8]) -> Option<usize> {
    let header_str = String::from_utf8_lossy(headers);
    for line in header_str.lines() {
        if line.is_empty() {
            break;
        }
        if let Some(idx) = line.find(':') {
            let name = line[..idx].trim().to_lowercase();
            if name == "content-length" {
                if let Ok(len) = line[idx + 1..].trim().parse::<usize>() {
                    return Some(len);
                }
            }
        }
    }
    None
}

/// レスポンスからヘッダー値を抽出
fn get_header_value(response: &str, header_name: &str) -> Option<String> {
    let header_lower = header_name.to_lowercase();
    for line in response.lines() {
        if line.is_empty() {
            break; // ヘッダー終了
        }
        if let Some(idx) = line.find(':') {
            let name = line[..idx].trim().to_lowercase();
            if name == header_lower {
                return Some(line[idx + 1..].trim().to_string());
            }
        }
    }
    None
}

/// レスポンスのステータスコードを取得
fn get_status_code(response: &str) -> Option<u16> {
    let first_line = response.lines().next()?;
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() >= 2 {
        parts[1].parse().ok()
    } else {
        None
    }
}

// ====================
// 非同期テストヘルパー関数（hyper + tokio）
// ====================

/// 非同期版: HTTPS GETリクエストを送信
async fn send_request_async(
    port: u16,
    path: &str,
) -> Result<(u16, String), Box<dyn std::error::Error + Send + Sync>> {
    let client = Http1TestClient::new_https("127.0.0.1", port)?;
    let (status, body) = client.get(path).await?;
    Ok((status, String::from_utf8_lossy(&body).to_string()))
}

// ====================
// 非同期版 プロキシ基本機能テスト（hyper使用）
// ====================

/// プロキシ基本リクエストテスト（非同期版）
#[tokio::test]
#[ntest::timeout(15000)]
async fn test_proxy_basic_request_async() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    match send_request_async(PROXY_PORT, "/").await {
        Ok((status, _body)) => {
            assert_eq!(status, 200, "Should return 200 OK");
        }
        Err(e) => {
            eprintln!("Request failed: {}", e);
            panic!("Failed to send request via hyper client: {}", e);
        }
    }
}

/// ヘルスエンドポイントテスト（非同期版）
#[tokio::test]
#[ntest::timeout(15000)]
async fn test_proxy_health_endpoint_async() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    match send_request_async(PROXY_PORT, "/health").await {
        Ok((status, _body)) => {
            assert_eq!(status, 200, "Health endpoint should return 200 OK");
        }
        Err(e) => {
            eprintln!("Request failed: {}", e);
            panic!("Failed to send request to health endpoint: {}", e);
        }
    }
}

// ====================
// 非同期版 HTTP/3テスト（h3 + quinn使用）
// ====================

/// HTTP/3基本接続テスト（非同期版）
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
async fn test_http3_basic_connection_async() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立（新しいh3-quinnクライアント使用）
    match Http3TestClient::connect(server_addr, "localhost").await {
        Ok(_client) => {
            eprintln!("HTTP/3 (h3-quinn) connection established successfully");
        }
        Err(e) => {
            panic!(
                "HTTP/3 handshake failed for {}: {} (HTTP/3 may not be enabled)",
                server_addr, e
            );
            // HTTP/3が有効化されていない場合はテストをスキップ
        }
    }
}

/// HTTP/3 GETリクエストテスト（非同期版）
#[tokio::test]
#[ntest::timeout(15000)]
#[cfg(feature = "http3")]
async fn test_http3_get_request_async() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    let server_addr = format!("127.0.0.1:{}", PROXY_HTTP3_PORT)
        .parse()
        .expect("Invalid server address");

    // HTTP/3接続を確立してリクエストを送信
    match Http3TestClient::new(server_addr, "localhost").await {
        Ok((_client, mut send_request)) => {
            // GETリクエストを送信
            match http3_get(&mut send_request, "/health").await {
                Ok((status, body)) => {
                    assert_eq!(status, 200, "HTTP/3 GET should return 200 OK");
                    eprintln!("HTTP/3 GET response: {} bytes", body.len());
                }
                Err(e) => {
                    panic!(
                        "HTTP/3 GET request failed: {} (HTTP/3 may not be enabled)",
                        e
                    );
                    // HTTP/3が有効化されていない場合はテストをスキップ
                }
            }
        }
        Err(e) => {
            panic!(
                "HTTP/3 connection failed for {}: {} (HTTP/3 may not be enabled)",
                server_addr, e
            );
            // HTTP/3が有効化されていない場合はテストをスキップ
        }
    }
}

// ====================
// 非同期版 gRPCテスト（tonic使用）
// ====================

/// gRPC接続テスト（非同期版）
/// 注意: tonicはProtobufサービス定義が必要なため、
/// ここでは接続確立のみをテストします
#[tokio::test]
#[ntest::timeout(15000)]
async fn test_grpc_connection_async() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // gRPCクライアントの作成を試行（TLS接続）
    let _client = GrpcTestClient::new("127.0.0.1", PROXY_PORT)
        .await
        .expect("gRPC (tonic) TLS connection should succeed");
    eprintln!("gRPC (tonic) connection created successfully");
}

/// gRPCプレーンテキスト（h2c）接続テスト（非同期版）
#[tokio::test]
#[ntest::timeout(15000)]
async fn test_grpc_h2c_connection_async() {
    if !is_e2e_environment_ready().await {
        eprintln!("Skipping test: E2E environment not ready");
        return;
    }

    // H2Cポートへの接続を試行
    let _client = GrpcTestClient::new_plaintext("127.0.0.1", PROXY_H2C_PORT)
        .await
        .unwrap_or_else(|e| {
            panic!(
                "gRPC h2c connection to port {} should succeed: {}",
                PROXY_H2C_PORT, e
            )
        });
    eprintln!(
        "gRPC (tonic h2c) connection created successfully on port {}",
        PROXY_H2C_PORT
    );
}

// F-233: 機能ごとのファイル（各ファイルは `use super::*;` でここのヘルパーを使う。テスト名は変えていない）
mod basic;
mod features;
#[cfg(any(feature = "http3", feature = "grpc", feature = "grpc-web"))]
mod h3_grpc_detail;
mod ktls_h2_ws_edge;
mod protocols;
#[cfg(feature = "ktls")]
use self::ktls_h2_ws_edge::is_ktls_available;
mod h2c_buffering_ws_health;
mod http_details;
mod methods_pool_stress;
mod sni_cache_buffering_health;
mod tickets;
#[cfg(feature = "wasm")]
mod wasm;
// h3_grpc_coverage のリロードの試験（http3）が使う
#[cfg(feature = "http3")]
use self::tickets::{fixtures_dir, proxy_pid, send_sighup, RELOAD_TEST_LOCK};
mod coverage;
#[cfg(any(feature = "http3", feature = "grpc", feature = "grpc-web"))]
mod h3_grpc_coverage;
#[cfg(any(feature = "http3", feature = "grpc", feature = "grpc-web"))]
mod h3_grpc_coverage2;
mod upstream;
// F-236 の監査で直した要求の扱い（B-142・B-143・B-148）
mod security_audit;
