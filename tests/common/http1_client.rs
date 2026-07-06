//! HTTP/1.1テストクライアント
//!
//! hyper + hyper-rustls + tokioを使用したHTTP/1.1クライアント実装
//! TLS対応の非同期HTTP/1.1リクエストを送信

use bytes::Bytes;
use http::{Method, Request, Uri};
use http_body_util::{BodyExt, Full};
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use rustls::ClientConfig;
use std::sync::Arc;

type HyperClient = Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// HTTP/1.1テストクライアント
/// hyper + hyper-rustlsを使用したHTTPS/HTTP対応クライアント
#[allow(dead_code)]
pub struct Http1TestClient {
    client: HyperClient,
    base_url: String,
}

#[allow(dead_code)]
impl Http1TestClient {
    /// 新しいHTTP/1.1クライアントを作成（HTTPS用、証明書検証なし）
    pub fn new_https(
        host: &str,
        port: u16,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // CryptoProviderを初期化
        init_crypto_provider();

        let tls_config = create_tls_config_no_alpn()?;

        let https_connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_tls_config((*tls_config).clone())
            .https_or_http()
            .enable_http1()
            .build();

        let client = Client::builder(TokioExecutor::new()).build(https_connector);

        Ok(Self {
            client,
            base_url: format!("https://{}:{}", host, port),
        })
    }

    /// 新しいHTTP/1.1クライアントを作成（HTTP用）
    pub fn new_http(
        host: &str,
        port: u16,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // CryptoProviderを初期化（native_rootsで必要）
        init_crypto_provider();

        let https_connector = hyper_rustls::HttpsConnectorBuilder::new()
            .with_native_roots()?
            .https_or_http()
            .enable_http1()
            .build();

        let client = Client::builder(TokioExecutor::new()).build(https_connector);

        Ok(Self {
            client,
            base_url: format!("http://{}:{}", host, port),
        })
    }

    /// GETリクエストを送信
    pub async fn get(
        &self,
        path: &str,
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        self.send_request(Method::GET, path, &[], None).await
    }

    /// POSTリクエストを送信
    pub async fn post(
        &self,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        self.send_request(Method::POST, path, &[], Some(body)).await
    }

    /// カスタムヘッダー付きGETリクエストを送信
    pub async fn get_with_headers(
        &self,
        path: &str,
        headers: &[(&str, &str)],
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        self.send_request(Method::GET, path, headers, None).await
    }

    /// カスタムヘッダー付きPOSTリクエストを送信
    pub async fn post_with_headers(
        &self,
        path: &str,
        headers: &[(&str, &str)],
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        self.send_request(Method::POST, path, headers, Some(body))
            .await
    }

    /// HTTPリクエストを送信
    pub async fn send_request(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        let uri: Uri = format!("{}{}", self.base_url, path).parse()?;
        let host = uri.host().unwrap_or("localhost").to_string();

        let mut request_builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", host);

        // カスタムヘッダーを追加
        for (name, value) in headers {
            request_builder = request_builder.header(*name, *value);
        }

        // ボディを設定
        let request_body = match body {
            Some(data) => Full::new(Bytes::copy_from_slice(data)),
            None => Full::new(Bytes::new()),
        };

        let request = request_builder.body(request_body)?;

        // リクエストを送信
        let response = self.client.request(request).await?;
        let status = response.status().as_u16();

        // レスポンスボディを読み取り
        let body_bytes = response.into_body().collect().await?.to_bytes();

        Ok((status, body_bytes.to_vec()))
    }

    /// レスポンスヘッダー付きでリクエストを送信
    pub async fn send_request_with_response_headers(
        &self,
        method: Method,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<(String, String)>, Vec<u8>), Box<dyn std::error::Error + Send + Sync>>
    {
        let uri: Uri = format!("{}{}", self.base_url, path).parse()?;
        let host = uri.host().unwrap_or("localhost").to_string();

        let mut request_builder = Request::builder()
            .method(method)
            .uri(uri)
            .header("host", host);

        for (name, value) in headers {
            request_builder = request_builder.header(*name, *value);
        }

        let request_body = match body {
            Some(data) => Full::new(Bytes::copy_from_slice(data)),
            None => Full::new(Bytes::new()),
        };

        let request = request_builder.body(request_body)?;
        let response = self.client.request(request).await?;

        let status = response.status().as_u16();

        // ヘッダーを抽出
        let response_headers: Vec<(String, String)> = response
            .headers()
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_str().unwrap_or("").to_string()))
            .collect();

        let body_bytes = response.into_body().collect().await?.to_bytes();

        Ok((status, response_headers, body_bytes.to_vec()))
    }
}

/// CryptoProviderを初期化（グローバルに一度だけ）
fn init_crypto_provider() {
    use rustls::crypto::CryptoProvider;

    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider());
    });
}

/// テスト用TLS設定を作成（ALPNなし、証明書検証なし）
/// hyper-rustlsがALPNを内部で設定するため、ここでは設定しない
fn create_tls_config_no_alpn() -> Result<Arc<ClientConfig>, Box<dyn std::error::Error + Send + Sync>>
{
    // 証明書検証をスキップするカスタム検証器
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
            rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
                .to_vec()
        }
    }

    // ALPNは設定しない（hyper-rustlsが内部で設定する）
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http1_client_creation() {
        // クライアント作成テスト（サーバーなしでもコンパイルが通ることを確認）
        let client = Http1TestClient::new_https("localhost", 8443);
        assert!(client.is_ok());
    }

    #[tokio::test]
    async fn test_http1_client_http_creation() {
        // HTTPクライアント作成テスト
        let client = Http1TestClient::new_http("localhost", 8080);
        assert!(client.is_ok());
    }
}
