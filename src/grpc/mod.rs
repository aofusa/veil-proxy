//! gRPC Protocol Support for Veil-Proxy
//!
//! gRPC ワイヤプロトコル（framing, headers, trailers）を実装します。
//! tonic を使用せず、monoio + HTTP/2 基盤上でネイティブに動作します。
//!
//! ## 機能
//!
//! - **Framing**: 5-byte gRPC メッセージフレーミング
//! - **Headers**: gRPC 固有ヘッダー（grpc-timeout, grpc-encoding 等）
//! - **Trailers**: grpc-status, grpc-message による応答ステータス
//! - **Compression**: gzip/deflate 圧縮サポート
//!
//! ## 使用例
//!
//! ```rust,ignore
//! use grpc::{is_grpc_request, GrpcFrame, GrpcStatusCode};
//!
//! if is_grpc_request(content_type) {
//!     let frame = GrpcFrame::decode(&data)?;
//!     // Process gRPC message...
//! }
//! ```

pub mod framing;
pub mod status;
pub mod headers;

#[cfg(feature = "grpc")]
pub mod stream;

#[cfg(feature = "grpc")]
pub mod compression;

#[cfg(feature = "grpc-web")]
pub mod web;

// Re-exports
pub use framing::{GrpcFrame, GrpcFrameDecoder, GrpcError};
pub use status::{GrpcStatus, GrpcStatusCode};
pub use headers::{is_grpc_request, is_grpc_content_type, GrpcHeaders, parse_grpc_timeout};

#[cfg(feature = "grpc")]
pub use stream::{GrpcStreamState, GrpcStreamType};

#[cfg(feature = "grpc")]
pub use compression::GrpcCompression;
