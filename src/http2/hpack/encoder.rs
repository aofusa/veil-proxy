//! # HPACK エンコーダ (RFC 7541)
//!
//! HTTP ヘッダーを HPACK 形式でエンコードします。

use super::huffman::{huffman_encode, huffman_encoded_len};
use super::table::{DynamicTable, StaticTable};
use super::{encode_integer, HpackResult};
use bytes::Bytes;

/// `HpackEncoder::encode` の戻り値（R3、F-165）。
///
/// 内部の `Vec<u8>` は `crate::pool::hpack_encode_buf_get` から借用しており、この値が
/// Drop されるときに `crate::pool::hpack_encode_buf_put` へ自動返却される。
/// `Deref<Target = [u8]>` / `AsRef<[u8]>` を実装しているため、既存の `&[u8]` 消費側
/// （`FrameEncoder::encode_headers`/`encode_headers_into` 等）はコード変更なしで動く
/// （`&encoded_block` が deref coercion で `&[u8]` になる）。
pub struct EncodedHeaderBlock(Vec<u8>);

impl std::ops::Deref for EncodedHeaderBlock {
    type Target = [u8];

    #[inline]
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl AsRef<[u8]> for EncodedHeaderBlock {
    #[inline]
    fn as_ref(&self) -> &[u8] {
        &self.0
    }
}

impl std::fmt::Debug for EncodedHeaderBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Debug::fmt(&self.0, f)
    }
}

impl Drop for EncodedHeaderBlock {
    #[inline]
    fn drop(&mut self) {
        // `mem::take` で空の `Vec`（確保済みキャパシティ 0）を残し、実バッファだけを
        // プールへ返す（Drop 内で self.0 を clone せず所有権だけ移す）。
        crate::pool::hpack_encode_buf_put(std::mem::take(&mut self.0));
    }
}

/// HPACK エンコーダ
pub struct HpackEncoder {
    /// 動的テーブル
    dynamic_table: DynamicTable,
    /// Huffman エンコードを使用するか
    use_huffman: bool,
    /// 動的テーブルサイズの変更保留
    pending_table_size_update: Option<usize>,
}

impl HpackEncoder {
    /// 新しいエンコーダを作成
    pub fn new(max_table_size: usize) -> Self {
        Self {
            dynamic_table: DynamicTable::new(max_table_size),
            use_huffman: true,
            pending_table_size_update: None,
        }
    }

    /// Huffman エンコードの使用を設定
    pub fn set_huffman(&mut self, enabled: bool) {
        self.use_huffman = enabled;
    }

    /// 動的テーブルの最大サイズを更新
    pub fn set_max_table_size(&mut self, size: usize) {
        self.pending_table_size_update = Some(size);
        self.dynamic_table.set_max_size(size);
    }

    /// 動的テーブルへの参照を取得
    pub fn dynamic_table(&self) -> &DynamicTable {
        &self.dynamic_table
    }

    /// ヘッダーリストをエンコード
    ///
    /// # Arguments
    ///
    /// * `headers` - エンコードするヘッダーのリスト (name, value, sensitive)
    ///   - sensitive: true の場合、インデックスを使用しない
    ///
    /// # Returns
    ///
    /// エンコードされたバイト列（プール由来バッファのゼロコピー貸し出し、R3/F-165）。
    /// 返り値を drop すると自動的にプールへ返却され、次回呼び出しで再利用される。
    pub fn encode(&mut self, headers: &[(&[u8], &[u8], bool)]) -> HpackResult<EncodedHeaderBlock> {
        // 毎回新規 `Vec::with_capacity` する代わりに、スレッドローカルの再利用バッファ
        // プール（`crate::pool::hpack_encode_buf_get`）から借用する（定常状態でゼロ確保）。
        let mut buf = crate::pool::hpack_encode_buf_get();
        buf.clear();
        if buf.capacity() < headers.len() * 32 {
            buf.reserve(headers.len() * 32 - buf.capacity());
        }

        // テーブルサイズ更新があれば先に送信
        if let Some(size) = self.pending_table_size_update.take() {
            self.encode_table_size_update(&mut buf, size);
        }

        for &(name, value, sensitive) in headers {
            if sensitive {
                // Never Indexed
                self.encode_never_indexed(&mut buf, name, value)?;
            } else {
                self.encode_header(&mut buf, name, value)?;
            }
        }

        Ok(EncodedHeaderBlock(buf))
    }

    /// 単一ヘッダーをエンコード
    fn encode_header(&mut self, buf: &mut Vec<u8>, name: &[u8], value: &[u8]) -> HpackResult<()> {
        // 1. 完全一致を検索 (静的テーブル + 動的テーブル)
        if let Some(index) = self.find_exact(name, value) {
            // Indexed Header Field
            self.encode_indexed(buf, index);
            return Ok(());
        }

        // 2. 名前のみ一致を検索
        let name_index = self.find_name(name);

        // 3. Literal Header Field with Incremental Indexing
        self.encode_literal_indexed(buf, name_index, name, value)?;

        // 動的テーブルに追加（エンコーダ側は呼び出し元借用の `&[u8]` からのコピーが
        // 避けられないため `Bytes::copy_from_slice` を使う。デコーダ側〈R1〉のような
        // アリーナ共有はできない：ここでの入力はホットパスの HeaderField 由来ではなく
        // 呼び出し元が渡す一時的な `&[u8]` であり、生存期間を跨いで共有できない）。
        self.dynamic_table
            .insert(Bytes::copy_from_slice(name), Bytes::copy_from_slice(value));

        Ok(())
    }

    /// 完全一致インデックスを検索
    fn find_exact(&self, name: &[u8], value: &[u8]) -> Option<usize> {
        // 静的テーブル
        if let Some(index) = StaticTable::find_exact(name, value) {
            return Some(index);
        }

        // 動的テーブル
        if let Some(index) = self.dynamic_table.find_exact(name, value) {
            return Some(StaticTable::SIZE + index);
        }

        None
    }

    /// 名前一致インデックスを検索
    fn find_name(&self, name: &[u8]) -> Option<usize> {
        // 静的テーブル
        if let Some(index) = StaticTable::find_name(name) {
            return Some(index);
        }

        // 動的テーブル
        if let Some(index) = self.dynamic_table.find_name(name) {
            return Some(StaticTable::SIZE + index);
        }

        None
    }

    /// Indexed Header Field (Section 6.1)
    /// ```
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 1 |        Index (7+)         |
    /// +---+---------------------------+
    /// ```
    fn encode_indexed(&self, buf: &mut Vec<u8>, index: usize) {
        encode_integer(buf, index, 7, 0x80);
    }

    /// Literal Header Field with Incremental Indexing (Section 6.2.1)
    /// ```
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 1 |      Index (6+)       |
    /// +---+---+-----------------------+
    /// | H |     Value Length (7+)     |
    /// +---+---------------------------+
    /// | Value String (Length octets)  |
    /// +-------------------------------+
    /// ```
    fn encode_literal_indexed(
        &self,
        buf: &mut Vec<u8>,
        name_index: Option<usize>,
        name: &[u8],
        value: &[u8],
    ) -> HpackResult<()> {
        if let Some(index) = name_index {
            // 名前はインデックス参照
            encode_integer(buf, index, 6, 0x40);
        } else {
            // 名前もリテラル
            buf.push(0x40);
            self.encode_string(buf, name)?;
        }

        // 値
        self.encode_string(buf, value)?;

        Ok(())
    }

    /// Literal Header Field Never Indexed (Section 6.2.3)
    /// ```
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 0 | 0 | 1 |  Index (4+)   |
    /// +---+---+-----------------------+
    /// | H |     Value Length (7+)     |
    /// +---+---------------------------+
    /// | Value String (Length octets)  |
    /// +-------------------------------+
    /// ```
    fn encode_never_indexed(
        &self,
        buf: &mut Vec<u8>,
        name: &[u8],
        value: &[u8],
    ) -> HpackResult<()> {
        let name_index = self.find_name(name);

        if let Some(index) = name_index {
            encode_integer(buf, index, 4, 0x10);
        } else {
            buf.push(0x10);
            self.encode_string(buf, name)?;
        }

        self.encode_string(buf, value)?;

        Ok(())
    }

    /// Dynamic Table Size Update (Section 6.3)
    /// ```
    ///   0   1   2   3   4   5   6   7
    /// +---+---+---+---+---+---+---+---+
    /// | 0 | 0 | 1 |   Max size (5+)   |
    /// +---+---------------------------+
    /// ```
    fn encode_table_size_update(&self, buf: &mut Vec<u8>, size: usize) {
        encode_integer(buf, size, 5, 0x20);
    }

    /// 文字列をエンコード
    /// ```
    ///   0   1   2   3   4   5   6   7
    /// +---+---------------------------+
    /// | H |     String Length (7+)    |
    /// +---+---------------------------+
    /// |  String Data (Length octets)  |
    /// +-------------------------------+
    /// ```
    fn encode_string(&self, buf: &mut Vec<u8>, s: &[u8]) -> HpackResult<()> {
        if self.use_huffman {
            let huffman_len = huffman_encoded_len(s);
            if huffman_len < s.len() {
                // Huffman エンコードが短い場合
                encode_integer(buf, huffman_len, 7, 0x80);
                buf.extend(huffman_encode(s));
                return Ok(());
            }
        }

        // Raw string
        encode_integer(buf, s.len(), 7, 0x00);
        buf.extend_from_slice(s);

        Ok(())
    }
}

impl Default for HpackEncoder {
    fn default() -> Self {
        Self::new(4096)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_indexed() {
        let encoder = HpackEncoder::new(4096);
        let mut buf = Vec::new();

        // :method GET (index 2)
        encoder.encode_indexed(&mut buf, 2);
        assert_eq!(buf, vec![0x82]);
    }

    #[test]
    fn test_encode_headers() {
        let mut encoder = HpackEncoder::new(4096);
        encoder.set_huffman(false); // テスト簡略化のため Huffman 無効

        let headers = [
            (b":method".as_slice(), b"GET".as_slice(), false),
            (b":path".as_slice(), b"/".as_slice(), false),
        ];

        let encoded = encoder.encode(&headers).unwrap();

        // :method GET と :path / は静的テーブルにあるのでインデックス参照
        assert_eq!(encoded[0] & 0x80, 0x80); // Indexed
        assert_eq!(encoded[1] & 0x80, 0x80); // Indexed
    }

    #[test]
    fn test_encode_custom_header() {
        let mut encoder = HpackEncoder::new(4096);
        encoder.set_huffman(false);

        let headers = [(
            b"custom-header".as_slice(),
            b"custom-value".as_slice(),
            false,
        )];

        let encoded = encoder.encode(&headers).unwrap();

        // Literal with Incremental Indexing
        assert_eq!(encoded[0] & 0xC0, 0x40);
    }

    #[test]
    fn test_encode_sensitive_header() {
        let mut encoder = HpackEncoder::new(4096);
        encoder.set_huffman(false);

        let headers = [(b"authorization".as_slice(), b"secret".as_slice(), true)];

        let encoded = encoder.encode(&headers).unwrap();

        // Never Indexed (静的テーブルに authorization があるのでインデックス使用)
        assert_eq!(encoded[0] & 0xF0, 0x10);
    }
}
