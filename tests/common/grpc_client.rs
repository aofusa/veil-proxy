//! gRPCテストクライアント
//!
//! HTTP/2 + gRPCフレーミングを使用したgRPCクライアントの実装

use std::net::TcpStream;
use std::io::{Read, Write};
use std::time::Duration;
use rustls::{ClientConfig, ClientConnection};
use rustls::pki_types::ServerName;
use std::sync::Arc;

/// gRPCフレーム構造体
#[derive(Debug, Clone)]
pub struct GrpcFrame {
    pub compressed: bool,
    pub data: Vec<u8>,
}

impl GrpcFrame {
    /// 新しいgRPCフレームを作成
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            compressed: false,
            data,
        }
    }
    
    /// gRPCフレームをエンコード
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(5 + self.data.len());
        // 1 byte: flags (compressed bit)
        buf.push(if self.compressed { 1 } else { 0 });
        // 4 bytes: length (big-endian)
        buf.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        // N bytes: message
        buf.extend_from_slice(&self.data);
        buf
    }
    
    /// gRPCフレームをデコード
    pub fn decode(data: &[u8]) -> Result<(Self, usize), Box<dyn std::error::Error>> {
        if data.len() < 5 {
            return Err("Insufficient data for gRPC frame header".into());
        }
        
        let compressed = (data[0] & 1) != 0;
        let length = u32::from_be_bytes([data[1], data[2], data[3], data[4]]) as usize;
        
        if data.len() < 5 + length {
            return Err(format!("Insufficient data: need {} bytes, have {}", 5 + length, data.len()).into());
        }
        
        let message = data[5..5 + length].to_vec();
        
        Ok((
            Self {
                compressed,
                data: message,
            },
            5 + length,
        ))
    }
}

/// gRPCテストクライアント
pub struct GrpcTestClient {
    tls_conn: ClientConnection,
    stream: TcpStream,
}

impl GrpcTestClient {
    /// 新しいgRPCクライアントを作成
    pub fn new(server_addr: &str, port: u16) -> Result<Self, Box<dyn std::error::Error>> {
        // TLS接続を確立
        let mut stream = TcpStream::connect(format!("{}:{}", server_addr, port))?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        stream.set_write_timeout(Some(Duration::from_secs(10)))?;
        
        // rustlsクライアント設定を作成
        let config = create_client_config();
        let server_name = ServerName::try_from(server_addr.to_string())?;
        let mut tls_conn = ClientConnection::new(config, server_name)?;
        
        // TLSハンドシェイク
        while tls_conn.is_handshaking() {
            tls_conn.complete_io(&mut stream)?;
        }
        
        Ok(Self {
            tls_conn,
            stream,
        })
    }
    
    /// gRPCリクエストを送信（HTTP/1.1経由、簡易実装）
    pub fn send_grpc_request(
        &mut self,
        path: &str,
        message: &[u8],
        metadata: &[(&str, &str)],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
        // gRPCフレームを構築
        let frame = GrpcFrame::new(message.to_vec());
        let frame_bytes = frame.encode();
        
        // HTTP/1.1リクエストを構築
        let mut request = format!(
            "POST {} HTTP/1.1\r\n\
             Host: localhost\r\n\
             Content-Type: application/grpc\r\n\
             Accept: application/grpc\r\n",
            path
        );
        
        // メタデータを追加
        for (name, value) in metadata {
            request.push_str(&format!("{}: {}\r\n", name, value));
        }
        
        request.push_str(&format!("Content-Length: {}\r\n\r\n", frame_bytes.len()));
        
        // リクエストを送信
        self.stream.write_all(request.as_bytes())?;
        self.stream.write_all(&frame_bytes)?;
        self.stream.flush()?;
        
        // レスポンスを受信
        let mut response = Vec::new();
        let mut buf = [0u8; 4096];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => response.extend_from_slice(&buf[..n]),
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(format!("Read error: {}", e).into()),
            }
        }
        
        Ok(response)
    }
    
    /// レスポンスからgRPCフレームを抽出
    pub fn extract_grpc_frame(response: &[u8]) -> Result<GrpcFrame, Box<dyn std::error::Error>> {
        // HTTPレスポンスからボディを抽出
        let body_start = response.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("No HTTP body separator found")? + 4;
        
        let body = &response[body_start..];
        
        // gRPCフレームをデコード
        let (frame, _) = GrpcFrame::decode(body)?;
        Ok(frame)
    }
    
    /// レスポンスからステータスコードを取得
    pub fn extract_status_code(response: &[u8]) -> Option<u16> {
        let response_str = std::str::from_utf8(response).ok()?;
        let status_line = response_str.lines().next()?;
        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() >= 2 {
            parts[1].parse().ok()
        } else {
            None
        }
    }
    
    /// レスポンスからgRPCステータスを取得
    pub fn extract_grpc_status(response: &[u8]) -> Option<u32> {
        let response_str = std::str::from_utf8(response).ok()?;
        for line in response_str.lines() {
            if line.starts_with("grpc-status:") {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() >= 2 {
                    return parts[1].trim().parse().ok();
                }
            }
        }
        None
    }
}

/// TLSクライアント設定を作成（自己署名証明書を許可）
fn create_client_config() -> Arc<ClientConfig> {
    use rustls::crypto::CryptoProvider;
    
    // CryptoProviderを初期化（一度だけ実行）
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        CryptoProvider::install_default(rustls::crypto::aws_lc_rs::default_provider())
            .expect("Failed to install rustls crypto provider");
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
    
    let config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    
    Arc::new(config)
}

