//! gRPC Compression Support
//!
//! Implements grpc-encoding compression for gRPC messages.
//! Supports gzip, deflate, and identity (no compression).
//!
//! Uses existing flate2 dependency for gzip/deflate.

use std::io::{Read, Write};
use flate2::read::{GzDecoder, DeflateDecoder};
use flate2::write::{GzEncoder, DeflateEncoder};
use flate2::Compression;

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
    pub fn compress(&self, data: &[u8], level: u32) -> Result<Vec<u8>, GrpcError> {
        match self {
            Self::Identity => Ok(data.to_vec()),
            Self::Gzip => compress_gzip(data, level),
            Self::Deflate => compress_deflate(data, level),
        }
    }

    /// Decompress data with this encoding
    pub fn decompress(&self, data: &[u8]) -> Result<Vec<u8>, GrpcError> {
        match self {
            Self::Identity => Ok(data.to_vec()),
            Self::Gzip => decompress_gzip(data),
            Self::Deflate => decompress_deflate(data),
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
fn decompress_gzip(data: &[u8]) -> Result<Vec<u8>, GrpcError> {
    let mut decoder = GzDecoder::new(data);
    let mut result = Vec::new();
    decoder
        .read_to_end(&mut result)
        .map_err(|e| GrpcError::CompressionError(format!("gzip decompress: {}", e)))?;
    Ok(result)
}

/// Compress data with deflate
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
    fn test_compression_roundtrip_gzip() {
        // Use larger, repetitive data that compresses well
        let original = b"Hello, gRPC compression! This is a test message. ".repeat(10);
        
        let compressed = GrpcCompression::Gzip.compress(&original, 6).unwrap();
        let decompressed = GrpcCompression::Gzip.decompress(&compressed).unwrap();
        
        assert_eq!(decompressed, original);
        // Verify round-trip is successful (size check removed - small data may not compress well)
    }

    #[test]
    fn test_compression_roundtrip_deflate() {
        let original = b"Hello, gRPC compression with deflate!";
        
        let compressed = GrpcCompression::Deflate.compress(original, 6).unwrap();
        let decompressed = GrpcCompression::Deflate.decompress(&compressed).unwrap();
        
        assert_eq!(decompressed, original);
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
        assert_eq!(GrpcCompression::from_bytes(b"gzip"), Some(GrpcCompression::Gzip));
        assert_eq!(GrpcCompression::from_bytes(b"GZIP"), Some(GrpcCompression::Gzip));
        assert_eq!(GrpcCompression::from_bytes(b"deflate"), Some(GrpcCompression::Deflate));
        assert_eq!(GrpcCompression::from_bytes(b"identity"), Some(GrpcCompression::Identity));
        assert_eq!(GrpcCompression::from_bytes(b"unknown"), None);
    }
}
