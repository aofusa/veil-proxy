//! Proxy-Wasm ABI のマップ直列化（B-19）
//!
//! proxy-wasm Rust SDK（hostcalls.rs `serialize_map` / `deserialize_map`）と
//! 同一のワイヤ形式を実装する:
//!
//! ```text
//! [num_pairs: u32 LE]
//! [key1_size: u32 LE][value1_size: u32 LE] ... （ペア数ぶんのサイズテーブル）
//! [key1 bytes]\0[value1 bytes]\0 ...            （NUL 終端のデータ本体）
//! ```
//!
//! 旧実装は `[num][klen][key][vlen][val]...` のインターリーブ形式で SDK と
//! 互換性がなく、SDK 側の `deserialize_map` が範囲外アクセスで panic していた。

/// ヘッダマップを Proxy-Wasm ABI 形式へ直列化する
pub(crate) fn serialize_headers(headers: &[(Vec<u8>, Vec<u8>)]) -> Vec<u8> {
    let mut size: usize = 4;
    for (key, value) in headers {
        size += key.len() + value.len() + 10;
    }
    let mut buf = Vec::with_capacity(size);

    buf.extend_from_slice(&(headers.len() as u32).to_le_bytes());
    // サイズテーブル
    for (key, value) in headers {
        buf.extend_from_slice(&(key.len() as u32).to_le_bytes());
        buf.extend_from_slice(&(value.len() as u32).to_le_bytes());
    }
    // データ本体（NUL 終端）
    for (key, value) in headers {
        buf.extend_from_slice(key);
        buf.push(0);
        buf.extend_from_slice(value);
        buf.push(0);
    }

    buf
}

/// Proxy-Wasm ABI 形式のヘッダマップを復元する（不正データは None）
pub(crate) fn deserialize_headers(data: &[u8]) -> Option<Vec<(Vec<u8>, Vec<u8>)>> {
    if data.is_empty() {
        return Some(Vec::new());
    }
    if data.len() < 4 {
        return None;
    }

    let num_pairs = u32::from_le_bytes(data[0..4].try_into().ok()?) as usize;

    // サイズテーブルの範囲チェック
    let table_end = 4usize.checked_add(num_pairs.checked_mul(8)?)?;
    if table_end > data.len() {
        return None;
    }

    let mut headers = Vec::with_capacity(num_pairs);
    let mut pos = table_end;

    for n in 0..num_pairs {
        let s = 4 + n * 8;
        let key_len = u32::from_le_bytes(data[s..s + 4].try_into().ok()?) as usize;
        let val_len = u32::from_le_bytes(data[s + 4..s + 8].try_into().ok()?) as usize;

        let key_end = pos.checked_add(key_len)?;
        // key の後の NUL
        if key_end >= data.len() {
            return None;
        }
        let key = data[pos..key_end].to_vec();
        pos = key_end + 1;

        let val_end = pos.checked_add(val_len)?;
        // value の後の NUL（最終要素の NUL は data 長ちょうどまで）
        if val_end >= data.len() {
            return None;
        }
        let value = data[pos..val_end].to_vec();
        pos = val_end + 1;

        headers.push((key, value));
    }

    Some(headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// SDK（proxy-wasm 0.2 hostcalls::serialize_map）と同一のワイヤ形式であること
    #[test]
    fn test_roundtrip_matches_sdk_format() {
        let headers = vec![
            (b":method".to_vec(), b"GET".to_vec()),
            (b":path".to_vec(), b"/api".to_vec()),
            (b"x-empty".to_vec(), b"".to_vec()),
        ];
        let bytes = serialize_headers(&headers);

        // レイアウト検証: [3][7,3][5,4][7,0] + ":method\0GET\0:path\0/api\0x-empty\0\0"
        assert_eq!(&bytes[0..4], &3u32.to_le_bytes());
        assert_eq!(&bytes[4..8], &7u32.to_le_bytes()); // ":method".len()
        assert_eq!(&bytes[8..12], &3u32.to_le_bytes()); // "GET".len()
        let table_end = 4 + 3 * 8;
        assert_eq!(&bytes[table_end..table_end + 7], b":method");
        assert_eq!(bytes[table_end + 7], 0); // NUL 終端

        let parsed = deserialize_headers(&bytes).expect("roundtrip");
        assert_eq!(parsed, headers);
    }

    #[test]
    fn test_empty_map() {
        let bytes = serialize_headers(&[]);
        assert_eq!(&bytes[0..4], &0u32.to_le_bytes());
        assert_eq!(deserialize_headers(&bytes), Some(Vec::new()));
        // SDK は空マップを空バイト列で送ることがある
        assert_eq!(deserialize_headers(&[]), Some(Vec::new()));
    }

    /// 不正データ（サイズテーブル超過・過大 num_pairs）で panic せず None を返すこと
    #[test]
    fn test_malformed_data_is_rejected() {
        assert_eq!(deserialize_headers(&[1, 2, 3]), None);
        // num_pairs = u32::MAX → オーバーフローせず None
        let mut bad = Vec::new();
        bad.extend_from_slice(&u32::MAX.to_le_bytes());
        assert_eq!(deserialize_headers(&bad), None);
        // サイズテーブルがデータ長を超える
        let mut bad2 = Vec::new();
        bad2.extend_from_slice(&2u32.to_le_bytes());
        bad2.extend_from_slice(&[0u8; 8]); // 1 ペア分しかない
        assert_eq!(deserialize_headers(&bad2), None);
        // データ本体が足りない
        let mut bad3 = Vec::new();
        bad3.extend_from_slice(&1u32.to_le_bytes());
        bad3.extend_from_slice(&100u32.to_le_bytes());
        bad3.extend_from_slice(&0u32.to_le_bytes());
        bad3.extend_from_slice(b"short\0\0");
        assert_eq!(deserialize_headers(&bad3), None);
    }
}
