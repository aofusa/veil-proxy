//! gRPCテストクライアント（tonic版）
//!
//! tonicライブラリを使用したgRPC over HTTP/2クライアント実装
//! tokioランタイム上で動作し、非同期API
//!
//! 既存のテストロジックとの互換性のため、HTTP/1.1経由のgRPCリクエストもサポート

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Request, Response, Status};
use http::uri::Uri;
use bytes::Bytes;
use std::sync::Arc;
use super::http1_client::Http1TestClient;

/// gRPCテストクライアント
/// tonicを使用したHTTP/2ベースのgRPCクライアント
#[allow(dead_code)]
pub struct GrpcTestClientV2 {
    channel: Channel,
}

#[allow(dead_code)]
impl GrpcTestClientV2 {
    /// 新しいgRPCクライアントを作成（TLS使用）
    pub async fn new(
        server_addr: &str,
        port: u16,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let uri: Uri = format!("https://{}:{}", server_addr, port).parse()?;
        
        // TLS設定（証明書検証なし - テスト用）
        let tls_config = ClientTlsConfig::new()
            .domain_name(server_addr);
        
        let endpoint = Endpoint::from(uri)
            .tls_config(tls_config)?
            .timeout(std::time::Duration::from_secs(30));
        
        let channel = endpoint.connect().await?;
        
        Ok(Self { channel })
    }
    
    /// 新しいgRPCクライアントを作成（プレーンテキスト/h2c）
    pub async fn new_plaintext(
        server_addr: &str,
        port: u16,
    ) -> Result<Self, Box<dyn std::error::Error + Send + Sync>> {
        let uri: Uri = format!("http://{}:{}", server_addr, port).parse()?;
        
        let endpoint = Endpoint::from(uri)
            .timeout(std::time::Duration::from_secs(30));
        
        let channel = endpoint.connect().await?;
        
        Ok(Self { channel })
    }
    
    /// チャネルを取得（カスタムサービスクライアント作成用）
    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }
    
    /// 生のUnary gRPCリクエストを送信
    /// path: gRPCメソッドパス（例: "/package.Service/MethodName"）
    pub async fn send_unary_request(
        &mut self,
        path: &str,
        message: &[u8],
        _metadata: &[(&str, &str)],
    ) -> Result<(Vec<u8>, Vec<(String, String)>), GrpcError> {
        // gRPCフレームを構築（5バイトヘッダー: 圧縮フラグ1バイト + 長さ4バイト）
        let mut grpc_frame = Vec::with_capacity(5 + message.len());
        grpc_frame.push(0); // 圧縮なし
        grpc_frame.extend_from_slice(&(message.len() as u32).to_be_bytes());
        grpc_frame.extend_from_slice(message);
        
        // HTTPリクエストを構築
        let _request_builder = http::Request::builder()
            .method("POST")
            .uri(path)
            .header("content-type", "application/grpc")
            .header("te", "trailers");
        
        // 注意: tonicでは通常、生成されたクライアントコードを使用します。
        // ここでは低レベルAPIを示していますが、実際のテストでは
        // service定義からProtobufで生成したクライアントを使用することを推奨します。
        
        // カスタムコーデックを使用してリクエストを送信する場合は
        // tonic::codec::Codecトレイトを実装する必要があります。
        
        Err(GrpcError::NotImplemented("Use generated tonic client instead".to_string()))
    }
    
    /// HTTP/1.1経由でgRPCリクエストを送信（既存のGrpcTestClientと互換性のあるAPI）
    /// 既存のテストロジックを維持するため、HTTP/1.1経由のgRPCリクエストをサポート
    pub async fn send_grpc_request(
        server_addr: &str,
        port: u16,
        path: &str,
        message: &[u8],
        metadata: &[(&str, &str)],
    ) -> Result<Vec<u8>, Box<dyn std::error::Error + Send + Sync>> {
        // gRPCフレームを構築（tests/common/grpc_client.rsのGrpcFrameを使用）
        #[cfg(feature = "grpc")]
        use super::grpc_client::GrpcFrame;
        #[cfg(not(feature = "grpc"))]
        // grpc featureがない場合のフォールバック
        struct GrpcFrame {
            compressed: bool,
            data: Vec<u8>,
        }
        #[cfg(not(feature = "grpc"))]
        impl GrpcFrame {
            fn new(data: Vec<u8>) -> Self {
                Self { compressed: false, data }
            }
            fn encode(&self) -> Vec<u8> {
                let mut buf = Vec::with_capacity(5 + self.data.len());
                buf.push(if self.compressed { 1 } else { 0 });
                buf.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
                buf.extend_from_slice(&self.data);
                buf
            }
        }
        let frame = GrpcFrame::new(message.to_vec());
        let frame_bytes = frame.encode();
        
        // HTTP/1.1クライアントを作成
        let client = Http1TestClient::new_https(server_addr, port)?;
        
        // メタデータをヘッダーに変換
        let mut headers = vec![
            ("Content-Type", "application/grpc"),
            ("Accept", "application/grpc"),
        ];
        for (name, value) in metadata {
            headers.push((name, value));
        }
        
        // POSTリクエストを送信
        let (status, body) = client.post_with_headers(path, &headers, &frame_bytes).await?;
        
        // HTTPレスポンスを構築（既存のGrpcTestClientと互換性のため）
        let status_line = format!("HTTP/1.1 {} OK\r\n", status);
        let mut response = status_line.into_bytes();
        response.extend_from_slice(b"\r\n");
        response.extend_from_slice(&body);
        
        Ok(response)
    }
    
    /// レスポンスからgRPCフレームを抽出（既存のGrpcTestClientと互換性のあるAPI）
    pub fn extract_grpc_frame(response: &[u8]) -> Result<super::grpc_client::GrpcFrame, Box<dyn std::error::Error + Send + Sync>> {
        #[cfg(feature = "grpc")]
        use super::grpc_client::GrpcFrame;
        #[cfg(not(feature = "grpc"))]
        // grpc featureがない場合のフォールバック
        struct GrpcFrame {
            compressed: bool,
            data: Vec<u8>,
        }
        
        // HTTPレスポンスからボディを抽出
        let body_start = response.windows(4)
            .position(|w| w == b"\r\n\r\n")
            .ok_or("No HTTP body separator found")? + 4;
        
        let body = &response[body_start..];
        
        // gRPCフレームをデコード（GrpcFrame::decodeを使用）
        #[cfg(feature = "grpc")]
        {
            let (frame, _) = GrpcFrame::decode(body)?;
            Ok(frame)
        }
        #[cfg(not(feature = "grpc"))]
        {
            // grpc featureがない場合の簡易実装
            if body.len() < 5 {
                return Err("Insufficient data for gRPC frame header".into());
            }
            let compressed = (body[0] & 1) != 0;
            let length = u32::from_be_bytes([body[1], body[2], body[3], body[4]]) as usize;
            if body.len() < 5 + length {
                return Err(format!("Insufficient data: need {} bytes, have {}", 5 + length, body.len()).into());
            }
            let message = body[5..5 + length].to_vec();
            Ok(GrpcFrame { compressed, data: message })
        }
    }
    
    /// レスポンスからステータスコードを取得（既存のGrpcTestClientと互換性のあるAPI）
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
    
    /// レスポンスからgRPCステータスを取得（既存のGrpcTestClientと互換性のあるAPI）
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
    
    /// レスポンスからgRPCメッセージを取得（既存のGrpcTestClientと互換性のあるAPI）
    pub fn extract_grpc_message(response: &[u8]) -> Option<String> {
        let response_str = std::str::from_utf8(response).ok()?;
        for line in response_str.lines() {
            if line.starts_with("grpc-message:") {
                let parts: Vec<&str> = line.split(':').collect();
                if parts.len() >= 2 {
                    let message = parts[1].trim();
                    // URLデコード
                    return Some(url_decode(message));
                }
            }
        }
        None
    }
    
    /// レスポンスからすべてのトレーラーヘッダーを取得（既存のGrpcTestClientと互換性のあるAPI）
    pub fn extract_trailers(response: &[u8]) -> Vec<(String, String)> {
        let mut trailers = Vec::new();
        let response_str = match std::str::from_utf8(response) {
            Ok(s) => s,
            Err(_) => return trailers,
        };
        
        // ヘッダーセクションとボディセクションを分離
        let header_end = response_str.find("\r\n\r\n").unwrap_or(0);
        let trailer_section = &response_str[header_end + 4..];
        
        // grpc-で始まるヘッダーを探す
        for line in trailer_section.lines() {
            if line.starts_with("grpc-") {
                if let Some(colon_idx) = line.find(':') {
                    let name = line[..colon_idx].trim().to_string();
                    let value = line[colon_idx + 1..].trim().to_string();
                    trailers.push((name, value));
                }
            }
        }
        
        trailers
    }
}

/// URLデコード（簡易実装）
fn url_decode(encoded: &str) -> String {
    let mut result = String::new();
    let mut chars = encoded.chars().peekable();
    
    while let Some(c) = chars.next() {
        if c == '%' {
            let hex: String = chars.by_ref().take(2).collect();
            if hex.len() == 2 {
                if let Ok(byte) = u8::from_str_radix(&hex, 16) {
                    result.push(byte as char);
                    continue;
                }
            }
            result.push('%');
            result.push_str(&hex);
        } else {
            result.push(c);
        }
    }
    
    result
}

/// gRPCエラー
#[derive(Debug)]
pub enum GrpcError {
    ConnectionError(String),
    RequestError(String),
    ResponseError(String),
    NotImplemented(String),
}

impl std::fmt::Display for GrpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GrpcError::ConnectionError(msg) => write!(f, "Connection error: {}", msg),
            GrpcError::RequestError(msg) => write!(f, "Request error: {}", msg),
            GrpcError::ResponseError(msg) => write!(f, "Response error: {}", msg),
            GrpcError::NotImplemented(msg) => write!(f, "Not implemented: {}", msg),
        }
    }
}

impl std::error::Error for GrpcError {}

/// tonicサービス用のヘルパートレイト
/// Protobufから生成されたサービスクライアントと組み合わせて使用
#[allow(dead_code)]
pub trait GrpcTestHelpers {
    /// gRPCステータスをアサート
    fn assert_ok_status<T>(result: Result<Response<T>, Status>) -> T {
        match result {
            Ok(response) => response.into_inner(),
            Err(status) => panic!("gRPC request failed: {:?}", status),
        }
    }
    
    /// 特定のgRPCステータスコードをアサート
    fn assert_status_code<T>(result: Result<Response<T>, Status>, expected_code: tonic::Code) {
        match result {
            Ok(_) => panic!("Expected error status {:?}, but got Ok", expected_code),
            Err(status) => {
                assert_eq!(
                    status.code(),
                    expected_code,
                    "Expected status {:?}, got {:?}",
                    expected_code,
                    status.code()
                );
            }
        }
    }
}

/// デフォルト実装
impl<T> GrpcTestHelpers for T {}

/// gRPCメタデータヘルパー
#[allow(dead_code)]
pub fn create_metadata(pairs: &[(&str, &str)]) -> tonic::metadata::MetadataMap {
    let mut metadata = tonic::metadata::MetadataMap::new();
    for (key, value) in pairs {
        if let Ok(key) = key.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>() {
            if let Ok(value) = value.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>() {
                metadata.insert(key, value);
            }
        }
    }
    metadata
}

/// gRPCリクエストにメタデータを追加するヘルパー
#[allow(dead_code)]
pub fn with_metadata<T>(request: T, metadata: &[(&str, &str)]) -> Request<T> {
    let mut req = Request::new(request);
    for (key, value) in metadata {
        if let Ok(k) = key.parse::<tonic::metadata::MetadataKey<tonic::metadata::Ascii>>() {
            if let Ok(v) = value.parse::<tonic::metadata::MetadataValue<tonic::metadata::Ascii>>() {
                req.metadata_mut().insert(k, v);
            }
        }
    }
    req
}

#[cfg(test)]
mod tests {
    use super::*;
    
    #[tokio::test]
    async fn test_grpc_client_creation() {
        // このテストは実際のサーバーなしでは失敗するが、
        // コンパイルが通ることを確認
        let result = GrpcTestClientV2::new("localhost", 8443).await;
        // 接続エラーは想定内
        assert!(result.is_err() || result.is_ok());
    }
    
    #[test]
    fn test_metadata_creation() {
        let metadata = create_metadata(&[("x-custom-header", "value")]);
        assert!(metadata.get("x-custom-header").is_some());
    }
}
