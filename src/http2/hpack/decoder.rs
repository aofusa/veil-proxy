//! # HPACK デコーダ (RFC 7541)
//!
//! HPACK 形式でエンコードされたヘッダーをデコードします。

use super::{HpackError, HpackResult, decode_integer};
use super::table::{StaticTable, DynamicTable, HeaderField, get_indexed};
use super::huffman::huffman_decode;

/// HPACK デコーダ
pub struct HpackDecoder {
    /// 動的テーブル
    dynamic_table: DynamicTable,
    /// 最大ヘッダーリストサイズ
    max_header_list_size: usize,
    /// SETTINGS_HEADER_TABLE_SIZE で許可された最大テーブルサイズ
    max_allowed_table_size: usize,
}

impl HpackDecoder {
    /// 新しいデコーダを作成
    pub fn new(max_table_size: usize) -> Self {
        Self {
            dynamic_table: DynamicTable::new(max_table_size),
            max_header_list_size: 16384, // 16KB デフォルト
            max_allowed_table_size: max_table_size,
        }
    }

    /// 最大ヘッダーリストサイズを設定
    pub fn set_max_header_list_size(&mut self, size: usize) {
        self.max_header_list_size = size;
    }

    /// 動的テーブルの最大サイズを更新
    pub fn set_max_table_size(&mut self, size: usize) {
        self.max_allowed_table_size = size;
        self.dynamic_table.set_max_size(size);
    }

    /// 動的テーブルへの参照を取得
    pub fn dynamic_table(&self) -> &DynamicTable {
        &self.dynamic_table
    }

    /// ヘッダーブロックをデコード
    ///
    /// # Arguments
    ///
    /// * `buf` - エンコードされたヘッダーブロック
    ///
    /// # Returns
    ///
    /// デコードされたヘッダーのリスト
    pub fn decode(&mut self, buf: &[u8]) -> HpackResult<Vec<HeaderField>> {
        let mut headers = Vec::new();
        let mut pos = 0;
        let mut total_size = 0usize;
        // RFC 7541 §4.2: Dynamic table size updates MUST occur at the beginning of the header block
        let mut seen_header = false;

        while pos < buf.len() {
            let first_byte = buf[pos];

            let field = if first_byte & 0x80 != 0 {
                // Indexed Header Field (Section 6.1)
                seen_header = true;
                self.decode_indexed(&buf[pos..], &mut pos)?
            } else if first_byte & 0x40 != 0 {
                // Literal Header Field with Incremental Indexing (Section 6.2.1)
                seen_header = true;
                self.decode_literal_indexed(&buf[pos..], &mut pos)?
            } else if first_byte & 0x20 != 0 {
                // Dynamic Table Size Update (Section 6.3)
                // RFC 7541 §4.2: MUST occur at the beginning of the header block
                if seen_header {
                    return Err(HpackError::TableSizeUpdateAfterHeader);
                }
                self.decode_table_size_update(&buf[pos..], &mut pos)?;
                continue;
            } else if first_byte & 0x10 != 0 {
                // Literal Header Field Never Indexed (Section 6.2.3)
                seen_header = true;
                self.decode_literal_never_indexed(&buf[pos..], &mut pos)?
            } else {
                // Literal Header Field without Indexing (Section 6.2.2)
                seen_header = true;
                self.decode_literal_without_indexing(&buf[pos..], &mut pos)?
            };

            // ヘッダーリストサイズチェック
            total_size = total_size.saturating_add(field.size());
            if total_size > self.max_header_list_size {
                return Err(HpackError::TableSizeExceeded);
            }

            headers.push(field);
        }

        Ok(headers)
    }

    /// Indexed Header Field (Section 6.1)
    fn decode_indexed(&self, buf: &[u8], pos: &mut usize) -> HpackResult<HeaderField> {
        let (index, consumed) = decode_integer(buf, 7)?;
        *pos += consumed;

        if index == 0 {
            return Err(HpackError::InvalidIndex(0));
        }

        let (name, value) = get_indexed(&StaticTable, &self.dynamic_table, index)
            .ok_or(HpackError::InvalidIndex(index))?;

        Ok(HeaderField::new(name, value))
    }

    /// Literal Header Field with Incremental Indexing (Section 6.2.1)
    fn decode_literal_indexed(&mut self, buf: &[u8], pos: &mut usize) -> HpackResult<HeaderField> {
        // ローカルオフセットで処理
        let mut local_pos = 0usize;
        
        let (index, consumed) = decode_integer(buf, 6)?;
        local_pos += consumed;

        let name = if index > 0 {
            // 名前はインデックス参照
            let (name, _) = get_indexed(&StaticTable, &self.dynamic_table, index)
                .ok_or(HpackError::InvalidIndex(index))?;
            name.to_vec()
        } else {
            // 名前はリテラル
            let mut name_pos = 0usize;
            let name = self.decode_string(&buf[local_pos..], &mut name_pos)?;
            local_pos += name_pos;
            name
        };

        // 値をデコード
        let mut value_pos = 0usize;
        let value = self.decode_string(&buf[local_pos..], &mut value_pos)?;
        local_pos += value_pos;

        *pos += local_pos;

        // 動的テーブルに追加
        self.dynamic_table.insert(name.clone(), value.clone());

        Ok(HeaderField::new(name, value))
    }

    /// Literal Header Field without Indexing (Section 6.2.2)
    fn decode_literal_without_indexing(&self, buf: &[u8], pos: &mut usize) -> HpackResult<HeaderField> {
        // ローカルオフセットで処理
        let mut local_pos = 0usize;
        
        let (index, consumed) = decode_integer(buf, 4)?;
        local_pos += consumed;

        let name = if index > 0 {
            let (name, _) = get_indexed(&StaticTable, &self.dynamic_table, index)
                .ok_or(HpackError::InvalidIndex(index))?;
            name.to_vec()
        } else {
            let mut name_pos = 0usize;
            let name = self.decode_string(&buf[local_pos..], &mut name_pos)?;
            local_pos += name_pos;
            name
        };

        // 値をデコード
        let mut value_pos = 0usize;
        let value = self.decode_string(&buf[local_pos..], &mut value_pos)?;
        local_pos += value_pos;

        *pos += local_pos;

        Ok(HeaderField::new(name, value))
    }

    /// Literal Header Field Never Indexed (Section 6.2.3)
    fn decode_literal_never_indexed(&self, buf: &[u8], pos: &mut usize) -> HpackResult<HeaderField> {
        // Same encoding as without indexing
        self.decode_literal_without_indexing(buf, pos)
    }

    /// Dynamic Table Size Update (Section 6.3)
    fn decode_table_size_update(&mut self, buf: &[u8], pos: &mut usize) -> HpackResult<()> {
        let (size, consumed) = decode_integer(buf, 5)?;
        *pos += consumed;

        // RFC 7541 §6.3: The new maximum size MUST be lower than or equal to
        // the limit determined by the protocol using HPACK
        if size > self.max_allowed_table_size {
            return Err(HpackError::TableSizeExceeded);
        }

        self.dynamic_table.set_max_size(size);

        Ok(())
    }

    /// 文字列をデコード
    fn decode_string(&self, buf: &[u8], pos: &mut usize) -> HpackResult<Vec<u8>> {
        if buf.is_empty() {
            return Err(HpackError::BufferTooShort);
        }

        let huffman = buf[0] & 0x80 != 0;
        let (length, consumed) = decode_integer(buf, 7)?;
        
        if consumed + length > buf.len() {
            return Err(HpackError::BufferTooShort);
        }

        let string_data = &buf[consumed..consumed + length];
        *pos += consumed + length;

        if huffman {
            huffman_decode(string_data)
        } else {
            Ok(string_data.to_vec())
        }
    }
}

impl Default for HpackDecoder {
    fn default() -> Self {
        Self::new(4096)
    }
}

/// 簡易デコーダ (ステートレス)
///
/// 動的テーブルを使用しない単純なデコード。
/// 主にテスト用。
pub fn decode_headers_simple(buf: &[u8]) -> HpackResult<Vec<HeaderField>> {
    let mut decoder = HpackDecoder::new(0);
    decoder.decode(buf)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http2::hpack::encoder::HpackEncoder;

    #[test]
    fn test_decode_indexed() {
        let mut decoder = HpackDecoder::new(4096);
        
        // :method GET (index 2)
        let buf = [0x82];
        let headers = decoder.decode(&buf).unwrap();
        
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0].name, b":method");
        assert_eq!(headers[0].value, b"GET");
    }

    #[test]
    fn test_decode_multiple_indexed() {
        let mut decoder = HpackDecoder::new(4096);
        
        // :method GET (2) + :path / (4)
        let buf = [0x82, 0x84];
        let headers = decoder.decode(&buf).unwrap();
        
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].name, b":method");
        assert_eq!(headers[0].value, b"GET");
        assert_eq!(headers[1].name, b":path");
        assert_eq!(headers[1].value, b"/");
    }

    #[test]
    fn test_encode_decode_roundtrip() {
        let mut encoder = HpackEncoder::new(4096);
        encoder.set_huffman(false);
        
        let mut decoder = HpackDecoder::new(4096);

        let headers = [
            (b":method".as_slice(), b"GET".as_slice(), false),
            (b":path".as_slice(), b"/index.html".as_slice(), false),
            (b":scheme".as_slice(), b"https".as_slice(), false),
        ];

        let encoded = encoder.encode(&headers).unwrap();
        let decoded = decoder.decode(&encoded).unwrap();

        assert_eq!(decoded.len(), headers.len());
        for (i, (name, value, _)) in headers.iter().enumerate() {
            assert_eq!(decoded[i].name, *name);
            assert_eq!(decoded[i].value, *value);
        }
    }
}
