//! # HTTP/2 設定 (SETTINGS)
//!
//! RFC 7540 Section 6.5 で定義された SETTINGS パラメータを管理します。

/// SETTINGS パラメータ ID (RFC 7540 Section 6.5.2)
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SettingsId {
    /// ヘッダー圧縮テーブルサイズ
    HeaderTableSize = 0x1,
    /// サーバープッシュ有効化
    EnablePush = 0x2,
    /// 最大同時ストリーム数
    MaxConcurrentStreams = 0x3,
    /// 初期ウィンドウサイズ
    InitialWindowSize = 0x4,
    /// 最大フレームサイズ
    MaxFrameSize = 0x5,
    /// 最大ヘッダーリストサイズ
    MaxHeaderListSize = 0x6,
}

impl SettingsId {
    /// u16 から SettingsId を作成
    pub fn from_u16(val: u16) -> Option<Self> {
        match val {
            0x1 => Some(Self::HeaderTableSize),
            0x2 => Some(Self::EnablePush),
            0x3 => Some(Self::MaxConcurrentStreams),
            0x4 => Some(Self::InitialWindowSize),
            0x5 => Some(Self::MaxFrameSize),
            0x6 => Some(Self::MaxHeaderListSize),
            _ => None, // 未知の ID は無視
        }
    }
}

/// デフォルト値 (RFC 7540 Section 6.5.2)
pub mod defaults {
    /// ヘッダーテーブルサイズ: 4096 bytes
    pub const HEADER_TABLE_SIZE: u32 = 4096;
    /// サーバープッシュ: 有効
    pub const ENABLE_PUSH: bool = true;
    /// 最大同時ストリーム数: 無制限 (実装では 100 を使用)
    pub const MAX_CONCURRENT_STREAMS: u32 = 100;
    /// 初期ウィンドウサイズ: 65535 bytes
    pub const INITIAL_WINDOW_SIZE: u32 = 65535;
    /// 最大フレームサイズ: 16384 bytes (最小値、RFC 7540 要件)
    pub const MAX_FRAME_SIZE: u32 = 16384;
    /// 最大フレームサイズ上限: 16777215 bytes (2^24 - 1)
    pub const MAX_FRAME_SIZE_UPPER_LIMIT: u32 = 16777215;
    /// 最大ヘッダーリストサイズ: 無制限 (実装では 16KB を使用)
    pub const MAX_HEADER_LIST_SIZE: u32 = 16384;
    /// コネクションプリフェース
    pub const CONNECTION_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
    /// コネクションレベルの初期ウィンドウサイズ
    pub const CONNECTION_WINDOW_SIZE: u32 = 65535;
    /// SETTINGS タイムアウト (秒)
    pub const SETTINGS_TIMEOUT_SECS: u64 = 10;
}

/// HTTP/2 コネクション設定
#[derive(Debug, Clone)]
pub struct Http2Settings {
    /// ヘッダー圧縮テーブルサイズ (bytes)
    pub header_table_size: u32,
    /// サーバープッシュ有効化
    pub enable_push: bool,
    /// 最大同時ストリーム数
    pub max_concurrent_streams: u32,
    /// 初期ウィンドウサイズ (bytes)
    pub initial_window_size: u32,
    /// 最大フレームサイズ (bytes)
    pub max_frame_size: u32,
    /// 最大ヘッダーリストサイズ (bytes)
    pub max_header_list_size: u32,
    /// コネクションレベルのウィンドウサイズ (bytes)
    pub connection_window_size: u32,
}

impl Default for Http2Settings {
    fn default() -> Self {
        Self {
            header_table_size: defaults::HEADER_TABLE_SIZE,
            enable_push: false, // リバースプロキシではサーバープッシュは通常無効
            max_concurrent_streams: defaults::MAX_CONCURRENT_STREAMS,
            initial_window_size: defaults::INITIAL_WINDOW_SIZE,
            max_frame_size: defaults::MAX_FRAME_SIZE,
            max_header_list_size: defaults::MAX_HEADER_LIST_SIZE,
            connection_window_size: defaults::CONNECTION_WINDOW_SIZE,
        }
    }
}

impl Http2Settings {
    /// 新しい設定を作成
    pub fn new() -> Self {
        Self::default()
    }

    /// 高パフォーマンス設定
    pub fn high_performance() -> Self {
        Self {
            header_table_size: 65536,           // 64KB (より多くのヘッダーをキャッシュ)
            enable_push: false,
            max_concurrent_streams: 256,        // より多くの同時ストリーム
            initial_window_size: 1048576,       // 1MB (より大きなウィンドウ)
            max_frame_size: 65536,              // 64KB (より大きなフレーム)
            max_header_list_size: 65536,        // 64KB
            connection_window_size: 16777216,   // 16MB (より大きなコネクションウィンドウ)
        }
    }

    /// SETTINGS フレームのペイロードにエンコード
    pub fn encode(&self) -> Vec<u8> {
        let mut buf = Vec::with_capacity(36); // 6 設定 × 6 bytes

        // HEADER_TABLE_SIZE
        buf.extend_from_slice(&(SettingsId::HeaderTableSize as u16).to_be_bytes());
        buf.extend_from_slice(&self.header_table_size.to_be_bytes());

        // ENABLE_PUSH
        buf.extend_from_slice(&(SettingsId::EnablePush as u16).to_be_bytes());
        buf.extend_from_slice(&(self.enable_push as u32).to_be_bytes());

        // MAX_CONCURRENT_STREAMS
        buf.extend_from_slice(&(SettingsId::MaxConcurrentStreams as u16).to_be_bytes());
        buf.extend_from_slice(&self.max_concurrent_streams.to_be_bytes());

        // INITIAL_WINDOW_SIZE
        buf.extend_from_slice(&(SettingsId::InitialWindowSize as u16).to_be_bytes());
        buf.extend_from_slice(&self.initial_window_size.to_be_bytes());

        // MAX_FRAME_SIZE
        buf.extend_from_slice(&(SettingsId::MaxFrameSize as u16).to_be_bytes());
        buf.extend_from_slice(&self.max_frame_size.to_be_bytes());

        // MAX_HEADER_LIST_SIZE
        buf.extend_from_slice(&(SettingsId::MaxHeaderListSize as u16).to_be_bytes());
        buf.extend_from_slice(&self.max_header_list_size.to_be_bytes());

        buf
    }

    /// SETTINGS フレームのペイロードからデコード
    pub fn decode(payload: &[u8]) -> Result<Self, String> {
        if payload.len() % 6 != 0 {
            return Err("SETTINGS payload length must be multiple of 6".into());
        }

        let mut settings = Self::default();

        for chunk in payload.chunks(6) {
            let id = u16::from_be_bytes([chunk[0], chunk[1]]);
            let value = u32::from_be_bytes([chunk[2], chunk[3], chunk[4], chunk[5]]);

            match SettingsId::from_u16(id) {
                Some(SettingsId::HeaderTableSize) => {
                    settings.header_table_size = value;
                }
                Some(SettingsId::EnablePush) => {
                    if value > 1 {
                        return Err("ENABLE_PUSH must be 0 or 1".into());
                    }
                    settings.enable_push = value == 1;
                }
                Some(SettingsId::MaxConcurrentStreams) => {
                    settings.max_concurrent_streams = value;
                }
                Some(SettingsId::InitialWindowSize) => {
                    if value > 0x7FFFFFFF {
                        return Err("INITIAL_WINDOW_SIZE too large".into());
                    }
                    settings.initial_window_size = value;
                }
                Some(SettingsId::MaxFrameSize) => {
                    if value < defaults::MAX_FRAME_SIZE || value > defaults::MAX_FRAME_SIZE_UPPER_LIMIT {
                        return Err(format!(
                            "MAX_FRAME_SIZE must be between {} and {}",
                            defaults::MAX_FRAME_SIZE,
                            defaults::MAX_FRAME_SIZE_UPPER_LIMIT
                        ));
                    }
                    settings.max_frame_size = value;
                }
                Some(SettingsId::MaxHeaderListSize) => {
                    settings.max_header_list_size = value;
                }
                None => {
                    // 未知の設定は無視 (RFC 7540 Section 6.5)
                }
            }
        }

        Ok(settings)
    }

    /// 設定値を更新
    pub fn apply(&mut self, other: &Http2Settings) {
        self.header_table_size = other.header_table_size;
        self.enable_push = other.enable_push;
        self.max_concurrent_streams = other.max_concurrent_streams;
        self.initial_window_size = other.initial_window_size;
        self.max_frame_size = other.max_frame_size;
        self.max_header_list_size = other.max_header_list_size;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_settings_encode_decode() {
        let original = Http2Settings {
            header_table_size: 8192,
            enable_push: false,
            max_concurrent_streams: 200,
            initial_window_size: 131072,
            max_frame_size: 32768,
            max_header_list_size: 32768,
            connection_window_size: 1048576,
        };

        let encoded = original.encode();
        let decoded = Http2Settings::decode(&encoded).unwrap();

        assert_eq!(decoded.header_table_size, original.header_table_size);
        assert_eq!(decoded.enable_push, original.enable_push);
        assert_eq!(decoded.max_concurrent_streams, original.max_concurrent_streams);
        assert_eq!(decoded.initial_window_size, original.initial_window_size);
        assert_eq!(decoded.max_frame_size, original.max_frame_size);
        assert_eq!(decoded.max_header_list_size, original.max_header_list_size);
    }

    #[test]
    fn test_settings_invalid_enable_push() {
        let payload = [
            0x00, 0x02, // ENABLE_PUSH
            0x00, 0x00, 0x00, 0x02, // 無効な値
        ];
        assert!(Http2Settings::decode(&payload).is_err());
    }

    #[test]
    fn test_settings_invalid_frame_size() {
        let payload = [
            0x00, 0x05, // MAX_FRAME_SIZE
            0x00, 0x00, 0x00, 0x10, // 16 (最小値 16384 未満)
        ];
        assert!(Http2Settings::decode(&payload).is_err());
    }
}
