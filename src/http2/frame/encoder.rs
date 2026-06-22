//! # HTTP/2 フレームエンコーダ

use super::types::{Frame, FrameFlags, FrameHeader, FrameType, PrioritySpec};

/// フレームエンコーダ
pub struct FrameEncoder {
    /// 最大フレームサイズ
    max_frame_size: u32,
}

impl FrameEncoder {
    /// 新しいエンコーダを作成
    pub fn new(max_frame_size: u32) -> Self {
        Self { max_frame_size }
    }

    /// 最大フレームサイズを設定
    pub fn set_max_frame_size(&mut self, size: u32) {
        self.max_frame_size = size;
    }

    /// DATA フレームをエンコード
    pub fn encode_data(&self, stream_id: u32, data: &[u8], end_stream: bool) -> Vec<u8> {
        let mut flags = 0u8;
        if end_stream {
            flags |= FrameFlags::END_STREAM;
        }

        let header = FrameHeader::new(FrameType::Data, flags, stream_id, data.len() as u32);

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + data.len());
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);
        buf.extend_from_slice(data);
        buf
    }

    /// HEADERS フレームをエンコード
    pub fn encode_headers(
        &self,
        stream_id: u32,
        header_block: &[u8],
        end_stream: bool,
        end_headers: bool,
        priority: Option<PrioritySpec>,
    ) -> Vec<u8> {
        let mut flags = 0u8;
        if end_stream {
            flags |= FrameFlags::END_STREAM;
        }
        if end_headers {
            flags |= FrameFlags::END_HEADERS;
        }
        if priority.is_some() {
            flags |= FrameFlags::PRIORITY;
        }

        let priority_len = if priority.is_some() { 5 } else { 0 };
        let length = priority_len + header_block.len() as u32;

        let header = FrameHeader::new(FrameType::Headers, flags, stream_id, length);

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + length as usize);
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);

        // Priority
        if let Some(p) = priority {
            let dep = if p.exclusive {
                p.dependency | 0x80000000
            } else {
                p.dependency
            };
            buf.extend_from_slice(&dep.to_be_bytes());
            buf.push(p.weight.saturating_sub(1)); // weight は 0-255 で送信
        }

        buf.extend_from_slice(header_block);
        buf
    }

    /// SETTINGS フレームをエンコード
    pub fn encode_settings(&self, settings: &[(u16, u32)], ack: bool) -> Vec<u8> {
        let flags = if ack { FrameFlags::ACK } else { 0 };
        let length = if ack { 0 } else { settings.len() * 6 };

        let header = FrameHeader::new(FrameType::Settings, flags, 0, length as u32);

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + length);
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);

        if !ack {
            for &(id, value) in settings {
                buf.extend_from_slice(&id.to_be_bytes());
                buf.extend_from_slice(&value.to_be_bytes());
            }
        }

        buf
    }

    /// SETTINGS ACK フレームをエンコード
    pub fn encode_settings_ack(&self) -> Vec<u8> {
        self.encode_settings(&[], true)
    }

    /// WINDOW_UPDATE フレームをエンコード
    pub fn encode_window_update(&self, stream_id: u32, increment: u32) -> Vec<u8> {
        let header = FrameHeader::new(FrameType::WindowUpdate, 0, stream_id, 4);

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + 4);
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);
        buf.extend_from_slice(&(increment & 0x7FFFFFFF).to_be_bytes());
        buf
    }

    /// PING フレームをエンコード
    pub fn encode_ping(&self, data: &[u8; 8], ack: bool) -> Vec<u8> {
        let flags = if ack { FrameFlags::ACK } else { 0 };
        let header = FrameHeader::new(FrameType::Ping, flags, 0, 8);

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + 8);
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);
        buf.extend_from_slice(data);
        buf
    }

    /// RST_STREAM フレームをエンコード
    pub fn encode_rst_stream(&self, stream_id: u32, error_code: u32) -> Vec<u8> {
        let header = FrameHeader::new(FrameType::RstStream, 0, stream_id, 4);

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + 4);
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);
        buf.extend_from_slice(&error_code.to_be_bytes());
        buf
    }

    /// GOAWAY フレームをエンコード
    pub fn encode_goaway(
        &self,
        last_stream_id: u32,
        error_code: u32,
        debug_data: &[u8],
    ) -> Vec<u8> {
        let length = 8 + debug_data.len() as u32;
        let header = FrameHeader::new(FrameType::GoAway, 0, 0, length);

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + length as usize);
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);
        buf.extend_from_slice(&(last_stream_id & 0x7FFFFFFF).to_be_bytes());
        buf.extend_from_slice(&error_code.to_be_bytes());
        buf.extend_from_slice(debug_data);
        buf
    }

    /// CONTINUATION フレームをエンコード
    pub fn encode_continuation(
        &self,
        stream_id: u32,
        header_block: &[u8],
        end_headers: bool,
    ) -> Vec<u8> {
        let flags = if end_headers {
            FrameFlags::END_HEADERS
        } else {
            0
        };
        let header = FrameHeader::new(
            FrameType::Continuation,
            flags,
            stream_id,
            header_block.len() as u32,
        );

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + header_block.len());
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);
        buf.extend_from_slice(header_block);
        buf
    }

    /// PRIORITY フレームをエンコード
    pub fn encode_priority(&self, stream_id: u32, priority: PrioritySpec) -> Vec<u8> {
        let header = FrameHeader::new(FrameType::Priority, 0, stream_id, 5);

        let mut buf = Vec::with_capacity(FrameHeader::SIZE + 5);
        let mut header_buf = [0u8; 9];
        header.encode(&mut header_buf);
        buf.extend_from_slice(&header_buf);

        let dep = if priority.exclusive {
            priority.dependency | 0x80000000
        } else {
            priority.dependency
        };
        buf.extend_from_slice(&dep.to_be_bytes());
        buf.push(priority.weight.saturating_sub(1));
        buf
    }

    /// Frame 型からエンコード
    pub fn encode(&self, frame: &Frame) -> Vec<u8> {
        match frame {
            Frame::Data {
                stream_id,
                end_stream,
                data,
            } => self.encode_data(*stream_id, data, *end_stream),
            Frame::Headers {
                stream_id,
                end_stream,
                end_headers,
                priority,
                header_block,
            } => self.encode_headers(
                *stream_id,
                header_block,
                *end_stream,
                *end_headers,
                *priority,
            ),
            Frame::Settings { ack, settings } => self.encode_settings(settings, *ack),
            Frame::WindowUpdate {
                stream_id,
                increment,
            } => self.encode_window_update(*stream_id, *increment),
            Frame::Ping { ack, data } => self.encode_ping(data, *ack),
            Frame::RstStream {
                stream_id,
                error_code,
            } => self.encode_rst_stream(*stream_id, *error_code),
            Frame::GoAway {
                last_stream_id,
                error_code,
                debug_data,
            } => self.encode_goaway(*last_stream_id, *error_code, debug_data),
            Frame::Continuation {
                stream_id,
                end_headers,
                header_block,
            } => self.encode_continuation(*stream_id, header_block, *end_headers),
            Frame::Priority {
                stream_id,
                priority,
            } => self.encode_priority(*stream_id, *priority),
            Frame::PushPromise { .. } => {
                // サーバープッシュは無効なので空を返す
                Vec::new()
            }
            Frame::Unknown { .. } => {
                // 未知のフレームはエンコードしない
                Vec::new()
            }
        }
    }
}

impl Default for FrameEncoder {
    fn default() -> Self {
        Self::new(16384)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_data() {
        let encoder = FrameEncoder::new(16384);
        let data = b"Hello, World!";
        let frame = encoder.encode_data(1, data, true);

        assert_eq!(frame.len(), 9 + data.len());
        // Check header
        let header = FrameHeader::decode(frame[..9].try_into().unwrap());
        assert_eq!(header.frame_type, FrameType::Data as u8);
        assert_eq!(header.stream_id, 1);
        assert_eq!(header.length, data.len() as u32);
        assert!(header.is_end_stream());
    }

    #[test]
    fn test_encode_settings() {
        let encoder = FrameEncoder::new(16384);
        let settings = vec![(0x01, 4096), (0x03, 100)];
        let frame = encoder.encode_settings(&settings, false);

        assert_eq!(frame.len(), 9 + 12); // 2 settings × 6 bytes
        let header = FrameHeader::decode(frame[..9].try_into().unwrap());
        assert_eq!(header.frame_type, FrameType::Settings as u8);
        assert!(!header.is_ack());
    }

    #[test]
    fn test_encode_settings_ack() {
        let encoder = FrameEncoder::new(16384);
        let frame = encoder.encode_settings_ack();

        assert_eq!(frame.len(), 9); // Header only
        let header = FrameHeader::decode(frame[..9].try_into().unwrap());
        assert_eq!(header.frame_type, FrameType::Settings as u8);
        assert!(header.is_ack());
        assert_eq!(header.length, 0);
    }

    #[test]
    fn test_encode_window_update() {
        let encoder = FrameEncoder::new(16384);
        let frame = encoder.encode_window_update(1, 65535);

        assert_eq!(frame.len(), 9 + 4);
        let header = FrameHeader::decode(frame[..9].try_into().unwrap());
        assert_eq!(header.frame_type, FrameType::WindowUpdate as u8);
        assert_eq!(header.stream_id, 1);
    }

    #[test]
    fn test_encode_goaway() {
        let encoder = FrameEncoder::new(16384);
        let debug = b"goodbye";
        let frame = encoder.encode_goaway(100, 0, debug);

        assert_eq!(frame.len(), 9 + 8 + debug.len());
        let header = FrameHeader::decode(frame[..9].try_into().unwrap());
        assert_eq!(header.frame_type, FrameType::GoAway as u8);
        assert_eq!(header.stream_id, 0);
    }
}
