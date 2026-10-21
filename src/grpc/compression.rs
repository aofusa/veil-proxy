//! gRPC Compression Support
//!
//! Implements grpc-encoding compression for gRPC messages.
//! Supports gzip, deflate, and identity (no compression).
//!
//! Gzip/Deflate implementations require the `compression` feature.
//! Without it, only identity (no compression) is functional.

#[cfg(feature = "compression")]
use flate2::read::{DeflateDecoder, GzDecoder};
#[cfg(feature = "compression")]
use flate2::write::{DeflateEncoder, GzEncoder};
#[cfg(feature = "compression")]
use flate2::Compression;
#[cfg(feature = "compression")]
use std::io::{Read, Write};

use crate::grpc::framing::GrpcError;

/// gRPC compression algorithm
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum GrpcCompression {
    /// No compression
    #[default]
    Identity,
    /// Gzip compression
    Gzip,
    /// Deflate compression
    Deflate,
}

impl GrpcCompression {
    /// Parse from grpc-encoding header value
    pub fn from_bytes(value: &[u8]) -> Option<Self> {
        if value.eq_ignore_ascii_case(b"identity") {
            Some(Self::Identity)
        } else if value.eq_ignore_ascii_case(b"gzip") {
            Some(Self::Gzip)
        } else if value.eq_ignore_ascii_case(b"deflate") {
            Some(Self::Deflate)
        } else {
            None
        }
    }

    /// Get header value as bytes
    pub fn as_bytes(&self) -> &'static [u8] {
        match self {
            Self::Identity => b"identity",
            Self::Gzip => b"gzip",
            Self::Deflate => b"deflate",
        }
    }

    /// Get header value as string
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::Gzip => "gzip",
            Self::Deflate => "deflate",
        }
    }

    /// Check if this encoding requires compression
    pub fn is_compressed(&self) -> bool {
        !matches!(self, Self::Identity)
    }

    /// Compress data with this encoding
    pub fn compress(&self, data: &[u8], _level: u32) -> Result<Vec<u8>, GrpcError> {
        match self {
            Self::Identity => Ok(data.to_vec()),
            #[cfg(feature = "compression")]
            Self::Gzip => compress_gzip(data, _level),
            #[cfg(feature = "compression")]
            Self::Deflate => compress_deflate(data, _level),
            #[cfg(not(feature = "compression"))]
            _ => Ok(data.to_vec()),
        }
    }

    // B-127: `compression` feature 無しでは gzip・deflate を扱えない。
    /// Whether this build can decompress this encoding (gzip and deflate need the `compression` feature).
    pub fn is_supported(&self) -> bool {
        matches!(self, Self::Identity) || cfg!(feature = "compression")
    }

    // B-127: 全部解凍してから大きさを比べると、高圧縮率の小さなメッセージで巨大な確保をさせられる。
    /// Decompresses `data`, stopping as soon as the output would exceed `limit` bytes.
    ///
    /// At most `limit + 1` bytes are read and allocated; if the output is larger, this returns
    /// `MessageTooLarge` with `size` set to `limit + 1`.
    pub fn decompress_limited(&self, data: &[u8], limit: usize) -> Result<Vec<u8>, GrpcError> {
        match self {
            Self::Identity => {
                if data.len() > limit {
                    return Err(GrpcError::MessageTooLarge {
                        size: data.len(),
                        max: limit,
                    });
                }
                Ok(data.to_vec())
            }
            #[cfg(feature = "compression")]
            Self::Gzip => read_limited(GzDecoder::new(data), limit, "gzip"),
            #[cfg(feature = "compression")]
            Self::Deflate => read_limited(DeflateDecoder::new(data), limit, "deflate"),
            #[cfg(not(feature = "compression"))]
            _ => Err(GrpcError::CompressionError(format!(
                "{} is not supported in this build",
                self.as_str()
            ))),
        }
    }

    /// Decompress data with this encoding
    pub fn decompress(&self, data: &[u8]) -> Result<Vec<u8>, GrpcError> {
        match self {
            Self::Identity => Ok(data.to_vec()),
            #[cfg(feature = "compression")]
            Self::Gzip => decompress_gzip(data),
            #[cfg(feature = "compression")]
            Self::Deflate => decompress_deflate(data),
            #[cfg(not(feature = "compression"))]
            _ => Ok(data.to_vec()),
        }
    }
}

/// gRPC compression configuration
#[derive(Debug, Clone)]
pub struct GrpcCompressionConfig {
    /// Enabled compression algorithms
    pub enabled: Vec<GrpcCompression>,
    /// Preferred algorithm for responses
    pub preferred: GrpcCompression,
    /// Compression level (1-9, higher = better compression, slower)
    pub level: u32,
    /// Minimum message size to compress (bytes)
    pub min_size: usize,
}

impl Default for GrpcCompressionConfig {
    fn default() -> Self {
        Self {
            enabled: vec![GrpcCompression::Identity, GrpcCompression::Gzip],
            preferred: GrpcCompression::Gzip,
            level: 6,
            min_size: 1024, // Don't compress small messages
        }
    }
}

impl GrpcCompressionConfig {
    /// Create config with only identity (no compression)
    pub fn identity_only() -> Self {
        Self {
            enabled: vec![GrpcCompression::Identity],
            preferred: GrpcCompression::Identity,
            level: 0,
            min_size: 0,
        }
    }

    /// Check if an encoding is supported
    pub fn supports(&self, encoding: GrpcCompression) -> bool {
        self.enabled.contains(&encoding)
    }

    /// Get best encoding from client's accepted encodings
    pub fn negotiate(&self, accept: &[GrpcCompression]) -> GrpcCompression {
        // Prefer our preferred encoding if client supports it
        if accept.contains(&self.preferred) && self.supports(self.preferred) {
            return self.preferred;
        }

        // Otherwise find first mutually supported encoding
        for enc in accept {
            if self.supports(*enc) {
                return *enc;
            }
        }

        // Fall back to identity
        GrpcCompression::Identity
    }

    /// Get grpc-accept-encoding header value
    pub fn accept_encoding_header(&self) -> String {
        self.enabled
            .iter()
            .map(|e| e.as_str())
            .collect::<Vec<_>>()
            .join(",")
    }
}

/// Compress data with gzip
#[cfg(feature = "compression")]
fn compress_gzip(data: &[u8], level: u32) -> Result<Vec<u8>, GrpcError> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::new(level));
    encoder
        .write_all(data)
        .map_err(|e| GrpcError::CompressionError(format!("gzip compress: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| GrpcError::CompressionError(format!("gzip finish: {}", e)))
}

/// Decompress gzip data
#[cfg(feature = "compression")]
fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, GrpcError> {
    let mut decoder = GzDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| GrpcError::CompressionError(format!("gzip decompress: {}", e)))?;
    Ok(result)
}

/// 解凍器から `limit + 1` バイトまで読む（B-127）。
#[cfg(feature = "compression")]
fn read_limited<R: Read>(decoder: R, limit: usize, name: &str) -> Result<Vec<u8>, GrpcError> {
    let cap = limit.saturating_add(1);
    let mut result = Vec::new();
    decoder
        .take(cap as u64)
        .read_to_end(&mut result)
        .map_err(|e| GrpcError::CompressionError(format!("{} decompress: {}", name, e)))?;
    if result.len() > limit {
        return Err(GrpcError::MessageTooLarge {
            size: result.len(),
            max: limit,
        });
    }
    Ok(result)
}

/// Compress data with deflate
#[cfg(feature = "compression")]
fn compress_deflate(data: &[u8], level: u32) -> Result<Vec<u8>, GrpcError> {
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::new(level));
    encoder
        .write_all(data)
        .map_err(|e| GrpcError::CompressionError(format!("deflate compress: {}", e)))?;
    encoder
        .finish()
        .map_err(|e| GrpcError::CompressionError(format!("deflate finish: {}", e)))
}

/// Decompress deflate data
#[cfg(feature = "compression")]
fn decompress_deflate(data: &[u8]) -> Result<Vec<u8>, GrpcError> {
    let mut decoder = DeflateDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| GrpcError::CompressionError(format!("deflate decompress: {}", e)))?;
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(feature = "compression")]
    fn test_compression_roundtrip_gzip() {
        // Use larger, repetitive data that compresses well
        let original = b"Hello, gRPC compression! This is a test message. ".repeat(10);

        let compressed = GrpcCompression::Gzip.compress(&original, 6).unwrap();
        let decompressed = GrpcCompression::Gzip.decompress(&compressed).unwrap();

        assert_eq!(decompressed, original);
    }

    #[test]
    #[cfg(feature = "compression")]
    fn test_compression_roundtrip_deflate() {
        let original = b"Hello, gRPC compression with deflate!";

        let compressed = GrpcCompression::Deflate.compress(original, 6).unwrap();
        let decompressed = GrpcCompression::Deflate.decompress(&compressed).unwrap();

        assert_eq!(decompressed, original);
    }

    /// B-127: 高圧縮率のメッセージは上限 + 1 バイトで打ち切る（全部は解凍しない）。
    #[test]
    #[cfg(feature = "compression")]
    fn test_decompress_limited_stops_at_limit() {
        let limit = 64 * 1024;
        // 上限の 1000 倍に展開される 0 の列
        let bomb = vec![0u8; limit * 1000];
        for enc in [GrpcCompression::Gzip, GrpcCompression::Deflate] {
            let compressed = enc.compress(&bomb, 9).unwrap();
            assert!(compressed.len() < limit, "{:?}: {}", enc, compressed.len());
            match enc.decompress_limited(&compressed, limit) {
                Err(GrpcError::MessageTooLarge { size, max }) => {
                    assert_eq!(size, limit + 1);
                    assert_eq!(max, limit);
                }
                other => panic!("{:?}: {:?}", enc, other.map(|v| v.len())),
            }
            // 上限ちょうどは通る
            let exact = vec![7u8; limit];
            let c = enc.compress(&exact, 6).unwrap();
            assert_eq!(enc.decompress_limited(&c, limit).unwrap(), exact);
        }
    }

    #[test]
    fn test_decompress_limited_identity() {
        assert_eq!(
            GrpcCompression::Identity
                .decompress_limited(b"abc", 3)
                .unwrap(),
            b"abc"
        );
        assert!(matches!(
            GrpcCompression::Identity.decompress_limited(b"abcd", 3),
            Err(GrpcError::MessageTooLarge { size: 4, max: 3 })
        ));
    }

    #[test]
    fn test_is_supported() {
        assert!(GrpcCompression::Identity.is_supported());
        assert_eq!(
            GrpcCompression::Gzip.is_supported(),
            cfg!(feature = "compression")
        );
    }

    #[test]
    fn test_identity_no_change() {
        let original = b"No compression";

        let result = GrpcCompression::Identity.compress(original, 0).unwrap();
        assert_eq!(result, original);

        let decompressed = GrpcCompression::Identity.decompress(original).unwrap();
        assert_eq!(decompressed, original);
    }

    #[test]
    fn test_config_negotiation() {
        let config = GrpcCompressionConfig::default();

        // Preferred is gzip
        let accept = vec![GrpcCompression::Identity, GrpcCompression::Gzip];
        assert_eq!(config.negotiate(&accept), GrpcCompression::Gzip);

        // Client only accepts identity
        let accept = vec![GrpcCompression::Identity];
        assert_eq!(config.negotiate(&accept), GrpcCompression::Identity);

        // Empty accept list -> identity
        let accept: Vec<GrpcCompression> = vec![];
        assert_eq!(config.negotiate(&accept), GrpcCompression::Identity);
    }

    #[test]
    fn test_compression_from_bytes() {
        assert_eq!(
            GrpcCompression::from_bytes(b"gzip"),
            Some(GrpcCompression::Gzip)
        );
        assert_eq!(
            GrpcCompression::from_bytes(b"GZIP"),
            Some(GrpcCompression::Gzip)
        );
        assert_eq!(
            GrpcCompression::from_bytes(b"deflate"),
            Some(GrpcCompression::Deflate)
        );
        assert_eq!(
            GrpcCompression::from_bytes(b"identity"),
            Some(GrpcCompression::Identity)
        );
        assert_eq!(GrpcCompression::from_bytes(b"unknown"), None);
    }
}
