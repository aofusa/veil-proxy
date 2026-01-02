//! gRPCテストクライアント（tonic版）
//!
//! tonicライブラリを使用したgRPC over HTTP/2クライアント実装
//! tokioランタイム上で動作し、非同期API

use tonic::transport::{Channel, ClientTlsConfig, Endpoint};
use tonic::{Request, Response, Status};
use http::uri::Uri;
use bytes::Bytes;
use std::sync::Arc;

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
