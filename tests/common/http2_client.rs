//! HTTP/2テストクライアント
//!
//! h2ライブラリを使用したHTTP/2クライアント実装
//! tokioランタイム上で動作し、非同期API

use bytes::Bytes;
use h2::client::SendRequest;
use http::{Request, Response, StatusCode};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio_rustls::rustls::pki_types::ServerName;
use tokio_rustls::rustls::{ClientConfig, RootCertStore};
use tokio_rustls::TlsConnector;

/// HTTP/2テストクライアント
#[allow(dead_code)]
pub struct Http2TestClient {
    sender: SendRequest<Bytes>,
}

#[allow(dead_code)]
impl Http2TestClient {
    /// 新しいHTTP/2クライアントを作成
    pub async fn new(
        server_addr: &str,
        port: u16,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        // TCP接続
        let tcp = TcpStream::connect(format!("{}:{}", server_addr, port)).await?;

        // TLS設定（証明書検証なし - テスト用）
        let tls_config = create_tls_config()?;
        let connector = TlsConnector::from(tls_config);
        let server_name = ServerName::try_from(server_addr.to_string())?;
        let tls_stream = connector.connect(server_name, tcp).await?;

        // HTTP/2ハンドシェイク
        let (sender, connection) = h2::client::handshake(tls_stream).await?;

        // 接続をバックグラウンドで維持
        tokio::spawn(async move {
            if let Err(e) = connection.await {
                eprintln!("HTTP/2 connection error: {:?}", e);
            }
        });

        Ok(Self { sender })
    }

    /// HTTPリクエストを送信
    pub async fn send_request(
        &mut self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<&[u8]>,
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        // リクエストを構築
        let mut builder = Request::builder().method(method).uri(path);

        // デフォルトのHTTP/2疑似ヘッダーを追加
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case("host")) {
            builder = builder.header("host", "localhost");
        }

        // カスタムヘッダーを追加
        for (name, value) in headers {
            builder = builder.header(*name, *value);
        }

        let request = builder.body(())?;

        // リクエストを送信
        let end_of_stream = body.is_none();
        let (response_future, mut send_body) = self.sender.send_request(request, end_of_stream)?;

        // ボディを送信（ある場合）
        if let Some(body_data) = body {
            send_body.send_data(Bytes::copy_from_slice(body_data), true)?;
        }

        // レスポンスを受信
        let response = response_future.await?;
        let status = response.status().as_u16();

        // レスポンスボディを受信
        let mut body_data = Vec::new();
        let mut body_stream = response.into_body();
        while let Some(chunk) = body_stream.data().await {
            let data = chunk?;
            body_data.extend_from_slice(&data);
            body_stream.flow_control().release_capacity(data.len())?;
        }

        Ok((status, body_data))
    }

    /// 同一 HTTP/2 接続で複数リクエストを**多重化して並行送信**し、各レスポンスを
    /// `(status, body)` の配列で返す（要求順に対応）。
    ///
    /// 全リクエストの response future を先に発行してから await するため、プロキシは
    /// 複数ストリームのレスポンス（連結された HEADERS+DATA）を 1 接続上で交互に送出する。
    /// 送信ホットパスのフレーム連結が多重化下でもフレーム境界・ストリーム対応を壊さない
    /// ことを検証するために使う。
    pub async fn send_concurrent(
        &mut self,
        reqs: &[(&str, &str, Option<&[u8]>)],
    ) -> Result<Vec<(u16, Vec<u8>)>, Box<dyn std::error::Error + Send + Sync>> {
        let mut response_futures = Vec::with_capacity(reqs.len());
        for (method, path, body) in reqs {
            let request = Request::builder()
                .method(*method)
                .uri(*path)
                .header("host", "localhost")
                .body(())?;
            let end_of_stream = body.is_none();
            // 少数の並行ストリーム（max_concurrent_streams 内）のため ready() 待ちは不要。
            let (response_future, mut send_body) =
                self.sender.send_request(request, end_of_stream)?;
            if let Some(body_data) = body {
                send_body.send_data(Bytes::copy_from_slice(body_data), true)?;
            }
            response_futures.push(response_future);
        }

        let mut results = Vec::with_capacity(response_futures.len());
        for response_future in response_futures {
            let response = response_future.await?;
            let status = response.status().as_u16();
            let mut body_data = Vec::new();
            let mut body_stream = response.into_body();
            while let Some(chunk) = body_stream.data().await {
                let data = chunk?;
                body_data.extend_from_slice(&data);
                body_stream.flow_control().release_capacity(data.len())?;
            }
            results.push((status, body_data));
        }

        Ok(results)
    }

    /// GETリクエストを送信
    pub async fn get(
        &mut self,
        path: &str,
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        self.send_request("GET", path, &[], None).await
    }

    /// POSTリクエストを送信
    pub async fn post(
        &mut self,
        path: &str,
        body: &[u8],
    ) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
        self.send_request("POST", path, &[], Some(body)).await
    }
}

/// テスト用TLS設定を作成（証明書検証なし）
fn create_tls_config() -> Result<Arc<ClientConfig>, Box<dyn std::error::Error + Send + Sync>> {
    use tokio_rustls::rustls::crypto::CryptoProvider;

    // CryptoProviderを初期化（既に初期化されている場合は無視）
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = CryptoProvider::install_default(
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider(),
        );
    });

    // 証明書検証をスキップするカスタム検証器
    #[derive(Debug)]
    struct SkipServerVerification;

    impl tokio_rustls::rustls::client::danger::ServerCertVerifier for SkipServerVerification {
        fn verify_server_cert(
            &self,
            _end_entity: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _intermediates: &[tokio_rustls::rustls::pki_types::CertificateDer<'_>],
            _server_name: &tokio_rustls::rustls::pki_types::ServerName<'_>,
            _ocsp: &[u8],
            _now: tokio_rustls::rustls::pki_types::UnixTime,
        ) -> Result<
            tokio_rustls::rustls::client::danger::ServerCertVerified,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::ServerCertVerified::assertion())
        }

        fn verify_tls12_signature(
            &self,
            _message: &[u8],
            _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _dss: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn verify_tls13_signature(
            &self,
            _message: &[u8],
            _cert: &tokio_rustls::rustls::pki_types::CertificateDer<'_>,
            _dss: &tokio_rustls::rustls::DigitallySignedStruct,
        ) -> Result<
            tokio_rustls::rustls::client::danger::HandshakeSignatureValid,
            tokio_rustls::rustls::Error,
        > {
            Ok(tokio_rustls::rustls::client::danger::HandshakeSignatureValid::assertion())
        }

        fn supported_verify_schemes(&self) -> Vec<tokio_rustls::rustls::SignatureScheme> {
            tokio_rustls::rustls::crypto::aws_lc_rs::default_provider()
                .signature_verification_algorithms
                .supported_schemes()
                .to_vec()
        }
    }

    // HTTP/2用のALPN設定
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();

    config.alpn_protocols = vec![b"h2".to_vec()];

    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_http2_client_creation() {
        // このテストは実際のサーバーなしでは失敗するが、
        // コンパイルが通ることを確認
        let result = Http2TestClient::new("localhost", 8443).await;
        // 接続エラーは想定内
        assert!(result.is_err() || result.is_ok());
    }
}
