//! HTTP/3 バックエンド共通型・ヘルパ（ngtcp2 / quiche 非依存）

use crate::config::{AcceptedEncoding, CompressionConfig};

/// HTTP/3 サーバー設定（両バックエンド共通）
#[derive(Clone)]
pub struct Http3ServerConfig {
    pub cert_path: String,
    pub key_path: String,
    pub cert_pem: Option<Vec<u8>>,
    pub key_pem: Option<Vec<u8>>,
    pub max_idle_timeout: u64,
    pub max_udp_payload_size: u64,
    pub initial_max_data: u64,
    pub initial_max_stream_data_bidi_local: u64,
    pub initial_max_stream_data_bidi_remote: u64,
    pub initial_max_stream_data_uni: u64,
    pub initial_max_streams_bidi: u64,
    pub initial_max_streams_uni: u64,
    pub gso_gro_enabled: bool,
}

impl Default for Http3ServerConfig {
    fn default() -> Self {
        Self {
            cert_path: String::new(),
            key_path: String::new(),
            cert_pem: None,
            key_pem: None,
            max_idle_timeout: 30000,
            max_udp_payload_size: 1350,
            initial_max_data: 10_000_000,
            initial_max_stream_data_bidi_local: 1_000_000,
            initial_max_stream_data_bidi_remote: 1_000_000,
            initial_max_stream_data_uni: 1_000_000,
            initial_max_streams_bidi: 100,
            initial_max_streams_uni: 100,
            gso_gro_enabled: false,
        }
    }
}

/// バックエンドプロキシ結果（バッファ経路）
pub struct BackendProxyResult {
    pub status_code: u16,
    pub body: Vec<u8>,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub trailers: Vec<(Vec<u8>, Vec<u8>)>,
}

/// 平文バッファをセキュアにゼロ化
pub fn secure_zero(data: &mut [u8]) {
    for byte in data.iter_mut() {
        unsafe {
            std::ptr::write_volatile(byte, 0);
        }
    }
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
}

/// HTTP/3 用レスポンスボディ圧縮
#[cfg(feature = "compression")]
pub fn compress_body_h3(
    body: &[u8],
    encoding: AcceptedEncoding,
    compression: &CompressionConfig,
) -> Vec<u8> {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    match encoding {
        AcceptedEncoding::Zstd => {
            match zstd::encode_all(std::io::Cursor::new(body), compression.zstd_level) {
                Ok(compressed) => compressed,
                Err(_) => body.to_vec(),
            }
        }
        AcceptedEncoding::Gzip => {
            let level = Compression::new(compression.gzip_level);
            let mut encoder = GzEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Brotli => {
            let mut compressed = Vec::with_capacity(body.len());
            let params = brotli::enc::BrotliEncoderParams {
                quality: compression.brotli_level as i32,
                ..Default::default()
            };
            let mut input = std::io::Cursor::new(body);
            if brotli::BrotliCompress(&mut input, &mut compressed, &params).is_err() {
                return body.to_vec();
            }
            compressed
        }
        AcceptedEncoding::Deflate => {
            use flate2::write::DeflateEncoder;
            let level = Compression::new(compression.gzip_level);
            let mut encoder = DeflateEncoder::new(Vec::with_capacity(body.len()), level);
            if encoder.write_all(body).is_err() {
                return body.to_vec();
            }
            encoder.finish().unwrap_or_else(|_| body.to_vec())
        }
        AcceptedEncoding::Identity => body.to_vec(),
    }
}

#[cfg(not(feature = "compression"))]
#[inline]
pub fn compress_body_h3(
    body: &[u8],
    _encoding: AcceptedEncoding,
    _compression: &CompressionConfig,
) -> Vec<u8> {
    body.to_vec()
}

/// バッファ経路プロキシは quiche 実装側に残している。
/// ngtcp2 はストリーミング経路（`http3::stream`）と簡易プロキシを server 内で行う。
pub async fn proxy_to_backend_async_with_tls(
    _target: &crate::config::ProxyTarget,
    _request: Vec<u8>,
    _timeout_secs: u64,
    _tls_insecure: bool,
) -> std::io::Result<BackendProxyResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "use backend-specific proxy path",
    ))
}
