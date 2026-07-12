//! HTTP/3 / QPACK ワイヤ形式の純関数パーサ（F-112、ホットパス外）。
//!
//! 本番データプレーンの QPACK / H3 フレーム処理は quiche 内部に委譲している。
//! 本モジュールは **信頼境界で到達し得る任意バイト列** を panic なく解釈できること
//! （ファジング・境界テスト用）を目的とする。ホットパスからは呼び出さない。
//!
//! 参照:
//! - RFC 9000 §16 Variable-Length Integer Encoding
//! - RFC 9114 §7.1 Frame Layout
//! - RFC 9204 §4.1.1 Integer / String Literals（QPACK は HPACK と同一プレフィックス）

/// ワイヤ解析エラー。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WireError {
    /// 入力が不足している
    BufferTooShort,
    /// 整数が表現可能な範囲を超えた / 不正プレフィックス
    IntegerOverflow,
    /// 文字列長が残バッファを超える
    InvalidString,
    /// フレーム種別または長さが不正
    InvalidFrame,
}

/// RFC 9000 可変長整数をデコードする。
///
/// 戻り値: `(value, consumed_bytes)`。
pub fn decode_quic_varint(buf: &[u8]) -> Result<(u64, usize), WireError> {
    if buf.is_empty() {
        return Err(WireError::BufferTooShort);
    }
    let first = buf[0];
    let prefix = first >> 6;
    let (len, mask) = match prefix {
        0 => (1usize, 0x3f),
        1 => (2, 0x3f),
        2 => (4, 0x3f),
        3 => (8, 0x3f),
        _ => unreachable!(),
    };
    if buf.len() < len {
        return Err(WireError::BufferTooShort);
    }
    let mut value = (first & mask) as u64;
    for &b in &buf[1..len] {
        value = (value << 8) | (b as u64);
    }
    Ok((value, len))
}

/// HTTP/3 フレームヘッダ（type + length の varint 列）をデコードする。
///
/// 戻り値: `(frame_type, payload_len, header_bytes)`。
/// ペイロード本体はスライスしない（呼び出し側で残バッファと照合）。
pub fn decode_http3_frame_header(buf: &[u8]) -> Result<(u64, u64, usize), WireError> {
    let (frame_type, t_len) = decode_quic_varint(buf)?;
    let (payload_len, l_len) = decode_quic_varint(&buf[t_len..])?;
    // 異常に巨大な length は DoS 面。解釈はするが後段で切り詰める前提で上限チェックは
    // 呼び出し側（ファザーは payload 境界のみ検証）。
    Ok((frame_type, payload_len, t_len + l_len))
}

/// バッファから可能な限り HTTP/3 フレームを走査する（panic しないことのみ保証）。
///
/// 各フレームについてヘッダを解釈し、ペイロードが揃っていれば消費して次へ進む。
/// 切り詰め・不正は打ち切り。戻り値は正常に消費したフレーム数。
pub fn walk_http3_frames(mut buf: &[u8], max_frames: usize) -> usize {
    let mut count = 0usize;
    while count < max_frames && !buf.is_empty() {
        match decode_http3_frame_header(buf) {
            Ok((_ty, plen, hlen)) => {
                let total = match hlen.checked_add(plen as usize) {
                    Some(t) => t,
                    None => break,
                };
                if buf.len() < total {
                    break;
                }
                buf = &buf[total..];
                count += 1;
            }
            Err(_) => break,
        }
    }
    count
}

/// QPACK / HPACK 共通のプレフィックス整数（RFC 7541 §5.1 / RFC 9204 §4.1.1）。
///
/// `prefix_bits` は 1..=8。戻り値: `(value, consumed)`。
pub fn decode_qpack_integer(buf: &[u8], prefix_bits: u8) -> Result<(u64, usize), WireError> {
    if !(1..=8).contains(&prefix_bits) || buf.is_empty() {
        return Err(WireError::BufferTooShort);
    }
    let mask = if prefix_bits == 8 {
        0xffu8
    } else {
        (1u8 << prefix_bits) - 1
    };
    let mut value = (buf[0] & mask) as u64;
    if value < mask as u64 {
        return Ok((value, 1));
    }
    let mut m = 0u32;
    let mut i = 1usize;
    loop {
        if i >= buf.len() {
            return Err(WireError::BufferTooShort);
        }
        let b = buf[i];
        i += 1;
        value = value
            .checked_add(((b & 0x7f) as u64).checked_shl(m).ok_or(WireError::IntegerOverflow)?)
            .ok_or(WireError::IntegerOverflow)?;
        m = m.checked_add(7).ok_or(WireError::IntegerOverflow)?;
        if m > 63 {
            return Err(WireError::IntegerOverflow);
        }
        if b & 0x80 == 0 {
            return Ok((value, i));
        }
    }
}

/// QPACK 文字列リテラルプレフィックス（RFC 9204 §4.1.2）。
///
/// 先頭バイトの MSB は Huffman フラグ、残り 7bit が長さプレフィックス。
/// 戻り値: `(huffman, string_bytes_slice_start, string_len, total_consumed_header)`。
/// 実際の文字列バイトは呼び出し側で `buf[start..start+len]` を読む。
pub fn decode_qpack_string_prefix(buf: &[u8]) -> Result<(bool, usize, usize, usize), WireError> {
    if buf.is_empty() {
        return Err(WireError::BufferTooShort);
    }
    let huffman = (buf[0] & 0x80) != 0;
    let (len, consumed) = decode_qpack_integer(buf, 7)?;
    let len = usize::try_from(len).map_err(|_| WireError::IntegerOverflow)?;
    // 長さ宣言が残バッファを超える場合は InvalidString（本文未達）
    if buf.len() < consumed.saturating_add(len) {
        return Err(WireError::InvalidString);
    }
    Ok((huffman, consumed, len, consumed))
}

/// QPACK エンコードされたヘッダブロック断片を走査する（型ビットのみ解釈）。
///
/// 完全な動的テーブルは持たない。各命令の先頭プレフィックス整数を読み進めるだけ。
/// 任意入力で panic しないことが主目的。戻り値は解釈を試みた命令数。
pub fn walk_qpack_block(mut buf: &[u8], max_instructions: usize) -> usize {
    let mut count = 0usize;
    while count < max_instructions && !buf.is_empty() {
        let first = buf[0];
        let advanced = if first & 0x80 != 0 {
            // Indexed Header Field — 6-bit or 7-bit prefix (QPACK uses 6 for post-base etc.)
            match decode_qpack_integer(buf, 6) {
                Ok((_, n)) => n,
                Err(_) => break,
            }
        } else if first & 0x40 != 0 {
            // Literal with name reference / insert — 4 or 6 bit + optional strings
            let (idx, n) = match decode_qpack_integer(buf, 4) {
                Ok(v) => v,
                Err(_) => break,
            };
            let mut pos = n;
            if idx == 0 {
                // name is a string literal
                match decode_qpack_string_prefix(&buf[pos..]) {
                    Ok((_h, start, len, _hdr)) => {
                        pos += start + len;
                    }
                    Err(_) => break,
                }
            }
            match decode_qpack_string_prefix(buf.get(pos..).unwrap_or(&[])) {
                Ok((_h, start, len, _hdr)) => pos + start + len,
                Err(_) => break,
            }
        } else if first & 0x20 != 0 {
            // Dynamic table size update / Set Dynamic Table Capacity — 5-bit prefix
            match decode_qpack_integer(buf, 5) {
                Ok((_, n)) => n,
                Err(_) => break,
            }
        } else {
            // Literal without indexing / other — try 4-bit + strings
            let (idx, n) = match decode_qpack_integer(buf, 4) {
                Ok(v) => v,
                Err(_) => break,
            };
            let mut pos = n;
            if idx == 0 {
                match decode_qpack_string_prefix(&buf[pos..]) {
                    Ok((_h, start, len, _hdr)) => pos += start + len,
                    Err(_) => break,
                }
            }
            match decode_qpack_string_prefix(buf.get(pos..).unwrap_or(&[])) {
                Ok((_h, start, len, _hdr)) => pos + start + len,
                Err(_) => break,
            }
        };

        if advanced == 0 || advanced > buf.len() {
            break;
        }
        buf = &buf[advanced..];
        count += 1;
    }
    count
}

/// ファジング用スモーク: HTTP/3 フレーム列を走査（panic しない）。
pub fn http3_frame_decode_smoke(data: &[u8]) {
    let _ = walk_http3_frames(data, 256);
    // 単一ヘッダ経路も刺激
    if let Ok((ty, plen, hlen)) = decode_http3_frame_header(data) {
        let _ = (ty, plen, hlen);
        if data.len() >= hlen {
            let payload = &data[hlen..];
            let take = (plen as usize).min(payload.len()).min(4096);
            let _ = &payload[..take];
        }
    }
}

/// ファジング用スモーク: QPACK 整数/文字列/ブロック走査（panic しない）。
pub fn qpack_decode_smoke(data: &[u8]) {
    for prefix in [1u8, 4, 5, 6, 7, 8] {
        let _ = decode_qpack_integer(data, prefix);
    }
    let _ = decode_qpack_string_prefix(data);
    let _ = walk_qpack_block(data, 256);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quic_varint_1_byte() {
        let (v, n) = decode_quic_varint(&[0x25]).unwrap();
        assert_eq!((v, n), (0x25, 1));
    }

    #[test]
    fn quic_varint_2_byte() {
        // 0x40 0x25 → prefix=1, value = 0x25
        let (v, n) = decode_quic_varint(&[0x40, 0x25]).unwrap();
        assert_eq!((v, n), (0x25, 2));
    }

    #[test]
    fn http3_frame_header_data() {
        // type=0 (DATA), length=5, payload "hello"
        let mut buf = vec![0x00, 0x05];
        buf.extend_from_slice(b"hello");
        let (ty, plen, hlen) = decode_http3_frame_header(&buf).unwrap();
        assert_eq!(ty, 0);
        assert_eq!(plen, 5);
        assert_eq!(hlen, 2);
        assert_eq!(walk_http3_frames(&buf, 8), 1);
    }

    #[test]
    fn qpack_integer_single() {
        // value 10 with 5-bit prefix: first byte low 5 bits = 10
        let (v, n) = decode_qpack_integer(&[0x0a], 5).unwrap();
        assert_eq!((v, n), (10, 1));
    }

    #[test]
    fn qpack_string_literal() {
        // huffman=0, length=5, "hello"
        let mut buf = vec![0x05];
        buf.extend_from_slice(b"hello");
        let (huff, start, len, _hdr) = decode_qpack_string_prefix(&buf).unwrap();
        assert!(!huff);
        assert_eq!(&buf[start..start + len], b"hello");
    }

    #[test]
    fn smoke_handles_empty_and_random() {
        http3_frame_decode_smoke(b"");
        http3_frame_decode_smoke(&[0xff; 32]);
        qpack_decode_smoke(b"");
        qpack_decode_smoke(&[0xff; 64]);
        // 擬似乱数ミニファザー
        let mut state = 0x9e37_79b9_7f4a_7c15u64;
        for _ in 0..128 {
            let mut buf = [0u8; 48];
            for b in &mut buf {
                state = state
                    .wrapping_mul(6364136223846793005)
                    .wrapping_add(1);
                *b = (state >> 33) as u8;
            }
            http3_frame_decode_smoke(&buf);
            qpack_decode_smoke(&buf);
        }
    }

    #[test]
    fn truncated_inputs_are_errors_not_panics() {
        assert_eq!(
            decode_quic_varint(&[]),
            Err(WireError::BufferTooShort)
        );
        assert_eq!(
            decode_quic_varint(&[0xc0]),
            Err(WireError::BufferTooShort)
        );
        assert!(decode_qpack_string_prefix(&[0x05]).is_err());
    }
}
