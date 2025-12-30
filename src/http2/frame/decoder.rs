//! # HTTP/2 フレームデコーダ

use super::types::{Frame, FrameHeader, FrameType, PrioritySpec};
use crate::http2::error::{Http2Error, Http2Result};

/// フレームデコーダ
pub struct FrameDecoder {
    /// 最大フレームサイズ
    max_frame_size: u32,
}

impl FrameDecoder {
    /// 新しいデコーダを作成
    pub fn new(max_frame_size: u32) -> Self {
        Self { max_frame_size }
    }

    /// 最大フレームサイズを設定
    pub fn set_max_frame_size(&mut self, size: u32) {
        self.max_frame_size = size;
    }

    /// 最大フレームサイズを取得
    pub fn max_frame_size(&self) -> u32 {
        self.max_frame_size
    }

    /// フレームヘッダーをデコード
    pub fn decode_header(&self, buf: &[u8]) -> Http2Result<FrameHeader> {
        if buf.len() < FrameHeader::SIZE {
            return Err(Http2Error::InvalidFrame("Buffer too short for frame header".into()));
        }

        let header_bytes: [u8; 9] = buf[..9].try_into().unwrap();
        let header = FrameHeader::decode(&header_bytes);

        // フレームサイズチェック
        if header.length > self.max_frame_size {
            return Err(Http2Error::FrameTooLarge(header.length as usize, self.max_frame_size as usize));
        }

        Ok(header)
    }

    /// フレームをデコード
    pub fn decode(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if payload.len() != header.length as usize {
            return Err(Http2Error::InvalidFrame("Payload length mismatch".into()));
        }

        match FrameType::from_u8(header.frame_type) {
            Some(FrameType::Data) => self.decode_data(header, payload),
            Some(FrameType::Headers) => self.decode_headers(header, payload),
            Some(FrameType::Priority) => self.decode_priority(header, payload),
            Some(FrameType::RstStream) => self.decode_rst_stream(header, payload),
            Some(FrameType::Settings) => self.decode_settings(header, payload),
            Some(FrameType::PushPromise) => self.decode_push_promise(header, payload),
            Some(FrameType::Ping) => self.decode_ping(header, payload),
            Some(FrameType::GoAway) => self.decode_goaway(header, payload),
            Some(FrameType::WindowUpdate) => self.decode_window_update(header, payload),
            Some(FrameType::Continuation) => self.decode_continuation(header, payload),
            None => Ok(Frame::Unknown {
                frame_type: header.frame_type,
                flags: header.flags,
                stream_id: header.stream_id,
                payload: payload.to_vec(),
            }),
        }
    }

    /// DATA フレームをデコード
    fn decode_data(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id == 0 {
            return Err(Http2Error::protocol_error("DATA frame with stream ID 0"));
        }

        let (data, _pad_length) = self.extract_padding(header, payload)?;

        Ok(Frame::Data {
            stream_id: header.stream_id,
            end_stream: header.is_end_stream(),
            data: data.to_vec(),
        })
    }

    /// HEADERS フレームをデコード
    fn decode_headers(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id == 0 {
            return Err(Http2Error::protocol_error("HEADERS frame with stream ID 0"));
        }

        let (data, _pad_length) = self.extract_padding(header, payload)?;
        
        let (priority, header_block) = if header.is_priority() {
            if data.len() < 5 {
                return Err(Http2Error::frame_size_error("HEADERS priority too short"));
            }
            let priority = self.decode_priority_spec(&data[..5]);
            (Some(priority), &data[5..])
        } else {
            (None, data)
        };

        Ok(Frame::Headers {
            stream_id: header.stream_id,
            end_stream: header.is_end_stream(),
            end_headers: header.is_end_headers(),
            priority,
            header_block: header_block.to_vec(),
        })
    }

    /// PRIORITY フレームをデコード
    fn decode_priority(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id == 0 {
            return Err(Http2Error::protocol_error("PRIORITY frame with stream ID 0"));
        }

        if payload.len() != 5 {
            return Err(Http2Error::frame_size_error("PRIORITY frame must be 5 bytes"));
        }

        let priority = self.decode_priority_spec(payload);

        Ok(Frame::Priority {
            stream_id: header.stream_id,
            priority,
        })
    }

    /// RST_STREAM フレームをデコード
    fn decode_rst_stream(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id == 0 {
            return Err(Http2Error::protocol_error("RST_STREAM frame with stream ID 0"));
        }

        if payload.len() != 4 {
            return Err(Http2Error::frame_size_error("RST_STREAM frame must be 4 bytes"));
        }

        let error_code = u32::from_be_bytes(payload.try_into().unwrap());

        Ok(Frame::RstStream {
            stream_id: header.stream_id,
            error_code,
        })
    }

    /// SETTINGS フレームをデコード
    fn decode_settings(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id != 0 {
            return Err(Http2Error::protocol_error("SETTINGS frame with non-zero stream ID"));
        }

        if header.is_ack() {
            if !payload.is_empty() {
                return Err(Http2Error::frame_size_error("SETTINGS ACK must be empty"));
            }
            return Ok(Frame::Settings {
                ack: true,
                settings: Vec::new(),
            });
        }

        if payload.len() % 6 != 0 {
            return Err(Http2Error::frame_size_error("SETTINGS payload must be multiple of 6"));
        }

        let mut settings = Vec::with_capacity(payload.len() / 6);
        for chunk in payload.chunks(6) {
            let id = u16::from_be_bytes([chunk[0], chunk[1]]);
            let value = u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]);
            settings.push((id, value));
        }

        Ok(Frame::Settings {
            ack: false,
            settings,
        })
    }

    /// PUSH_PROMISE フレームをデコード
    fn decode_push_promise(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id == 0 {
            return Err(Http2Error::protocol_error("PUSH_PROMISE frame with stream ID 0"));
        }

        let (data, _pad_length) = self.extract_padding(header, payload)?;

        if data.len() < 4 {
            return Err(Http2Error::frame_size_error("PUSH_PROMISE too short"));
        }

        let promised_stream_id = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) & 0x7FFFFFFF;

        Ok(Frame::PushPromise {
            stream_id: header.stream_id,
            promised_stream_id,
            end_headers: header.is_end_headers(),
            header_block: data[4..].to_vec(),
        })
    }

    /// PING フレームをデコード
    fn decode_ping(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id != 0 {
            return Err(Http2Error::protocol_error("PING frame with non-zero stream ID"));
        }

        if payload.len() != 8 {
            return Err(Http2Error::frame_size_error("PING frame must be 8 bytes"));
        }

        let mut data = [0u8; 8];
        data.copy_from_slice(payload);

        Ok(Frame::Ping {
            ack: header.is_ack(),
            data,
        })
    }

    /// GOAWAY フレームをデコード
    fn decode_goaway(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id != 0 {
            return Err(Http2Error::protocol_error("GOAWAY frame with non-zero stream ID"));
        }

        if payload.len() < 8 {
            return Err(Http2Error::frame_size_error("GOAWAY frame too short"));
        }

        let last_stream_id = u32::from_be_bytes([payload[0], payload[1], payload[2], payload[3]]) & 0x7FFFFFFF;
        let error_code = u32::from_be_bytes([payload[4], payload[5], payload[6], payload[7]]);
        let debug_data = payload[8..].to_vec();

        Ok(Frame::GoAway {
            last_stream_id,
            error_code,
            debug_data,
        })
    }

    /// WINDOW_UPDATE フレームをデコード
    fn decode_window_update(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if payload.len() != 4 {
            return Err(Http2Error::frame_size_error("WINDOW_UPDATE frame must be 4 bytes"));
        }

        let increment = u32::from_be_bytes(payload.try_into().unwrap()) & 0x7FFFFFFF;

        if increment == 0 {
            return Err(Http2Error::protocol_error("WINDOW_UPDATE increment must be non-zero"));
        }

        Ok(Frame::WindowUpdate {
            stream_id: header.stream_id,
            increment,
        })
    }

    /// CONTINUATION フレームをデコード
    fn decode_continuation(&self, header: &FrameHeader, payload: &[u8]) -> Http2Result<Frame> {
        if header.stream_id == 0 {
            return Err(Http2Error::protocol_error("CONTINUATION frame with stream ID 0"));
        }

        Ok(Frame::Continuation {
            stream_id: header.stream_id,
            end_headers: header.is_end_headers(),
            header_block: payload.to_vec(),
        })
    }

    /// パディングを抽出
    fn extract_padding<'a>(&self, header: &FrameHeader, payload: &'a [u8]) -> Http2Result<(&'a [u8], usize)> {
        if !header.is_padded() {
            return Ok((payload, 0));
        }

        if payload.is_empty() {
            return Err(Http2Error::protocol_error("PADDED frame has no pad length"));
        }

        let pad_length = payload[0] as usize;
        if pad_length >= payload.len() {
            return Err(Http2Error::protocol_error("Pad length exceeds payload"));
        }

        let data_end = payload.len() - pad_length;
        Ok((&payload[1..data_end], pad_length))
    }

    /// Priority spec をデコード
    fn decode_priority_spec(&self, data: &[u8]) -> PrioritySpec {
        let first_word = u32::from_be_bytes([data[0], data[1], data[2], data[3]]);
        let exclusive = first_word & 0x80000000 != 0;
        let dependency = first_word & 0x7FFFFFFF;
        let weight = data[4].saturating_add(1); // 1-256 に変換

        PrioritySpec {
            dependency,
            exclusive,
            weight,
        }
    }
}

impl Default for FrameDecoder {
    fn default() -> Self {
        Self::new(16384)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http2::frame::encoder::FrameEncoder;

    #[test]
    fn test_decode_data() {
        let encoder = FrameEncoder::new(16384);
        let decoder = FrameDecoder::new(16384);

        let data = b"Hello, World!";
        let frame_bytes = encoder.encode_data(1, data, true);

        let header = decoder.decode_header(&frame_bytes).unwrap();
        let frame = decoder.decode(&header, &frame_bytes[9..]).unwrap();

        match frame {
            Frame::Data { stream_id, end_stream, data: decoded_data } => {
                assert_eq!(stream_id, 1);
                assert!(end_stream);
                assert_eq!(decoded_data, data);
            }
            _ => panic!("Expected DATA frame"),
        }
    }

    #[test]
    fn test_decode_settings() {
        let encoder = FrameEncoder::new(16384);
        let decoder = FrameDecoder::new(16384);

        let settings = vec![(0x01, 4096), (0x03, 100)];
        let frame_bytes = encoder.encode_settings(&settings, false);

        let header = decoder.decode_header(&frame_bytes).unwrap();
        let frame = decoder.decode(&header, &frame_bytes[9..]).unwrap();

        match frame {
            Frame::Settings { ack, settings: decoded_settings } => {
                assert!(!ack);
                assert_eq!(decoded_settings.len(), 2);
                assert_eq!(decoded_settings[0], (0x01, 4096));
                assert_eq!(decoded_settings[1], (0x03, 100));
            }
            _ => panic!("Expected SETTINGS frame"),
        }
    }

    #[test]
    fn test_decode_window_update() {
        let encoder = FrameEncoder::new(16384);
        let decoder = FrameDecoder::new(16384);

        let frame_bytes = encoder.encode_window_update(1, 65535);

        let header = decoder.decode_header(&frame_bytes).unwrap();
        let frame = decoder.decode(&header, &frame_bytes[9..]).unwrap();

        match frame {
            Frame::WindowUpdate { stream_id, increment } => {
                assert_eq!(stream_id, 1);
                assert_eq!(increment, 65535);
            }
            _ => panic!("Expected WINDOW_UPDATE frame"),
        }
    }

    #[test]
    fn test_decode_ping() {
        let encoder = FrameEncoder::new(16384);
        let decoder = FrameDecoder::new(16384);

        let ping_data = [1, 2, 3, 4, 5, 6, 7, 8];
        let frame_bytes = encoder.encode_ping(&ping_data, false);

        let header = decoder.decode_header(&frame_bytes).unwrap();
        let frame = decoder.decode(&header, &frame_bytes[9..]).unwrap();

        match frame {
            Frame::Ping { ack, data } => {
                assert!(!ack);
                assert_eq!(data, ping_data);
            }
            _ => panic!("Expected PING frame"),
        }
    }
}
