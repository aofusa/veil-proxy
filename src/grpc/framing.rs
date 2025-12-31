//! gRPC Message Framing (5-byte header)
//!
//! gRPC では各メッセージに 5 バイトのヘッダーが付与されます:
//!
//! ```text
//! +------------------+
//! | 1 byte: flags    |  bit 0 = compressed (0/1)
//! +------------------+
//! | 4 bytes: length  |  big-endian message length
//! +------------------+
//! | N bytes: message |  Protobuf or raw bytes
//! +------------------+
//! ```
//!
//! ## 使用例
//!
//! ```rust,ignore
//! use grpc::framing::{GrpcFrame, decode_grpc_frame};
//!
//! // Decode a gRPC frame
//! let (frame, consumed) = decode_grpc_frame(&buffer)?;
//! println!("Compressed: {}, Size: {}", frame.compressed, frame.data.len());
//!
//! // Encode a gRPC frame
//! let encoded = frame.encode();
//! ```

use std::io;

/// gRPC framing header size (1 byte flags + 4 bytes length)
pub const GRPC_FRAME_HEADER_SIZE: usize = 5;

/// Maximum gRPC message size (default: 4MB)
pub const GRPC_MAX_MESSAGE_SIZE: usize = 4 * 1024 * 1024;

/// gRPC error types
#[derive(Debug, Clone)]
pub enum GrpcError {
    /// Insufficient data in buffer
    InsufficientData {
        /// Required bytes
        required: usize,
        /// Available bytes
        available: usize,
    },
    /// Message size exceeds limit
    MessageTooLarge {
        /// Actual size
        size: usize,
        /// Maximum allowed
        max: usize,
    },
    /// Invalid frame format
    InvalidFrame(String),
    /// Compression error
    CompressionError(String),
    /// IO error
    Io(io::ErrorKind),
}

impl std::fmt::Display for GrpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InsufficientData { required, available } => {
                write!(f, "Insufficient data: need {} bytes, have {}", required, available)
            }
            Self::MessageTooLarge { size, max } => {
                write!(f, "Message too large: {} bytes (max: {})", size, max)
            }
            Self::InvalidFrame(msg) => write!(f, "Invalid gRPC frame: {}", msg),
            Self::CompressionError(msg) => write!(f, "Compression error: {}", msg),
            Self::Io(kind) => write!(f, "IO error: {:?}", kind),
        }
    }
}

impl std::error::Error for GrpcError {}

impl From<io::Error> for GrpcError {
    fn from(e: io::Error) -> Self {
        Self::Io(e.kind())
    }
}

/// gRPC message frame
#[derive(Debug, Clone)]
pub struct GrpcFrame {
    /// Compression flag (bit 0 of flags byte)
    pub compressed: bool,
    /// Message payload (Protobuf or raw bytes)
    pub data: Vec<u8>,
}

impl GrpcFrame {
    /// Create a new uncompressed frame
    pub fn new(data: Vec<u8>) -> Self {
        Self {
            compressed: false,
            data,
        }
    }

    /// Create a new compressed frame
    pub fn compressed(data: Vec<u8>) -> Self {
        Self {
            compressed: true,
            data,
        }
    }

    /// Encode frame to bytes
    ///
    /// Returns 5-byte header + payload
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(GRPC_FRAME_HEADER_SIZE + self.data.len());
        
        // Flags byte: bit 0 = compressed
        buf.push(if self.compressed { 1 } else { 0 });
        
        // Length (4 bytes, big-endian)
        buf.extend_from_slice(&(self.data.len() as u32).to_be_bytes());
        
        // Payload
        buf.extend_from_slice(&self.data);
        
        buf
    }

    /// Encode frame header only (for streaming)
    pub fn encode_header(&self) -> [u8; 5] {
        let mut header = [0u8; 5];
        header[0] = if self.compressed { 1 } else { 0 };
        header[1..5].copy_from_slice(&(self.data.len() as u32).to_be_bytes());
        header
    }

    /// Get total frame size (header + payload)
    pub fn total_size(&self) -> usize {
        GRPC_FRAME_HEADER_SIZE + self.data.len()
    }
}

/// Decode a single gRPC frame from buffer
///
/// Returns (frame, bytes_consumed) on success.
/// Returns InsufficientData error if buffer doesn't contain complete frame.
pub fn decode_grpc_frame(buf: &[u8]) -> Result<(GrpcFrame, usize), GrpcError> {
    decode_grpc_frame_with_max_size(buf, GRPC_MAX_MESSAGE_SIZE)
}

/// Decode a single gRPC frame with custom max size
pub fn decode_grpc_frame_with_max_size(
    buf: &[u8],
    max_size: usize,
) -> Result<(GrpcFrame, usize), GrpcError> {
    // Check header is available
    if buf.len() < GRPC_FRAME_HEADER_SIZE {
        return Err(GrpcError::InsufficientData {
            required: GRPC_FRAME_HEADER_SIZE,
            available: buf.len(),
        });
    }

    // Parse header
    let compressed = buf[0] == 1;
    let length = u32::from_be_bytes([buf[1], buf[2], buf[3], buf[4]]) as usize;

    // Check size limit
    if length > max_size {
        return Err(GrpcError::MessageTooLarge {
            size: length,
            max: max_size,
        });
    }

    // Check payload is available
    let total_len = GRPC_FRAME_HEADER_SIZE + length;
    if buf.len() < total_len {
        return Err(GrpcError::InsufficientData {
            required: total_len,
            available: buf.len(),
        });
    }

    // Extract payload
    let data = buf[GRPC_FRAME_HEADER_SIZE..total_len].to_vec();

    Ok((GrpcFrame { compressed, data }, total_len))
}

/// Streaming gRPC frame decoder
///
/// Handles partial reads and buffers incomplete frames.
#[derive(Debug)]
pub struct GrpcFrameDecoder {
    /// Internal buffer for incomplete frames
    buffer: Vec<u8>,
    /// Maximum message size
    max_message_size: usize,
}

impl GrpcFrameDecoder {
    /// Create new decoder with default max size
    pub fn new() -> Self {
        Self::with_max_size(GRPC_MAX_MESSAGE_SIZE)
    }

    /// Create decoder with custom max size
    pub fn with_max_size(max_size: usize) -> Self {
        Self {
            buffer: Vec::with_capacity(8192),
            max_message_size: max_size,
        }
    }

    /// Add data to internal buffer
    pub fn push(&mut self, data: &[u8]) {
        self.buffer.extend_from_slice(data);
    }

    /// Try to decode the next complete frame
    ///
    /// Returns None if no complete frame is available.
    pub fn decode_next(&mut self) -> Result<Option<GrpcFrame>, GrpcError> {
        if self.buffer.len() < GRPC_FRAME_HEADER_SIZE {
            return Ok(None);
        }

        match decode_grpc_frame_with_max_size(&self.buffer, self.max_message_size) {
            Ok((frame, consumed)) => {
                // Remove consumed bytes from buffer
                self.buffer.drain(..consumed);
                Ok(Some(frame))
            }
            Err(GrpcError::InsufficientData { .. }) => Ok(None),
            Err(e) => Err(e),
        }
    }

    /// Check if buffer has any pending data
    pub fn has_pending(&self) -> bool {
        !self.buffer.is_empty()
    }

    /// Get pending buffer length
    pub fn pending_len(&self) -> usize {
        self.buffer.len()
    }

    /// Clear the internal buffer
    pub fn clear(&mut self) {
        self.buffer.clear();
    }
}

impl Default for GrpcFrameDecoder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_uncompressed() {
        let original = GrpcFrame::new(b"Hello, gRPC!".to_vec());
        let encoded = original.encode();

        let (decoded, consumed) = decode_grpc_frame(&encoded).unwrap();

        assert_eq!(consumed, encoded.len());
        assert!(!decoded.compressed);
        assert_eq!(decoded.data, b"Hello, gRPC!");
    }

    #[test]
    fn test_encode_decode_compressed() {
        let original = GrpcFrame::compressed(b"Compressed data".to_vec());
        let encoded = original.encode();

        let (decoded, _) = decode_grpc_frame(&encoded).unwrap();

        assert!(decoded.compressed);
        assert_eq!(decoded.data, b"Compressed data");
    }

    #[test]
    fn test_decode_insufficient_header() {
        let buf = [0u8; 3]; // Less than 5 bytes
        let result = decode_grpc_frame(&buf);

        assert!(matches!(result, Err(GrpcError::InsufficientData { .. })));
    }

    #[test]
    fn test_decode_insufficient_payload() {
        let mut buf = vec![0u8; 5];
        buf[1..5].copy_from_slice(&100u32.to_be_bytes()); // Claim 100 bytes
        // But only provide header

        let result = decode_grpc_frame(&buf);
        assert!(matches!(result, Err(GrpcError::InsufficientData { .. })));
    }

    #[test]
    fn test_decode_message_too_large() {
        let mut buf = vec![0u8; 5];
        buf[1..5].copy_from_slice(&(GRPC_MAX_MESSAGE_SIZE as u32 + 1).to_be_bytes());

        let result = decode_grpc_frame(&buf);
        assert!(matches!(result, Err(GrpcError::MessageTooLarge { .. })));
    }

    #[test]
    fn test_frame_decoder_streaming() {
        let mut decoder = GrpcFrameDecoder::new();

        // First frame
        let frame1 = GrpcFrame::new(b"Frame 1".to_vec());
        let frame2 = GrpcFrame::new(b"Frame 2".to_vec());

        let mut data = frame1.encode();
        data.extend(frame2.encode());

        // Push partial data
        decoder.push(&data[..10]);
        assert!(decoder.decode_next().unwrap().is_none());

        // Push remaining data
        decoder.push(&data[10..]);

        // Decode first frame
        let decoded1 = decoder.decode_next().unwrap().unwrap();
        assert_eq!(decoded1.data, b"Frame 1");

        // Decode second frame
        let decoded2 = decoder.decode_next().unwrap().unwrap();
        assert_eq!(decoded2.data, b"Frame 2");

        // No more frames
        assert!(decoder.decode_next().unwrap().is_none());
        assert!(!decoder.has_pending());
    }

    #[test]
    fn test_encode_header() {
        let frame = GrpcFrame::new(vec![0u8; 100]);
        let header = frame.encode_header();

        assert_eq!(header[0], 0); // Uncompressed
        assert_eq!(u32::from_be_bytes([header[1], header[2], header[3], header[4]]), 100);
    }

    #[test]
    fn test_empty_frame() {
        let frame = GrpcFrame::new(vec![]);
        let encoded = frame.encode();

        assert_eq!(encoded.len(), 5); // Just header
        assert_eq!(encoded[0], 0);
        assert_eq!(&encoded[1..5], &[0, 0, 0, 0]);
    }
}
