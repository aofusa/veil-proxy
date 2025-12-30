//! # HPACK ヘッダー圧縮 (RFC 7541)
//!
//! HTTP/2 のヘッダー圧縮アルゴリズムを実装します。
//!
//! ## 主要コンポーネント
//!
//! - `StaticTable`: 61 エントリの静的テーブル (Appendix A)
//! - `DynamicTable`: FIFO 動的テーブル
//! - `Huffman`: Huffman 符号化/復号化 (Appendix B)
//! - `Encoder`: HPACK エンコーダ
//! - `Decoder`: HPACK デコーダ

pub mod table;
pub mod huffman;
pub mod encoder;
pub mod decoder;

pub use table::{StaticTable, DynamicTable, HeaderField};
pub use huffman::{huffman_encode, huffman_decode};
pub use encoder::HpackEncoder;
pub use decoder::HpackDecoder;

/// HPACK エラー
#[derive(Debug, Clone)]
pub enum HpackError {
    /// 無効なインデックス
    InvalidIndex(usize),
    /// 整数オーバーフロー
    IntegerOverflow,
    /// Huffman デコードエラー
    HuffmanDecodeError,
    /// 無効な文字列
    InvalidString,
    /// バッファ不足
    BufferTooShort,
    /// 動的テーブルサイズ超過
    TableSizeExceeded,
    /// テーブルサイズ更新がヘッダーの後に出現
    TableSizeUpdateAfterHeader,
    /// 無効なエンコーディング
    InvalidEncoding(String),
}

impl std::fmt::Display for HpackError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidIndex(i) => write!(f, "Invalid HPACK index: {}", i),
            Self::IntegerOverflow => write!(f, "HPACK integer overflow"),
            Self::HuffmanDecodeError => write!(f, "Huffman decode error"),
            Self::InvalidString => write!(f, "Invalid HPACK string"),
            Self::BufferTooShort => write!(f, "Buffer too short for HPACK"),
            Self::TableSizeExceeded => write!(f, "Dynamic table size exceeded"),
            Self::TableSizeUpdateAfterHeader => write!(f, "Dynamic table size update after header"),
            Self::InvalidEncoding(s) => write!(f, "Invalid HPACK encoding: {}", s),
        }
    }
}

impl std::error::Error for HpackError {}

/// HPACK 処理結果
pub type HpackResult<T> = Result<T, HpackError>;

/// HPACK 整数デコード (RFC 7541 Section 5.1)
///
/// N ビットプレフィックスの整数をデコードします。
///
/// # Arguments
///
/// * `buf` - 入力バッファ
/// * `prefix_bits` - プレフィックスビット数 (1-8)
///
/// # Returns
///
/// (デコードされた値, 消費バイト数)
pub fn decode_integer(buf: &[u8], prefix_bits: u8) -> HpackResult<(usize, usize)> {
    if buf.is_empty() {
        return Err(HpackError::BufferTooShort);
    }

    let mask = if prefix_bits >= 8 { 0xFFu8 } else { (1u8 << prefix_bits) - 1 };
    let first_byte = buf[0] & mask;

    if first_byte < mask {
        // 1バイトで完結
        return Ok((first_byte as usize, 1));
    }

    // マルチバイト整数
    let mut value = mask as usize;
    let mut m: u32 = 0;
    let mut i = 1;

    loop {
        if i >= buf.len() {
            return Err(HpackError::BufferTooShort);
        }

        let byte = buf[i];
        let add = ((byte & 0x7F) as usize)
            .checked_shl(m)
            .ok_or(HpackError::IntegerOverflow)?;
        value = value.checked_add(add).ok_or(HpackError::IntegerOverflow)?;
        i += 1;

        if byte & 0x80 == 0 {
            break;
        }

        m += 7;
        if m > 28 {
            return Err(HpackError::IntegerOverflow);
        }
    }

    Ok((value, i))
}

/// HPACK 整数エンコード (RFC 7541 Section 5.1)
///
/// N ビットプレフィックスで整数をエンコードします。
///
/// # Arguments
///
/// * `buf` - 出力バッファ
/// * `value` - エンコードする値
/// * `prefix_bits` - プレフィックスビット数 (1-8)
/// * `prefix` - 最初のバイトのプレフィックス値
pub fn encode_integer(buf: &mut Vec<u8>, value: usize, prefix_bits: u8, prefix: u8) {
    let mask = if prefix_bits >= 8 { 0xFFu8 } else { (1u8 << prefix_bits) - 1 };

    if value < mask as usize {
        // 1バイトで完結
        buf.push(prefix | (value as u8));
        return;
    }

    // マルチバイト整数
    buf.push(prefix | mask);
    let mut remaining = value - mask as usize;

    while remaining >= 128 {
        buf.push(0x80 | (remaining as u8 & 0x7F));
        remaining >>= 7;
    }

    buf.push(remaining as u8);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_decode_integer_single_byte() {
        // 5ビットプレフィックス、値10
        let buf = [0b00001010];
        let (value, consumed) = decode_integer(&buf, 5).unwrap();
        assert_eq!(value, 10);
        assert_eq!(consumed, 1);
    }

    #[test]
    fn test_decode_integer_multi_byte() {
        // 5ビットプレフィックス、値1337 (RFC 7541 Example)
        let buf = [0b00011111, 0b10011010, 0b00001010];
        let (value, consumed) = decode_integer(&buf, 5).unwrap();
        assert_eq!(value, 1337);
        assert_eq!(consumed, 3);
    }

    #[test]
    fn test_encode_integer_single_byte() {
        let mut buf = Vec::new();
        encode_integer(&mut buf, 10, 5, 0);
        assert_eq!(buf, vec![10]);
    }

    #[test]
    fn test_encode_integer_multi_byte() {
        let mut buf = Vec::new();
        encode_integer(&mut buf, 1337, 5, 0);
        assert_eq!(buf, vec![0b00011111, 0b10011010, 0b00001010]);
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        for value in [0, 1, 30, 31, 127, 128, 1337, 65535, 1000000] {
            for prefix_bits in 1..=8 {
                let mut buf = Vec::new();
                encode_integer(&mut buf, value, prefix_bits, 0);
                let (decoded, _) = decode_integer(&buf, prefix_bits).unwrap();
                assert_eq!(decoded, value, "Failed for value={}, prefix={}", value, prefix_bits);
            }
        }
    }
}
