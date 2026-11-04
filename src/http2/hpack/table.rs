//! # HPACK テーブル (RFC 7541 Section 2)
//!
//! 静的テーブル (Appendix A) と動的テーブルを実装します。

use bytes::Bytes;
use std::collections::VecDeque;

/// HTTP ヘッダーフィールド
///
/// `name`/`value` は `Bytes`（参照カウント共有バッファ）で保持する（R1、F-165）。
/// 静的テーブル由来の値は `Bytes::from_static`（確保ゼロ）、動的テーブルからの
/// ヒットは既存 `Bytes` の参照カウント clone（メモリコピーなし）、デコーダのリテラル
/// ヘッダーはデコーダが持つアリーナ `BytesMut` から `split_to().freeze()` した
/// スライスであり、いずれも per-field のヒープ確保を発生させない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HeaderField {
    /// ヘッダー名
    pub name: Bytes,
    /// ヘッダー値
    pub value: Bytes,
}

impl HeaderField {
    /// 新しいヘッダーフィールドを作成
    pub fn new(name: impl Into<Bytes>, value: impl Into<Bytes>) -> Self {
        Self {
            name: name.into(),
            value: value.into(),
        }
    }

    /// エントリサイズ (RFC 7541 Section 4.1)
    /// サイズ = name.len() + value.len() + 32
    #[inline]
    pub fn size(&self) -> usize {
        self.name.len() + self.value.len() + 32
    }

    /// 名前が一致するか
    #[inline]
    pub fn name_eq(&self, other: &[u8]) -> bool {
        self.name.as_ref() == other
    }

    /// 名前と値が一致するか
    #[inline]
    pub fn eq_field(&self, name: &[u8], value: &[u8]) -> bool {
        self.name.as_ref() == name && self.value.as_ref() == value
    }
}

/// 静的テーブル (RFC 7541 Appendix A)
pub struct StaticTable;

impl StaticTable {
    /// 静的テーブルのエントリ数
    pub const SIZE: usize = 61;

    /// 静的テーブルエントリ (1-indexed)
    /// (名前, 値)
    const ENTRIES: [(&'static [u8], &'static [u8]); 61] = [
        (b":authority", b""),
        (b":method", b"GET"),
        (b":method", b"POST"),
        (b":path", b"/"),
        (b":path", b"/index.html"),
        (b":scheme", b"http"),
        (b":scheme", b"https"),
        (b":status", b"200"),
        (b":status", b"204"),
        (b":status", b"206"),
        (b":status", b"304"),
        (b":status", b"400"),
        (b":status", b"404"),
        (b":status", b"500"),
        (b"accept-charset", b""),
        (b"accept-encoding", b"gzip, deflate"),
        (b"accept-language", b""),
        (b"accept-ranges", b""),
        (b"accept", b""),
        (b"access-control-allow-origin", b""),
        (b"age", b""),
        (b"allow", b""),
        (b"authorization", b""),
        (b"cache-control", b""),
        (b"content-disposition", b""),
        (b"content-encoding", b""),
        (b"content-language", b""),
        (b"content-length", b""),
        (b"content-location", b""),
        (b"content-range", b""),
        (b"content-type", b""),
        (b"cookie", b""),
        (b"date", b""),
        (b"etag", b""),
        (b"expect", b""),
        (b"expires", b""),
        (b"from", b""),
        (b"host", b""),
        (b"if-match", b""),
        (b"if-modified-since", b""),
        (b"if-none-match", b""),
        (b"if-range", b""),
        (b"if-unmodified-since", b""),
        (b"last-modified", b""),
        (b"link", b""),
        (b"location", b""),
        (b"max-forwards", b""),
        (b"proxy-authenticate", b""),
        (b"proxy-authorization", b""),
        (b"range", b""),
        (b"referer", b""),
        (b"refresh", b""),
        (b"retry-after", b""),
        (b"server", b""),
        (b"set-cookie", b""),
        (b"strict-transport-security", b""),
        (b"transfer-encoding", b""),
        (b"user-agent", b""),
        (b"vary", b""),
        (b"via", b""),
        (b"www-authenticate", b""),
    ];

    /// インデックスでエントリを取得 (1-indexed)
    #[inline]
    pub fn get(index: usize) -> Option<(&'static [u8], &'static [u8])> {
        if index == 0 || index > Self::SIZE {
            return None;
        }
        let (name, value) = Self::ENTRIES[index - 1];
        Some((name, value))
    }

    /// インデックスでエントリを `Bytes::from_static`（確保ゼロ）で取得 (1-indexed)
    #[inline]
    pub fn get_bytes(index: usize) -> Option<(Bytes, Bytes)> {
        Self::get(index).map(|(n, v)| (Bytes::from_static(n), Bytes::from_static(v)))
    }

    /// 名前の最初のインデックス（1-indexed）と同じ名前のエントリ数。同じ名前のエントリは連続している。
    ///
    /// 応答ごとに全ヘッダを引くので、61 個を順に比べず名前の `match`（長さと内容の分岐）で決める。
    #[inline]
    fn name_range(name: &[u8]) -> Option<(usize, usize)> {
        match name {
            b":authority" => Some((1, 1)),
            b":method" => Some((2, 2)),
            b":path" => Some((4, 2)),
            b":scheme" => Some((6, 2)),
            b":status" => Some((8, 7)),
            b"accept-charset" => Some((15, 1)),
            b"accept-encoding" => Some((16, 1)),
            b"accept-language" => Some((17, 1)),
            b"accept-ranges" => Some((18, 1)),
            b"accept" => Some((19, 1)),
            b"access-control-allow-origin" => Some((20, 1)),
            b"age" => Some((21, 1)),
            b"allow" => Some((22, 1)),
            b"authorization" => Some((23, 1)),
            b"cache-control" => Some((24, 1)),
            b"content-disposition" => Some((25, 1)),
            b"content-encoding" => Some((26, 1)),
            b"content-language" => Some((27, 1)),
            b"content-length" => Some((28, 1)),
            b"content-location" => Some((29, 1)),
            b"content-range" => Some((30, 1)),
            b"content-type" => Some((31, 1)),
            b"cookie" => Some((32, 1)),
            b"date" => Some((33, 1)),
            b"etag" => Some((34, 1)),
            b"expect" => Some((35, 1)),
            b"expires" => Some((36, 1)),
            b"from" => Some((37, 1)),
            b"host" => Some((38, 1)),
            b"if-match" => Some((39, 1)),
            b"if-modified-since" => Some((40, 1)),
            b"if-none-match" => Some((41, 1)),
            b"if-range" => Some((42, 1)),
            b"if-unmodified-since" => Some((43, 1)),
            b"last-modified" => Some((44, 1)),
            b"link" => Some((45, 1)),
            b"location" => Some((46, 1)),
            b"max-forwards" => Some((47, 1)),
            b"proxy-authenticate" => Some((48, 1)),
            b"proxy-authorization" => Some((49, 1)),
            b"range" => Some((50, 1)),
            b"referer" => Some((51, 1)),
            b"refresh" => Some((52, 1)),
            b"retry-after" => Some((53, 1)),
            b"server" => Some((54, 1)),
            b"set-cookie" => Some((55, 1)),
            b"strict-transport-security" => Some((56, 1)),
            b"transfer-encoding" => Some((57, 1)),
            b"user-agent" => Some((58, 1)),
            b"vary" => Some((59, 1)),
            b"via" => Some((60, 1)),
            b"www-authenticate" => Some((61, 1)),
            _ => None,
        }
    }

    /// 名前と値の両方が一致するインデックスを検索
    pub fn find_exact(name: &[u8], value: &[u8]) -> Option<usize> {
        let (first, count) = Self::name_range(name)?;
        (first..first + count).find(|&i| Self::ENTRIES[i - 1].1 == value)
    }

    /// 名前が一致するインデックスを検索
    #[inline]
    pub fn find_name(name: &[u8]) -> Option<usize> {
        Self::name_range(name).map(|(first, _)| first)
    }
}

/// 動的テーブル (RFC 7541 Section 2.3.2)
pub struct DynamicTable {
    /// エントリ (FIFO: 新しいものが先頭)
    entries: VecDeque<HeaderField>,
    /// 現在のサイズ (bytes)
    size: usize,
    /// 最大サイズ (bytes)
    max_size: usize,
}

impl DynamicTable {
    /// 新しい動的テーブルを作成
    pub fn new(max_size: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            size: 0,
            max_size,
        }
    }

    /// 現在のエントリ数
    #[inline]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// テーブルが空かどうか
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// 現在のサイズ (bytes)
    #[inline]
    pub fn size(&self) -> usize {
        self.size
    }

    /// 最大サイズ (bytes)
    #[inline]
    pub fn max_size(&self) -> usize {
        self.max_size
    }

    /// 最大サイズを更新 (RFC 7541 Section 4.3)
    pub fn set_max_size(&mut self, max_size: usize) {
        self.max_size = max_size;
        self.evict();
    }

    /// エントリを追加 (RFC 7541 Section 4.4)
    ///
    /// 新しいエントリは先頭に追加され、サイズ超過時は末尾から削除されます。
    /// `name`/`value` は既に `Bytes` を持つ呼び出し側（デコーダのアリーナ由来スライス）
    /// から渡され、ここでは参照カウント clone のみ（コピーなし）。
    pub fn insert(&mut self, name: Bytes, value: Bytes) {
        let entry = HeaderField { name, value };
        let entry_size = entry.size();

        // エントリが max_size より大きい場合はテーブルをクリア
        if entry_size > self.max_size {
            self.entries.clear();
            self.size = 0;
            return;
        }

        // 容量確保のために古いエントリを削除
        while self.size + entry_size > self.max_size && !self.entries.is_empty() {
            if let Some(old) = self.entries.pop_back() {
                self.size = self.size.saturating_sub(old.size());
            }
        }

        // 新しいエントリを先頭に追加
        self.entries.push_front(entry);
        self.size += entry_size;
    }

    /// インデックスでエントリを取得 (1-indexed, 静的テーブル考慮なし)
    #[inline]
    pub fn get(&self, index: usize) -> Option<&HeaderField> {
        if index == 0 {
            return None;
        }
        self.entries.get(index - 1)
    }

    /// サイズ超過分を削除
    fn evict(&mut self) {
        while self.size > self.max_size && !self.entries.is_empty() {
            if let Some(old) = self.entries.pop_back() {
                self.size = self.size.saturating_sub(old.size());
            }
        }
    }

    /// 名前と値の両方が一致するインデックスを検索 (1-indexed)
    pub fn find_exact(&self, name: &[u8], value: &[u8]) -> Option<usize> {
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.eq_field(name, value) {
                return Some(i + 1);
            }
        }
        None
    }

    /// 名前が一致するインデックスを検索 (1-indexed)
    pub fn find_name(&self, name: &[u8]) -> Option<usize> {
        for (i, entry) in self.entries.iter().enumerate() {
            if entry.name_eq(name) {
                return Some(i + 1);
            }
        }
        None
    }

    /// テーブルをクリア
    pub fn clear(&mut self) {
        self.entries.clear();
        self.size = 0;
    }
}

/// 統合インデックスアクセス (静的 + 動的テーブル)
///
/// インデックス 1-61: 静的テーブル
/// インデックス 62+: 動的テーブル
///
/// 戻り値は `Bytes`。静的テーブル側は `Bytes::from_static`（確保ゼロ）、動的テーブル側は
/// 既存エントリの参照カウント clone（メモリコピーなし）。
pub fn get_indexed(
    static_table: &StaticTable,
    dynamic_table: &DynamicTable,
    index: usize,
) -> Option<(Bytes, Bytes)> {
    let _ = static_table; // StaticTable は関数で使用

    if index == 0 {
        return None;
    }

    if index <= StaticTable::SIZE {
        StaticTable::get_bytes(index)
    } else {
        let dynamic_index = index - StaticTable::SIZE;
        dynamic_table
            .get(dynamic_index)
            .map(|f| (f.name.clone(), f.value.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_static_table_get() {
        // Index 1: :authority
        let (name, value) = StaticTable::get(1).unwrap();
        assert_eq!(name, b":authority");
        assert_eq!(value, b"");

        // Index 2: :method GET
        let (name, value) = StaticTable::get(2).unwrap();
        assert_eq!(name, b":method");
        assert_eq!(value, b"GET");

        // Index 8: :status 200
        let (name, value) = StaticTable::get(8).unwrap();
        assert_eq!(name, b":status");
        assert_eq!(value, b"200");

        // Index 61: www-authenticate
        let (name, value) = StaticTable::get(61).unwrap();
        assert_eq!(name, b"www-authenticate");
        assert_eq!(value, b"");

        // Invalid indices
        assert!(StaticTable::get(0).is_none());
        assert!(StaticTable::get(62).is_none());
    }

    #[test]
    fn test_static_table_find() {
        assert_eq!(StaticTable::find_exact(b":method", b"GET"), Some(2));
        assert_eq!(StaticTable::find_exact(b":method", b"POST"), Some(3));
        assert_eq!(StaticTable::find_exact(b":method", b"PUT"), None);
        assert_eq!(StaticTable::find_name(b":method"), Some(2));
    }

    /// 名前の `match` が全エントリを順に比べたのと同じ結果になること（表と `match` のずれを防ぐ）
    #[test]
    fn test_static_table_find_matches_linear_scan() {
        let linear_exact = |n: &[u8], v: &[u8]| {
            (1..=StaticTable::SIZE).find(|&i| StaticTable::get(i) == Some((n, v)))
        };
        let linear_name =
            |n: &[u8]| (1..=StaticTable::SIZE).find(|&i| StaticTable::get(i).unwrap().0 == n);
        for i in 1..=StaticTable::SIZE {
            let (n, v) = StaticTable::get(i).unwrap();
            assert_eq!(StaticTable::find_exact(n, v), Some(i), "{i}");
            assert_eq!(StaticTable::find_name(n), linear_name(n), "{i}");
            assert_eq!(
                StaticTable::find_exact(n, b"x"),
                linear_exact(n, b"x"),
                "{i}"
            );
        }
        for n in [&b"x-custom"[..], b"", b"Etag", b":status2"] {
            assert_eq!(StaticTable::find_name(n), None);
            assert_eq!(StaticTable::find_exact(n, b""), None);
        }
    }

    /// R1: 静的テーブルのエントリが `Bytes::from_static` 由来（確保ゼロ）であることを検証。
    /// `Bytes::from_static` は常に `is_inline() == false` かつ `ptr` がプログラムの
    /// 静的データ領域を指す。ここでは間接的に「静的テーブルの `HeaderField` を大量に
    /// clone してもヒープ確保が発生しない」ことを確認する（clone は Arc カウントの
    /// インクリメントのみで完了するはずで、パニックや解放不整合が起きないことを見る）。
    #[test]
    fn test_static_table_bytes_from_static() {
        let (name, value) = StaticTable::get_bytes(2).unwrap();
        assert_eq!(name.as_ref(), b":method");
        assert_eq!(value.as_ref(), b"GET");

        // from_static のポインタは元の &'static [u8] と一致する（コピーされていない証拠）。
        let (static_name, static_value) = StaticTable::get(2).unwrap();
        assert_eq!(name.as_ptr(), static_name.as_ptr());
        assert_eq!(value.as_ptr(), static_value.as_ptr());

        // 大量 clone してもデータは常に同一
        for _ in 0..1000 {
            let cloned = name.clone();
            assert_eq!(cloned.as_ptr(), static_name.as_ptr());
        }
    }

    #[test]
    fn test_dynamic_table_insert() {
        let mut table = DynamicTable::new(4096);

        // Insert entry
        table.insert(
            Bytes::from_static(b"custom-header"),
            Bytes::from_static(b"custom-value"),
        );
        assert_eq!(table.len(), 1);

        let entry = table.get(1).unwrap();
        assert_eq!(entry.name, b"custom-header".as_slice());
        assert_eq!(entry.value, b"custom-value".as_slice());
    }

    #[test]
    fn test_dynamic_table_eviction() {
        // Small max_size: 名前10 + 値10 + 32 = 52 bytes per entry
        let mut table = DynamicTable::new(100);

        table.insert(
            Bytes::from_static(b"header1234"),
            Bytes::from_static(b"value12345"),
        ); // 52 bytes
        assert_eq!(table.len(), 1);

        table.insert(
            Bytes::from_static(b"header5678"),
            Bytes::from_static(b"value67890"),
        ); // 52 bytes, exceeds 100
           // 最初のエントリが削除される
        assert_eq!(table.len(), 1);

        let entry = table.get(1).unwrap();
        assert_eq!(entry.name, b"header5678".as_slice());
    }

    #[test]
    fn test_dynamic_table_clear_on_oversize() {
        let mut table = DynamicTable::new(50);

        // 50 bytes を超えるエントリ
        table.insert(
            Bytes::from_static(b"very-long-header-name"),
            Bytes::from_static(b"very-long-value"),
        );
        assert!(table.is_empty());
    }

    /// R1: 型変更後もエビクション時のサイズ会計（RFC 7541 §4.1）が正しいことを検証。
    /// 複数エントリを挿入・退避させ、`size()` が実際に残っているエントリの合計と一致するかを確認する。
    #[test]
    fn test_dynamic_table_eviction_accounting_after_bytes_migration() {
        // 各エントリ 5 + 5 + 32 = 42 bytes
        let mut table = DynamicTable::new(100); // 最大2エントリ分強

        table.insert(Bytes::from_static(b"aaaaa"), Bytes::from_static(b"11111"));
        table.insert(Bytes::from_static(b"bbbbb"), Bytes::from_static(b"22222"));
        assert_eq!(table.len(), 2);
        assert_eq!(table.size(), 84);

        // 3件目挿入で最古（aaaaa）が退避される
        table.insert(Bytes::from_static(b"ccccc"), Bytes::from_static(b"33333"));
        assert_eq!(table.len(), 2);
        assert_eq!(table.size(), 84);
        assert!(table.find_name(b"aaaaa").is_none());
        assert_eq!(table.get(1).unwrap().name, b"ccccc".as_slice());
        assert_eq!(table.get(2).unwrap().name, b"bbbbb".as_slice());

        // set_max_size を絞るとサイズ会計を保ったまま追加退避される
        table.set_max_size(42);
        assert_eq!(table.len(), 1);
        assert_eq!(table.size(), 42);
    }

    #[test]
    fn test_header_field_size() {
        let field = HeaderField::new(b"content-type".as_slice(), b"text/html".as_slice());
        // 12 + 9 + 32 = 53
        assert_eq!(field.size(), 53);
    }
}
