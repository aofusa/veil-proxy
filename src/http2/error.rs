//! # HTTP/2 エラー定義
//!
//! RFC 7540 Section 7 で定義されたエラーコードと、
//! 実装固有のエラー型を提供します。

use std::fmt;
use std::io;

/// HTTP/2 エラーコード (RFC 7540 Section 7)
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Http2ErrorCode {
    /// 正常終了
    NoError = 0x0,
    /// プロトコルエラー
    ProtocolError = 0x1,
    /// 内部エラー
    InternalError = 0x2,
    /// フロー制御エラー
    FlowControlError = 0x3,
    /// SETTINGS タイムアウト
    SettingsTimeout = 0x4,
    /// ストリームがクローズ済み
    StreamClosed = 0x5,
    /// フレームサイズエラー
    FrameSizeError = 0x6,
    /// ストリーム拒否
    RefusedStream = 0x7,
    /// ストリームキャンセル
    Cancel = 0x8,
    /// HPACK 圧縮エラー
    CompressionError = 0x9,
    /// TCP 接続エラー
    ConnectError = 0xa,
    /// 処理能力超過
    EnhanceYourCalm = 0xb,
    /// セキュリティ不足
    InadequateSecurity = 0xc,
    /// HTTP/1.1 が必要
    Http11Required = 0xd,
}

impl Http2ErrorCode {
    /// エラーコードからインスタンスを作成
    pub fn from_u32(code: u32) -> Self {
        match code {
            0x0 => Self::NoError,
            0x1 => Self::ProtocolError,
            0x2 => Self::InternalError,
            0x3 => Self::FlowControlError,
            0x4 => Self::SettingsTimeout,
            0x5 => Self::StreamClosed,
            0x6 => Self::FrameSizeError,
            0x7 => Self::RefusedStream,
            0x8 => Self::Cancel,
            0x9 => Self::CompressionError,
            0xa => Self::ConnectError,
            0xb => Self::EnhanceYourCalm,
            0xc => Self::InadequateSecurity,
            0xd => Self::Http11Required,
            _ => Self::InternalError, // 未知のコードは内部エラーとして扱う
        }
    }
}

impl fmt::Display for Http2ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = match self {
            Self::NoError => "NO_ERROR",
            Self::ProtocolError => "PROTOCOL_ERROR",
            Self::InternalError => "INTERNAL_ERROR",
            Self::FlowControlError => "FLOW_CONTROL_ERROR",
            Self::SettingsTimeout => "SETTINGS_TIMEOUT",
            Self::StreamClosed => "STREAM_CLOSED",
            Self::FrameSizeError => "FRAME_SIZE_ERROR",
            Self::RefusedStream => "REFUSED_STREAM",
            Self::Cancel => "CANCEL",
            Self::CompressionError => "COMPRESSION_ERROR",
            Self::ConnectError => "CONNECT_ERROR",
            Self::EnhanceYourCalm => "ENHANCE_YOUR_CALM",
            Self::InadequateSecurity => "INADEQUATE_SECURITY",
            Self::Http11Required => "HTTP_1_1_REQUIRED",
        };
        write!(f, "{}", name)
    }
}

/// HTTP/2 エラー
#[derive(Debug)]
pub enum Http2Error {
    /// コネクションエラー（GOAWAY を送信して終了）
    ConnectionError(Http2ErrorCode, String),
    /// ストリームエラー（RST_STREAM を送信）
    StreamError(u32, Http2ErrorCode, String),
    /// I/O エラー
    Io(io::Error),
    /// HPACK デコードエラー
    HpackDecode(String),
    /// HPACK エンコードエラー
    HpackEncode(String),
    /// プリフェースエラー
    InvalidPreface,
    /// タイムアウト
    Timeout,
    /// 接続終了
    ConnectionClosed,
    /// フレームが大きすぎる
    FrameTooLarge(usize, usize), // (actual, max)
    /// 不正なフレーム
    InvalidFrame(String),
    /// ストリーム数超過
    TooManyStreams,
    /// ウィンドウサイズ超過
    WindowOverflow,
}

impl fmt::Display for Http2Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ConnectionError(code, msg) => {
                write!(f, "Connection error {}: {}", code, msg)
            }
            Self::StreamError(stream_id, code, msg) => {
                write!(f, "Stream {} error {}: {}", stream_id, code, msg)
            }
            Self::Io(e) => write!(f, "I/O error: {}", e),
            Self::HpackDecode(msg) => write!(f, "HPACK decode error: {}", msg),
            Self::HpackEncode(msg) => write!(f, "HPACK encode error: {}", msg),
            Self::InvalidPreface => write!(f, "Invalid connection preface"),
            Self::Timeout => write!(f, "Operation timed out"),
            Self::ConnectionClosed => write!(f, "Connection closed"),
            Self::FrameTooLarge(actual, max) => {
                write!(f, "Frame too large: {} bytes (max: {})", actual, max)
            }
            Self::InvalidFrame(msg) => write!(f, "Invalid frame: {}", msg),
            Self::TooManyStreams => write!(f, "Too many concurrent streams"),
            Self::WindowOverflow => write!(f, "Flow control window overflow"),
        }
    }
}

impl std::error::Error for Http2Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for Http2Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

impl Http2Error {
    /// コネクションエラーを作成
    pub fn connection_error<S: Into<String>>(code: Http2ErrorCode, msg: S) -> Self {
        Self::ConnectionError(code, msg.into())
    }

    /// ストリームエラーを作成
    pub fn stream_error<S: Into<String>>(stream_id: u32, code: Http2ErrorCode, msg: S) -> Self {
        Self::StreamError(stream_id, code, msg.into())
    }

    /// プロトコルエラー（コネクションレベル）を作成
    pub fn protocol_error<S: Into<String>>(msg: S) -> Self {
        Self::ConnectionError(Http2ErrorCode::ProtocolError, msg.into())
    }

    /// フレームサイズエラーを作成
    pub fn frame_size_error<S: Into<String>>(msg: S) -> Self {
        Self::ConnectionError(Http2ErrorCode::FrameSizeError, msg.into())
    }

    /// 圧縮エラーを作成
    pub fn compression_error<S: Into<String>>(msg: S) -> Self {
        Self::ConnectionError(Http2ErrorCode::CompressionError, msg.into())
    }

    /// ストリームがクローズされたエラーを作成
    pub fn stream_closed(stream_id: u32, error_code: u32) -> Self {
        let code = Http2ErrorCode::from_u32(error_code);
        Self::StreamError(stream_id, code, format!("Stream closed with error code {}", error_code))
    }

    /// GOAWAY を送信すべきかどうか
    pub fn should_goaway(&self) -> bool {
        matches!(self, Self::ConnectionError(_, _))
    }

    /// RST_STREAM を送信すべきストリームID
    pub fn rst_stream_id(&self) -> Option<u32> {
        match self {
            Self::StreamError(id, _, _) => Some(*id),
            _ => None,
        }
    }

    /// エラーコードを取得
    pub fn error_code(&self) -> Http2ErrorCode {
        match self {
            Self::ConnectionError(code, _) => *code,
            Self::StreamError(_, code, _) => *code,
            Self::Io(_) => Http2ErrorCode::InternalError,
            Self::HpackDecode(_) | Self::HpackEncode(_) => Http2ErrorCode::CompressionError,
            Self::InvalidPreface => Http2ErrorCode::ProtocolError,
            Self::Timeout => Http2ErrorCode::SettingsTimeout,
            Self::ConnectionClosed => Http2ErrorCode::NoError,
            Self::FrameTooLarge(_, _) => Http2ErrorCode::FrameSizeError,
            Self::InvalidFrame(_) => Http2ErrorCode::ProtocolError,
            Self::TooManyStreams => Http2ErrorCode::RefusedStream,
            Self::WindowOverflow => Http2ErrorCode::FlowControlError,
        }
    }
}

/// HTTP/2 処理結果
pub type Http2Result<T> = Result<T, Http2Error>;
