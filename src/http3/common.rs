//! HTTP/3 バックエンド共通型・ヘルパ（ngtcp2 / quiche 非依存）

use crate::config::{AcceptedEncoding, CompressionConfig, ProxyTarget};

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
            // 大容量ボディ E2E（1.2–1.5MB 超）でも初回ウィンドウ内に収まるよう余裕を持たせる。
            // フロー制御の更新経路も動くが、debug ビルドでの往復タイムアウトを避ける。
            initial_max_data: 50_000_000,
            initial_max_stream_data_bidi_local: 10_000_000,
            initial_max_stream_data_bidi_remote: 10_000_000,
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

/// 上流リクエストパスを構築する。
pub fn compute_upstream_request_path(
    path_str: &str,
    prefix: &[u8],
    target_path_prefix: &str,
    preserve_full_path: bool,
) -> String {
    if preserve_full_path {
        return if path_str.is_empty() {
            "/".to_string()
        } else {
            path_str.to_string()
        };
    }

    let sub_path = if prefix.is_empty() {
        path_str.to_string()
    } else {
        let prefix_str = std::str::from_utf8(prefix).unwrap_or("");
        if let Some(remaining) = path_str.strip_prefix(prefix_str) {
            let base = target_path_prefix.trim_end_matches('/');
            if remaining.is_empty() {
                if base.is_empty() {
                    "/".to_string()
                } else {
                    format!("{}/", base)
                }
            } else if remaining.starts_with('/') {
                if base.is_empty() {
                    remaining.to_string()
                } else {
                    format!("{}{}", base, remaining)
                }
            } else if base.is_empty() {
                format!("/{}", remaining)
            } else {
                format!("{}/{}", base, remaining)
            }
        } else {
            path_str.to_string()
        }
    };
    if sub_path.is_empty() {
        "/".to_string()
    } else {
        sub_path
    }
}

/// HTTP/1.1 リクエスト head（リクエストライン + ヘッダ + Connection: close、末尾空行なし）。
/// ボディフレーミングは呼び出し側で付与する。
pub fn build_h1_request_head(
    target: &ProxyTarget,
    method: &[u8],
    final_path: &str,
    headers: &[(Vec<u8>, Vec<u8>)],
) -> Vec<u8> {
    let mut req = Vec::with_capacity(512);
    req.extend_from_slice(method);
    req.push(b' ');
    req.extend_from_slice(final_path.as_bytes());
    req.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    req.extend_from_slice(target.host.as_bytes());
    if !target.is_default_port() {
        req.push(b':');
        let mut port_buf = itoa::Buffer::new();
        req.extend_from_slice(port_buf.format(target.port).as_bytes());
    }
    req.extend_from_slice(b"\r\n");

    for (name, value) in headers {
        if name.starts_with(b":")
            || name.eq_ignore_ascii_case(b"connection")
            || name.eq_ignore_ascii_case(b"keep-alive")
            || name.eq_ignore_ascii_case(b"transfer-encoding")
            || name.eq_ignore_ascii_case(b"content-length")
            || name.eq_ignore_ascii_case(b"expect")
            || name.eq_ignore_ascii_case(b"host")
        {
            continue;
        }
        req.extend_from_slice(name);
        req.extend_from_slice(b": ");
        req.extend_from_slice(value);
        req.extend_from_slice(b"\r\n");
    }
    req.extend_from_slice(b"Connection: close\r\n");
    req
}

/// バッファ経路用スタブ（バックエンド固有実装を優先）。
pub async fn proxy_to_backend_async_with_tls(
    _target: &ProxyTarget,
    _request: Vec<u8>,
    _timeout_secs: u64,
    _tls_insecure: bool,
) -> std::io::Result<BackendProxyResult> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "use backend-specific proxy path",
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_path_strips_prefix() {
        assert_eq!(
            compute_upstream_request_path("/api/v1/x", b"/api", "", false),
            "/v1/x"
        );
        assert_eq!(
            compute_upstream_request_path("/health", b"", "", false),
            "/health"
        );
    }
}
