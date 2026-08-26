//! # HPACK デコーダ (RFC 7541)
//!
//! HPACK 形式でエンコードされたヘッダーをデコードします。

use super::huffman::huffman_decode_into;
use super::table::{get_indexed, DynamicTable, HeaderField, StaticTable};
use super::{decode_integer, HpackError, HpackResult};
use bytes::{Bytes, BytesMut};

/// アリーナ (`HpackDecoder::arena`) を確保するときの既定チャンクサイズ。
///
/// HTTP/2 の 1 ヘッダーブロックには通常数個〜数十個のリテラルヘッダーが含まれる。
/// 8KiB は一般的なリクエストヘッダー総量（HPACK 展開後）を 1 回の `reserve` で
/// まかなえる大きさであり、以降のヘッダーブロックでも `BytesMut` の再アロケーションが
/// 定常状態でほぼ発生しなくなる（アロケーションの償却）。
const ARENA_CHUNK: usize = 8192;

/// HPACK デコーダ
pub struct HpackDecoder {
    /// 動的テーブル
    dynamic_table: DynamicTable,
    /// 最大ヘッダーリストサイズ
    max_header_list_size: usize,
    /// SETTINGS_HEADER_TABLE_SIZE で許可された最大テーブルサイズ
    max_allowed_table_size: usize,
    /// リテラルヘッダー名/値の書き込み先アリーナ (R1、F-165)
    ///
    /// `decode_string` は Huffman デコード結果・raw 文字列のいずれも、このアリーナへ
    /// 直接書き込んでから `split_to(len).freeze()` で当該フィールド分だけを `Bytes` として
    /// 切り出す。これにより「デコードごとに `Vec<u8>` を新規確保する」コストを、
    /// アリーナの再アロケーション（チャンク単位・償却済み）だけに削減する。
    ///
    /// # 安全性: アリーナ成長と既発行 `Bytes` の独立性
    ///
    /// `split_to(at)` は `BytesMut` を `[0, at)` と `[at, len)` に分割し、`[0, at)` 側を
    /// 独立した参照カウント付き `Bytes` として返す（`self.arena` には残りの `[at, len)` だけが
    /// 残る）。返された `Bytes` は元の確保済みメモリブロックを Arc で共有するだけで、
    /// `self.arena`（残り部分）とはもはや同一の書き込み可能領域を指さない。
    /// したがって、その後 `self.arena` に対して `reserve`/`extend_from_slice`/
    /// `huffman_decode_into` の `put_u8` 等で書き込みを続け、内部的に容量不足で
    /// 新しいメモリブロックへ再アロケーションが起きても、それは「これから書く」領域を
    /// 切り替えるだけであり、**既に `split_to` で切り出し済みの旧メモリには一切書き込まない**。
    /// 旧メモリは、それを指す `Bytes` が 1 つでも生きている限り Arc の参照カウントにより
    /// 解放されない。以上により、アリーナがヘッダーブロック処理中に何度再アロケーションを
    /// 起こしても、既に呼び出し元へ返した過去の `HeaderField` の内容が書き換わったり
    /// 解放済みメモリを指したりすることはない（テストの
    /// `test_arena_growth_does_not_corrupt_earlier_fields` を参照）。
    arena: BytesMut,
}

impl HpackDecoder {
    /// 新しいデコーダを作成
    pub fn new(max_table_size: usize) -> Self {
        Self {
            dynamic_table: DynamicTable::new(max_table_size),
            max_header_list_size: 16384, // 16KB デフォルト
            max_allowed_table_size: max_table_size,
            arena: BytesMut::new(),
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

        Ok(HeaderField { name, value })
    }

    /// Literal Header Field with Incremental Indexing (Section 6.2.1)
    fn decode_literal_indexed(&mut self, buf: &[u8], pos: &mut usize) -> HpackResult<HeaderField> {
        // ローカルオフセットで処理
        let mut local_pos = 0usize;

        let (index, consumed) = decode_integer(buf, 6)?;
        local_pos += consumed;

        let name = if index > 0 {
            // 名前はインデックス参照 (静的は Bytes::from_static、動的は参照カウント clone)
            let (name, _) = get_indexed(&StaticTable, &self.dynamic_table, index)
                .ok_or(HpackError::InvalidIndex(index))?;
            name
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

        // 動的テーブルに追加 (Bytes の参照カウント clone のみ、コピーなし)
        self.dynamic_table.insert(name.clone(), value.clone());

        Ok(HeaderField { name, value })
    }

    /// Literal Header Field without Indexing (Section 6.2.2)
    fn decode_literal_without_indexing(
        &mut self,
        buf: &[u8],
        pos: &mut usize,
    ) -> HpackResult<HeaderField> {
        // ローカルオフセットで処理
        let mut local_pos = 0usize;

        let (index, consumed) = decode_integer(buf, 4)?;
        local_pos += consumed;

        let name = if index > 0 {
            let (name, _) = get_indexed(&StaticTable, &self.dynamic_table, index)
                .ok_or(HpackError::InvalidIndex(index))?;
            name
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

        Ok(HeaderField { name, value })
    }

    /// Literal Header Field Never Indexed (Section 6.2.3)
    fn decode_literal_never_indexed(
        &mut self,
        buf: &[u8],
        pos: &mut usize,
    ) -> HpackResult<HeaderField> {
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

    /// アリーナの残り容量が `additional` 未満なら `ARENA_CHUNK` 単位で確保を追加する。
    #[inline]
    fn reserve_arena(&mut self, additional: usize) {
        let remaining = self.arena.capacity() - self.arena.len();
        if remaining < additional {
            self.arena.reserve(additional.max(ARENA_CHUNK));
        }
    }

    /// 文字列をデコードし、アリーナ由来の `Bytes` として返す (R1、F-165)。
    ///
    /// Huffman・raw のいずれも中間 `Vec<u8>` を経由せず、`self.arena` へ直接書き込んでから
    /// `split_to(len).freeze()` で当該フィールド分だけを切り出す。
    fn decode_string(&mut self, buf: &[u8], pos: &mut usize) -> HpackResult<Bytes> {
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
            // Huffman 符号の最短コード長は 5bit/シンボルなので、展開後の最大バイト数は
            // ceil(入力ビット数 / 5) で上界を見積もれる。この分だけ事前確保しておけば、
            // 通常ケースでは `huffman_decode_into` の内部 `put_u8` によるアリーナの
            // 追加再アロケーションを避けられる。
            let max_decoded = (string_data.len().saturating_mul(8)).div_ceil(5);
            self.reserve_arena(max_decoded);
            huffman_decode_into(string_data, &mut self.arena)?;
        } else {
            self.reserve_arena(string_data.len());
            self.arena.extend_from_slice(string_data);
        }

        let written = self.arena.len();
        Ok(self.arena.split_to(written).freeze())
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
        assert_eq!(headers[0].name, b":method".as_slice());
        assert_eq!(headers[0].value, b"GET".as_slice());
    }

    #[test]
    fn test_decode_multiple_indexed() {
        let mut decoder = HpackDecoder::new(4096);

        // :method GET (2) + :path / (4)
        let buf = [0x82, 0x84];
        let headers = decoder.decode(&buf).unwrap();

        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].name, b":method".as_slice());
        assert_eq!(headers[0].value, b"GET".as_slice());
        assert_eq!(headers[1].name, b":path".as_slice());
        assert_eq!(headers[1].value, b"/".as_slice());
    }

    /// R1: 静的テーブル経由のインデックスヒットが `Bytes::from_static` 由来
    /// （確保ゼロ、静的データ領域を指す）であることを検証する。
    #[test]
    fn test_decode_indexed_produces_static_bytes() {
        let mut decoder = HpackDecoder::new(4096);
        let buf = [0x82]; // :method GET (index 2)
        let headers = decoder.decode(&buf).unwrap();

        let (static_name, static_value) = StaticTable::get(2).unwrap();
        assert_eq!(headers[0].name.as_ptr(), static_name.as_ptr());
        assert_eq!(headers[0].value.as_ptr(), static_value.as_ptr());
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

    /// R1(b): Huffman デコードされたリテラルヘッダーが正しい内容を持つことを検証する
    /// (アリーナ経由でも Huffman デコード結果が壊れないことの確認)。
    #[test]
    fn test_huffman_literal_correctness_via_arena() {
        let mut encoder = HpackEncoder::new(4096);
        encoder.set_huffman(true); // Huffman を強制使用

        let mut decoder = HpackDecoder::new(4096);

        let headers = [
            (
                b"custom-header-name".as_slice(),
                b"some rather long header value that should huffman-compress well aaaaaaaaaa"
                    .as_slice(),
                false,
            ),
            (
                b"x-request-id".as_slice(),
                b"abcdef0123456789".as_slice(),
                false,
            ),
        ];

        let encoded = encoder.encode(&headers).unwrap();
        let decoded = decoder.decode(&encoded).unwrap();

        assert_eq!(decoded.len(), headers.len());
        for (i, (name, value, _)) in headers.iter().enumerate() {
            assert_eq!(decoded[i].name.as_ref(), *name);
            assert_eq!(decoded[i].value.as_ref(), *value);
        }
    }

    /// R1(d): 大きなヘッダーブロックをデコードする際、アリーナの再アロケーションが
    /// 発生しても（`ARENA_CHUNK` = 8192 を大幅に超える量のリテラルを積む）、
    /// 既に返却済みの先頭側フィールドの内容が破壊されない（Bytes の独立性）ことを検証する。
    /// これはアリーナ安全性設計における最も危険なケースであり、
    /// `HpackDecoder::arena` の doc コメントで説明した不変条件を直接テストする。
    #[test]
    fn test_arena_growth_does_not_corrupt_earlier_fields() {
        // デコーダ側は 4096 を許容する（エンコーダが動的テーブルサイズ更新を出しても
        // `TableSizeExceeded` にしない）。インデックス化はエンコーダ側の max=0 で
        // 起こらないため、全フィールドがリテラル＝アリーナ経路を通る。
        let mut decoder = HpackDecoder::new(4096);
        let mut encoder = HpackEncoder::new(0);
        encoder.set_huffman(false); // 内容を明確にするため raw 文字列を使用

        // 1 ブロックあたりのヘッダーリストサイズ上限（16KB）に収まる範囲で
        // 複数ブロックをデコードし、**返却済み `Bytes` を全て保持したまま**
        // アリーナを何度も伸ばす（`BytesMut::reserve` の再アロケーションを跨ぐ）。
        // これがアリーナ設計で最も危険なケース。
        let mut expected: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
        let mut decoded_all: Vec<HeaderField> = Vec::new();

        for block in 0..20 {
            let mut headers: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            for i in 0..50 {
                headers.push((
                    format!("x-bulk-{block}-{i}").into_bytes(),
                    format!("bulk-value-{block}-{i}-{}", "x".repeat(64)).into_bytes(),
                ));
            }
            let header_refs: Vec<(&[u8], &[u8], bool)> = headers
                .iter()
                .map(|(n, v)| (n.as_slice(), v.as_slice(), false))
                .collect();
            let encoded = encoder.encode(&header_refs).unwrap();
            let decoded = decoder.decode(&encoded).unwrap();
            assert_eq!(decoded.len(), headers.len());
            decoded_all.extend(decoded);
            expected.extend(headers);
        }

        // 先頭（アリーナ再アロケーションより前に発行された）フィールドを含め、
        // 全件の内容が後続のデコードで破壊されていないこと。
        assert_eq!(decoded_all.len(), expected.len());
        for (i, (name, value)) in expected.iter().enumerate() {
            assert_eq!(
                decoded_all[i].name.as_ref(),
                name.as_slice(),
                "field {i} name"
            );
            assert_eq!(
                decoded_all[i].value.as_ref(),
                value.as_slice(),
                "field {i} value"
            );
        }
    }
}
