//! # HTTP/2 フレーム型定義 (RFC 7540 Section 4, 6)

/// HTTP/2 フレームタイプ (RFC 7540 Section 6)
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameType {
    /// DATA フレーム (Section 6.1)
    Data = 0x0,
    /// HEADERS フレーム (Section 6.2)
    Headers = 0x1,
    /// PRIORITY フレーム (Section 6.3)
    Priority = 0x2,
    /// RST_STREAM フレーム (Section 6.4)
    RstStream = 0x3,
    /// SETTINGS フレーム (Section 6.5)
    Settings = 0x4,
    /// PUSH_PROMISE フレーム (Section 6.6)
    PushPromise = 0x5,
    /// PING フレーム (Section 6.7)
    Ping = 0x6,
    /// GOAWAY フレーム (Section 6.8)
    GoAway = 0x7,
    /// WINDOW_UPDATE フレーム (Section 6.9)
    WindowUpdate = 0x8,
    /// CONTINUATION フレーム (Section 6.10)
    Continuation = 0x9,
}

impl FrameType {
    /// u8 から FrameType を作成
    pub fn from_u8(val: u8) -> Option<Self> {
        match val {
            0x0 => Some(Self::Data),
            0x1 => Some(Self::Headers),
            0x2 => Some(Self::Priority),
            0x3 => Some(Self::RstStream),
            0x4 => Some(Self::Settings),
            0x5 => Some(Self::PushPromise),
            0x6 => Some(Self::Ping),
            0x7 => Some(Self::GoAway),
            0x8 => Some(Self::WindowUpdate),
            0x9 => Some(Self::Continuation),
            _ => None,
        }
    }
}

/// フレームフラグ
#[allow(non_snake_case)]
pub mod FrameFlags {
    /// END_STREAM (DATA, HEADERS)
    pub const END_STREAM: u8 = 0x01;
    /// ACK (SETTINGS, PING)
    pub const ACK: u8 = 0x01;
    /// END_HEADERS (HEADERS, PUSH_PROMISE, CONTINUATION)
    pub const END_HEADERS: u8 = 0x04;
    /// PADDED (DATA, HEADERS, PUSH_PROMISE)
    pub const PADDED: u8 = 0x08;
    /// PRIORITY (HEADERS)
    pub const PRIORITY: u8 = 0x20;
}

/// HTTP/2 フレームヘッダー (9 bytes)
///
/// ```text
/// +-----------------------------------------------+
/// |                 Length (24)                   |
/// +---------------+---------------+---------------+
/// |   Type (8)    |   Flags (8)   |
/// +-+-------------+---------------+-------------------------------+
/// |R|                 Stream Identifier (31)                      |
/// +-+-------------------------------------------------------------+
/// ```
#[derive(Debug, Clone, Copy)]
pub struct FrameHeader {
    /// ペイロード長 (24ビット、最大 16,777,215)
    pub length: u32,
    /// フレームタイプ
    pub frame_type: u8,
    /// フラグ
    pub flags: u8,
    /// ストリーム ID (31ビット、最上位ビットは予約)
    pub stream_id: u32,
}

impl FrameHeader {
    /// フレームヘッダーのサイズ
    pub const SIZE: usize = 9;

    /// 新しいフレームヘッダーを作成
    pub fn new(frame_type: FrameType, flags: u8, stream_id: u32, length: u32) -> Self {
        Self {
            length,
            frame_type: frame_type as u8,
            flags,
            stream_id: stream_id & 0x7FFFFFFF, // 最上位ビットをクリア
        }
    }

    /// バイト列からデコード
    #[inline]
    pub fn decode(buf: &[u8; 9]) -> Self {
        let length = ((buf[0] as u32) << 16) | ((buf[1] as u32) << 8) | (buf[2] as u32);
        let frame_type = buf[3];
        let flags = buf[4];
        let stream_id = ((buf[5] as u32) << 24)
            | ((buf[6] as u32) << 16)
            | ((buf[7] as u32) << 8)
            | (buf[8] as u32);
        let stream_id = stream_id & 0x7FFFFFFF; // Reserved bit をマスク

        Self {
            length,
            frame_type,
            flags,
            stream_id,
        }
    }

    /// バイト列にエンコード
    #[inline]
    pub fn encode(&self, buf: &mut [u8; 9]) {
        buf[0] = ((self.length >> 16) & 0xFF) as u8;
        buf[1] = ((self.length >> 8) & 0xFF) as u8;
        buf[2] = (self.length & 0xFF) as u8;
        buf[3] = self.frame_type;
        buf[4] = self.flags;
        buf[5] = ((self.stream_id >> 24) & 0x7F) as u8;
        buf[6] = ((self.stream_id >> 16) & 0xFF) as u8;
        buf[7] = ((self.stream_id >> 8) & 0xFF) as u8;
        buf[8] = (self.stream_id & 0xFF) as u8;
    }

    /// フレームタイプを取得
    pub fn get_frame_type(&self) -> Option<FrameType> {
        FrameType::from_u8(self.frame_type)
    }

    /// END_STREAM フラグが設定されているか
    #[inline]
    pub fn is_end_stream(&self) -> bool {
        self.flags & FrameFlags::END_STREAM != 0
    }

    /// END_HEADERS フラグが設定されているか
    #[inline]
    pub fn is_end_headers(&self) -> bool {
        self.flags & FrameFlags::END_HEADERS != 0
    }

    /// ACK フラグが設定されているか
    #[inline]
    pub fn is_ack(&self) -> bool {
        self.flags & FrameFlags::ACK != 0
    }

    /// PADDED フラグが設定されているか
    #[inline]
    pub fn is_padded(&self) -> bool {
        self.flags & FrameFlags::PADDED != 0
    }

    /// PRIORITY フラグが設定されているか
    #[inline]
    pub fn is_priority(&self) -> bool {
        self.flags & FrameFlags::PRIORITY != 0
    }
}

/// HTTP/2 フレーム (ヘッダー + ペイロード)
#[derive(Debug, Clone)]
pub enum Frame {
    /// DATA フレーム
    Data {
        stream_id: u32,
        end_stream: bool,
        data: Vec<u8>,
    },
    /// HEADERS フレーム
    Headers {
        stream_id: u32,
        end_stream: bool,
        end_headers: bool,
        priority: Option<PrioritySpec>,
        header_block: Vec<u8>,
    },
    /// PRIORITY フレーム
    Priority {
        stream_id: u32,
        priority: PrioritySpec,
    },
    /// RST_STREAM フレーム
    RstStream { stream_id: u32, error_code: u32 },
    /// SETTINGS フレーム
    Settings {
        ack: bool,
        settings: Vec<(u16, u32)>,
    },
    /// PUSH_PROMISE フレーム
    PushPromise {
        stream_id: u32,
        promised_stream_id: u32,
        end_headers: bool,
        header_block: Vec<u8>,
    },
    /// PING フレーム
    Ping { ack: bool, data: [u8; 8] },
    /// GOAWAY フレーム
    GoAway {
        last_stream_id: u32,
        error_code: u32,
        debug_data: Vec<u8>,
    },
    /// WINDOW_UPDATE フレーム
    WindowUpdate { stream_id: u32, increment: u32 },
    /// CONTINUATION フレーム
    Continuation {
        stream_id: u32,
        end_headers: bool,
        header_block: Vec<u8>,
    },
    /// 未知のフレームタイプ
    Unknown {
        frame_type: u8,
        flags: u8,
        stream_id: u32,
        payload: Vec<u8>,
    },
}

/// ストリーム優先度 (Section 5.3)
#[derive(Debug, Clone, Copy)]
pub struct PrioritySpec {
    /// 依存ストリーム ID
    pub dependency: u32,
    /// 排他フラグ
    pub exclusive: bool,
    /// 重み (1-256)
    pub weight: u8,
}

impl Default for PrioritySpec {
    fn default() -> Self {
        Self {
            dependency: 0,
            exclusive: false,
            weight: 16,
        }
    }
}

impl Frame {
    /// ストリーム ID を取得
    pub fn stream_id(&self) -> u32 {
        match self {
            Frame::Data { stream_id, .. } => *stream_id,
            Frame::Headers { stream_id, .. } => *stream_id,
            Frame::Priority { stream_id, .. } => *stream_id,
            Frame::RstStream { stream_id, .. } => *stream_id,
            Frame::Settings { .. } => 0,
            Frame::PushPromise { stream_id, .. } => *stream_id,
            Frame::Ping { .. } => 0,
            Frame::GoAway { .. } => 0,
            Frame::WindowUpdate { stream_id, .. } => *stream_id,
            Frame::Continuation { stream_id, .. } => *stream_id,
            Frame::Unknown { stream_id, .. } => *stream_id,
        }
    }

    /// フレームタイプを取得
    pub fn frame_type(&self) -> FrameType {
        match self {
            Frame::Data { .. } => FrameType::Data,
            Frame::Headers { .. } => FrameType::Headers,
            Frame::Priority { .. } => FrameType::Priority,
            Frame::RstStream { .. } => FrameType::RstStream,
            Frame::Settings { .. } => FrameType::Settings,
            Frame::PushPromise { .. } => FrameType::PushPromise,
            Frame::Ping { .. } => FrameType::Ping,
            Frame::GoAway { .. } => FrameType::GoAway,
            Frame::WindowUpdate { .. } => FrameType::WindowUpdate,
            Frame::Continuation { .. } => FrameType::Continuation,
            Frame::Unknown { .. } => FrameType::Data, // フォールバック
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_header_encode_decode() {
        let original = FrameHeader::new(FrameType::Headers, FrameFlags::END_HEADERS, 1, 100);

        let mut buf = [0u8; 9];
        original.encode(&mut buf);

        let decoded = FrameHeader::decode(&buf);

        assert_eq!(decoded.length, 100);
        assert_eq!(decoded.frame_type, FrameType::Headers as u8);
        assert_eq!(decoded.flags, FrameFlags::END_HEADERS);
        assert_eq!(decoded.stream_id, 1);
    }

    #[test]
    fn test_frame_header_flags() {
        let header = FrameHeader::new(
            FrameType::Headers,
            FrameFlags::END_STREAM | FrameFlags::END_HEADERS,
            1,
            0,
        );

        assert!(header.is_end_stream());
        assert!(header.is_end_headers());
        assert!(!header.is_padded());
        assert!(!header.is_priority());
    }

    #[test]
    fn test_frame_header_stream_id_mask() {
        // 最上位ビットは予約されているのでマスクされる
        let header = FrameHeader::new(FrameType::Data, 0, 0xFFFFFFFF, 0);
        assert_eq!(header.stream_id, 0x7FFFFFFF);
    }
}
