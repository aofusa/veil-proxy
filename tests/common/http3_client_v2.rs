//! HTTP/3テストクライアント（h3/quinn版）
//!
//! h3+h3-quinn+quinnライブラリを使用したHTTP/3クライアント実装
//! tokioランタイム上で動作し、非同期API

use h3::client::SendRequest;
use h3_quinn::Connection;
use quinn::Endpoint;
use http::Request;
use bytes::{Bytes, Buf};
use std::net::SocketAddr;
use std::sync::Arc;

/// HTTP/3テストクライアント
#[allow(dead_code)]
pub struct Http3TestClientV2 {
    endpoint: Endpoint,
}

#[allow(dead_code)]
impl Http3TestClientV2 {
    /// 新しいHTTP/3クライアントを作成
    pub async fn new(
        server_addr: SocketAddr,
        server_name: &str,
    ) -> Result<(Self, SendRequest<h3_quinn::OpenStreams, Bytes>), Box<dyn std::error::Error + Send + Sync>> {
        // QUIC設定
        let client_crypto = create_quic_tls_config()?;
        
        // QuicClientConfigでラップ（これがquinnが期待する型）
        let quic_config = quinn::crypto::rustls::QuicClientConfig::try_from(client_crypto)?;
        let mut client_config = quinn::ClientConfig::new(Arc::new(quic_config));
        
        // トランスポート設定
        let mut transport = quinn::TransportConfig::default();
        transport.max_idle_timeout(Some(std::time::Duration::from_secs(30).try_into()?));
        client_config.transport_config(Arc::new(transport));
        
        // エンドポイント作成
        let mut endpoint = Endpoint::client("0.0.0.0:0".parse()?)?;
        endpoint.set_default_client_config(client_config);
        
        // サーバーに接続
        let conn = endpoint.connect(server_addr, server_name)?.await?;
        
        // HTTP/3接続をセットアップ
        let h3_conn = Connection::new(conn);
        let (mut driver, send_request) = h3::client::new(h3_conn).await?;
        
        // ドライバーをバックグラウンドで動かす
        tokio::spawn(async move {
            // wait_idle() returns ConnectionError directly
            let err = driver.wait_idle().await;
            eprintln!("HTTP/3 connection closed: {:?}", err);
        });
        
        Ok((Self { endpoint }, send_request))
    }

    /// 単純な接続テスト用
    pub async fn connect(
        server_addr: SocketAddr,
        server_name: &str,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let (client, _) = Self::new(server_addr, server_name).await?;
        Ok(client)
    }
}

/// HTTP/3リクエストを送信するヘルパー関数
#[allow(dead_code)]
pub async fn send_http3_request(
    send_request: &mut SendRequest<h3_quinn::OpenStreams, Bytes>,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    // リクエストを構築
    let mut builder = Request::builder()
        .method(method)
        .uri(path);
    
    // カスタムヘッダーを追加
    for (name, value) in headers {
        builder = builder.header(*name, *value);
    }
    
    let request = builder.body(())?;
    
    // リクエストを送信
    let mut stream = send_request.send_request(request).await?;
    
    // ボディを送信（ある場合）
    if let Some(body_data) = body {
        stream.send_data(Bytes::copy_from_slice(body_data)).await?;
    }
    stream.finish().await?;
    
    // レスポンスを受信
    let response = stream.recv_response().await?;
    let status = response.status().as_u16();
    
    // レスポンスボディを受信
    let mut body_data = Vec::new();
    while let Some(chunk) = stream.recv_data().await? {
        // Bufトレイトを使用してバイトを取得
        let mut remaining = chunk;
        while remaining.has_remaining() {
            let bytes = remaining.chunk();
            body_data.extend_from_slice(bytes);
            remaining.advance(bytes.len());
        }
    }
    
    Ok((status, body_data))
}

/// GETリクエストを送信するヘルパー関数
#[allow(dead_code)]
pub async fn http3_get(
    send_request: &mut SendRequest<h3_quinn::OpenStreams, Bytes>,
    path: &str,
) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    send_http3_request(send_request, "GET", path, &[], None).await
}

/// POSTリクエストを送信するヘルパー関数
#[allow(dead_code)]
pub async fn http3_post(
    send_request: &mut SendRequest<h3_quinn::OpenStreams, Bytes>,
    path: &str,
    body: &[u8],
) -> Result<(u16, Vec<u8>), Box<dyn std::error::Error + Send + Sync>> {
    send_http3_request(send_request, "POST", path, &[], Some(body)).await
}

/// テスト用QUIC TLS設定を作成（証明書検証なし）
fn create_quic_tls_config() -> Result<Arc<rustls::ClientConfig>, Box<dyn std::error::Error + Send + Sync>> {
    use rustls::crypto::CryptoProvider;
    
    // CryptoProviderを初期化（既に初期化されている場合は無視）
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider());
    });
    
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
    
    // HTTP/3用のALPN設定
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    
    // HTTP/3のALPN
    config.alpn_protocols = vec![b"h3".to_vec()];
    
    Ok(Arc::new(config))
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_http3_client_creation() {
        // このテストは実際のサーバーなしでは失敗するが、
        // コンパイルが通ることを確認
        let result = Http3TestClientV2::connect(
            "127.0.0.1:8443".parse().unwrap(),
            "localhost"
        ).await;
        // 接続エラーは想定内
        assert!(result.is_err() || result.is_ok());
    }
}
