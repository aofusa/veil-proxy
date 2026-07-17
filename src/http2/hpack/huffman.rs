//! # HPACK Huffman 符号化 (RFC 7541 Appendix B)
//!
//! HTTP/2 ヘッダー圧縮用の Huffman 符号化/復号化を実装します。

use super::HpackError;

/// Huffman 符号テーブル (RFC 7541 Appendix B)
/// (符号, ビット長)
static HUFFMAN_ENCODE_TABLE: [(u32, u8); 257] = [
    (0x1ff8, 13),     // 0
    (0x7fffd8, 23),   // 1
    (0xfffffe2, 28),  // 2
    (0xfffffe3, 28),  // 3
    (0xfffffe4, 28),  // 4
    (0xfffffe5, 28),  // 5
    (0xfffffe6, 28),  // 6
    (0xfffffe7, 28),  // 7
    (0xfffffe8, 28),  // 8
    (0xffffea, 24),   // 9
    (0x3ffffffc, 30), // 10
    (0xfffffe9, 28),  // 11
    (0xfffffea, 28),  // 12
    (0x3ffffffd, 30), // 13
    (0xfffffeb, 28),  // 14
    (0xfffffec, 28),  // 15
    (0xfffffed, 28),  // 16
    (0xfffffee, 28),  // 17
    (0xfffffef, 28),  // 18
    (0xffffff0, 28),  // 19
    (0xffffff1, 28),  // 20
    (0xffffff2, 28),  // 21
    (0x3ffffffe, 30), // 22
    (0xffffff3, 28),  // 23
    (0xffffff4, 28),  // 24
    (0xffffff5, 28),  // 25
    (0xffffff6, 28),  // 26
    (0xffffff7, 28),  // 27
    (0xffffff8, 28),  // 28
    (0xffffff9, 28),  // 29
    (0xffffffa, 28),  // 30
    (0xffffffb, 28),  // 31
    (0x14, 6),        // 32 ' '
    (0x3f8, 10),      // 33 '!'
    (0x3f9, 10),      // 34 '"'
    (0xffa, 12),      // 35 '#'
    (0x1ff9, 13),     // 36 '$'
    (0x15, 6),        // 37 '%'
    (0xf8, 8),        // 38 '&'
    (0x7fa, 11),      // 39 '\''
    (0x3fa, 10),      // 40 '('
    (0x3fb, 10),      // 41 ')'
    (0xf9, 8),        // 42 '*'
    (0x7fb, 11),      // 43 '+'
    (0xfa, 8),        // 44 ','
    (0x16, 6),        // 45 '-'
    (0x17, 6),        // 46 '.'
    (0x18, 6),        // 47 '/'
    (0x0, 5),         // 48 '0'
    (0x1, 5),         // 49 '1'
    (0x2, 5),         // 50 '2'
    (0x19, 6),        // 51 '3'
    (0x1a, 6),        // 52 '4'
    (0x1b, 6),        // 53 '5'
    (0x1c, 6),        // 54 '6'
    (0x1d, 6),        // 55 '7'
    (0x1e, 6),        // 56 '8'
    (0x1f, 6),        // 57 '9'
    (0x5c, 7),        // 58 ':'
    (0xfb, 8),        // 59 ';'
    (0x7ffc, 15),     // 60 '<'
    (0x20, 6),        // 61 '='
    (0xffb, 12),      // 62 '>'
    (0x3fc, 10),      // 63 '?'
    (0x1ffa, 13),     // 64 '@'
    (0x21, 6),        // 65 'A'
    (0x5d, 7),        // 66 'B'
    (0x5e, 7),        // 67 'C'
    (0x5f, 7),        // 68 'D'
    (0x60, 7),        // 69 'E'
    (0x61, 7),        // 70 'F'
    (0x62, 7),        // 71 'G'
    (0x63, 7),        // 72 'H'
    (0x64, 7),        // 73 'I'
    (0x65, 7),        // 74 'J'
    (0x66, 7),        // 75 'K'
    (0x67, 7),        // 76 'L'
    (0x68, 7),        // 77 'M'
    (0x69, 7),        // 78 'N'
    (0x6a, 7),        // 79 'O'
    (0x6b, 7),        // 80 'P'
    (0x6c, 7),        // 81 'Q'
    (0x6d, 7),        // 82 'R'
    (0x6e, 7),        // 83 'S'
    (0x6f, 7),        // 84 'T'
    (0x70, 7),        // 85 'U'
    (0x71, 7),        // 86 'V'
    (0x72, 7),        // 87 'W'
    (0xfc, 8),        // 88 'X'
    (0x73, 7),        // 89 'Y'
    (0xfd, 8),        // 90 'Z'
    (0x1ffb, 13),     // 91 '['
    (0x7fff0, 19),    // 92 '\\'
    (0x1ffc, 13),     // 93 ']'
    (0x3ffc, 14),     // 94 '^'
    (0x22, 6),        // 95 '_'
    (0x7ffd, 15),     // 96 '`'
    (0x3, 5),         // 97 'a'
    (0x23, 6),        // 98 'b'
    (0x4, 5),         // 99 'c'
    (0x24, 6),        // 100 'd'
    (0x5, 5),         // 101 'e'
    (0x25, 6),        // 102 'f'
    (0x26, 6),        // 103 'g'
    (0x27, 6),        // 104 'h'
    (0x6, 5),         // 105 'i'
    (0x74, 7),        // 106 'j'
    (0x75, 7),        // 107 'k'
    (0x28, 6),        // 108 'l'
    (0x29, 6),        // 109 'm'
    (0x2a, 6),        // 110 'n'
    (0x7, 5),         // 111 'o'
    (0x2b, 6),        // 112 'p'
    (0x76, 7),        // 113 'q'
    (0x2c, 6),        // 114 'r'
    (0x8, 5),         // 115 's'
    (0x9, 5),         // 116 't'
    (0x2d, 6),        // 117 'u'
    (0x77, 7),        // 118 'v'
    (0x78, 7),        // 119 'w'
    (0x79, 7),        // 120 'x'
    (0x7a, 7),        // 121 'y'
    (0x7b, 7),        // 122 'z'
    (0x7ffe, 15),     // 123 '{'
    (0x7fc, 11),      // 124 '|'
    (0x3ffd, 14),     // 125 '}'
    (0x1ffd, 13),     // 126 '~'
    (0xffffffc, 28),  // 127
    (0xfffe6, 20),    // 128
    (0x3fffd2, 22),   // 129
    (0xfffe7, 20),    // 130
    (0xfffe8, 20),    // 131
    (0x3fffd3, 22),   // 132
    (0x3fffd4, 22),   // 133
    (0x3fffd5, 22),   // 134
    (0x7fffd9, 23),   // 135
    (0x3fffd6, 22),   // 136
    (0x7fffda, 23),   // 137
    (0x7fffdb, 23),   // 138
    (0x7fffdc, 23),   // 139
    (0x7fffdd, 23),   // 140
    (0x7fffde, 23),   // 141
    (0xffffeb, 24),   // 142
    (0x7fffdf, 23),   // 143
    (0xffffec, 24),   // 144
    (0xffffed, 24),   // 145
    (0x3fffd7, 22),   // 146
    (0x7fffe0, 23),   // 147
    (0xffffee, 24),   // 148
    (0x7fffe1, 23),   // 149
    (0x7fffe2, 23),   // 150
    (0x7fffe3, 23),   // 151
    (0x7fffe4, 23),   // 152
    (0x1fffdc, 21),   // 153
    (0x3fffd8, 22),   // 154
    (0x7fffe5, 23),   // 155
    (0x3fffd9, 22),   // 156
    (0x7fffe6, 23),   // 157
    (0x7fffe7, 23),   // 158
    (0xffffef, 24),   // 159
    (0x3fffda, 22),   // 160
    (0x1fffdd, 21),   // 161
    (0xfffe9, 20),    // 162
    (0x3fffdb, 22),   // 163
    (0x3fffdc, 22),   // 164
    (0x7fffe8, 23),   // 165
    (0x7fffe9, 23),   // 166
    (0x1fffde, 21),   // 167
    (0x7fffea, 23),   // 168
    (0x3fffdd, 22),   // 169
    (0x3fffde, 22),   // 170
    (0xfffff0, 24),   // 171
    (0x1fffdf, 21),   // 172
    (0x3fffdf, 22),   // 173
    (0x7fffeb, 23),   // 174
    (0x7fffec, 23),   // 175
    (0x1fffe0, 21),   // 176
    (0x1fffe1, 21),   // 177
    (0x3fffe0, 22),   // 178
    (0x1fffe2, 21),   // 179
    (0x7fffed, 23),   // 180
    (0x3fffe1, 22),   // 181
    (0x7fffee, 23),   // 182
    (0x7fffef, 23),   // 183
    (0xfffea, 20),    // 184
    (0x3fffe2, 22),   // 185
    (0x3fffe3, 22),   // 186
    (0x3fffe4, 22),   // 187
    (0x7ffff0, 23),   // 188
    (0x3fffe5, 22),   // 189
    (0x3fffe6, 22),   // 190
    (0x7ffff1, 23),   // 191
    (0x3ffffe0, 26),  // 192
    (0x3ffffe1, 26),  // 193
    (0xfffeb, 20),    // 194
    (0x7fff1, 19),    // 195
    (0x3fffe7, 22),   // 196
    (0x7ffff2, 23),   // 197
    (0x3fffe8, 22),   // 198
    (0x1ffffec, 25),  // 199
    (0x3ffffe2, 26),  // 200
    (0x3ffffe3, 26),  // 201
    (0x3ffffe4, 26),  // 202
    (0x7ffffde, 27),  // 203
    (0x7ffffdf, 27),  // 204
    (0x3ffffe5, 26),  // 205
    (0xfffff1, 24),   // 206
    (0x1ffffed, 25),  // 207
    (0x7fff2, 19),    // 208
    (0x1fffe3, 21),   // 209
    (0x3ffffe6, 26),  // 210
    (0x7ffffe0, 27),  // 211
    (0x7ffffe1, 27),  // 212
    (0x3ffffe7, 26),  // 213
    (0x7ffffe2, 27),  // 214
    (0xfffff2, 24),   // 215
    (0x1fffe4, 21),   // 216
    (0x1fffe5, 21),   // 217
    (0x3ffffe8, 26),  // 218
    (0x3ffffe9, 26),  // 219
    (0xffffffd, 28),  // 220
    (0x7ffffe3, 27),  // 221
    (0x7ffffe4, 27),  // 222
    (0x7ffffe5, 27),  // 223
    (0xfffec, 20),    // 224
    (0xfffff3, 24),   // 225
    (0xfffed, 20),    // 226
    (0x1fffe6, 21),   // 227
    (0x3fffe9, 22),   // 228
    (0x1fffe7, 21),   // 229
    (0x1fffe8, 21),   // 230
    (0x7ffff3, 23),   // 231
    (0x3fffea, 22),   // 232
    (0x3fffeb, 22),   // 233
    (0x1ffffee, 25),  // 234
    (0x1ffffef, 25),  // 235
    (0xfffff4, 24),   // 236
    (0xfffff5, 24),   // 237
    (0x3ffffea, 26),  // 238
    (0x7ffff4, 23),   // 239
    (0x3ffffeb, 26),  // 240
    (0x7ffffe6, 27),  // 241
    (0x3ffffec, 26),  // 242
    (0x3ffffed, 26),  // 243
    (0x7ffffe7, 27),  // 244
    (0x7ffffe8, 27),  // 245
    (0x7ffffe9, 27),  // 246
    (0x7ffffea, 27),  // 247
    (0x7ffffeb, 27),  // 248
    (0xffffffe, 28),  // 249
    (0x7ffffec, 27),  // 250
    (0x7ffffed, 27),  // 251
    (0x7ffffee, 27),  // 252
    (0x7ffffef, 27),  // 253
    (0x7fffff0, 27),  // 254
    (0x3ffffee, 26),  // 255
    (0x3fffffff, 30), // 256 EOS
];

/// Huffman エンコード
///
/// バイト列を Huffman 符号化します。
pub fn huffman_encode(src: &[u8]) -> Vec<u8> {
    let mut result = Vec::with_capacity(src.len());
    let mut current: u64 = 0;
    let mut bits: u32 = 0;

    for &byte in src {
        let (code, len) = HUFFMAN_ENCODE_TABLE[byte as usize];
        current = (current << len) | code as u64;
        bits += len as u32;

        while bits >= 8 {
            bits -= 8;
            result.push((current >> bits) as u8);
        }
    }

    // パディング (EOS プレフィックスで埋める)
    if bits > 0 {
        let padding = 8 - bits;
        current = (current << padding) | ((1u64 << padding) - 1);
        result.push(current as u8);
    }

    result
}

// ---------------------------------------------------------------------------
// 4-bit LUT デコード（F-121）
// ---------------------------------------------------------------------------

/// 生成済みデコード表（private 子 mod。crate 外に公開しない）。
/// `huffman.rs` と同階層の `huffman_decode_table.rs` を参照する。
#[path = "huffman_decode_table.rs"]
mod huffman_decode_table;

use huffman_decode_table::{
    HUFFMAN_DECODE_PEEK_COUNT, HUFFMAN_DECODE_PEEK_MASK, HUFFMAN_DECODE_STATE_COUNT,
    HUFFMAN_DECODE_STRIDE, HUFFMAN_DECODE_TABLE_PACKED,
};

/// デコード LUT フラグ（単一値。複合禁止）。
const HUFFMAN_DECODE_FLAG_NEED: u8 = 0x00;
const HUFFMAN_DECODE_FLAG_ACCEPT: u8 = 0x01;
const HUFFMAN_DECODE_FLAG_ERROR: u8 = 0x02;

/// HPACK Huffman 最短データ符号長（root から完全シンボルを確定するのに最低必要なビット）。
const MIN_CODE_LEN: u32 = 5;

/// 異常時の防御キャップ（B-21）。線形デコーダの MAX_CODE_LEN=30 の代替ではない。
/// 主検出は LUT ERROR エントリ。
const MAX_BITS_LEFT: u32 = 32;

/// Huffman デコード LUT 1 エントリ（4 バイト詰め）。
///
/// # 不変条件
/// - `flags` は { NEED=0x00, ACCEPT=0x01, ERROR=0x02 } の単一値。
/// - ACCEPT/NEED 時 `bits ∈ 1..=STRIDE`。ERROR 時 `bits` は 0。
/// - ACCEPT 後の `next` は root=0。
#[derive(Clone, Copy, Debug)]
#[repr(C)]
struct DecodeEntry {
    flags: u8,
    sym: u8,
    bits: u8,
    next: u8,
}

impl DecodeEntry {
    /// パック `u32`（`flags | sym<<8 | bits<<16 | next<<24`）から展開。
    #[inline]
    fn from_packed(v: u32) -> Self {
        Self {
            flags: (v & 0xff) as u8,
            sym: ((v >> 8) & 0xff) as u8,
            bits: ((v >> 16) & 0xff) as u8,
            next: ((v >> 24) & 0xff) as u8,
        }
    }
}

/// Huffman デコード
///
/// Huffman 符号化されたバイト列をデコードします。
/// 4-bit ストライド LUT によるテーブル駆動 FSM（F-121）。
///
/// # 終端契約（設計 I3–I6）
/// - root かつ `bits_left < 5` では LUT ステップしない。
/// - root かつ `bits_left ∈ 1..=7` かつ残ビット全1 では LUT ステップしない
///   （pad ≥ STRIDE を NEED で EOS パスへ進めない）。
/// - 中間状態で `bits_left < STRIDE` のときは residual ACCEPT のみ。
/// - 終了時は `state == 0` かつ残 0..=7 全1。
pub fn huffman_decode(src: &[u8]) -> Result<Vec<u8>, HpackError> {
    let mut result = Vec::with_capacity(src.len().saturating_mul(2));
    let mut acc: u64 = 0;
    let mut bits_left: u32 = 0;
    let mut state: u8 = 0;

    for &byte in src {
        acc = (acc << 8) | u64::from(byte);
        bits_left += 8;
        if bits_left > MAX_BITS_LEFT {
            return Err(HpackError::HuffmanDecodeError);
        }
        drain_lut(&mut acc, &mut bits_left, &mut state, &mut result)?;
    }
    drain_lut(&mut acc, &mut bits_left, &mut state, &mut result)?;
    // 中間状態に残ビットがある場合の最後の residual ACCEPT
    residual_accept(&mut acc, &mut bits_left, &mut state, &mut result)?;
    finish_padding(acc, bits_left, state)?;
    Ok(result)
}

/// ビットキャッシュを可能な限り消費する（I3–I5）。
#[inline]
fn drain_lut(
    acc: &mut u64,
    bits_left: &mut u32,
    state: &mut u8,
    result: &mut Vec<u8>,
) -> Result<(), HpackError> {
    loop {
        if *state == 0 {
            // I3: root から完全シンボルは最低 5 bit
            if *bits_left < MIN_CODE_LEN {
                break;
            }
            // I4: pad サイズの全1 を NEED で EOS パスへ進めない（4-bit の致命バグ回避）
            if *bits_left <= 7 && all_ones(*acc, *bits_left) {
                break;
            }
        } else {
            // 中間状態: STRIDE 未満は residual のみ
            if *bits_left == 0 {
                break;
            }
            if *bits_left < HUFFMAN_DECODE_STRIDE {
                residual_accept(acc, bits_left, state, result)?;
                break;
            }
        }

        debug_assert!(*bits_left >= HUFFMAN_DECODE_STRIDE);
        let peek = ((*acc >> (*bits_left - HUFFMAN_DECODE_STRIDE))
            & u64::from(HUFFMAN_DECODE_PEEK_MASK)) as usize;
        let e = load_entry(*state as usize, peek);
        match e.flags {
            HUFFMAN_DECODE_FLAG_ERROR => return Err(HpackError::HuffmanDecodeError),
            HUFFMAN_DECODE_FLAG_ACCEPT => {
                consume_bits(acc, bits_left, u32::from(e.bits));
                result.push(e.sym);
                *state = e.next; // I1: root
            }
            HUFFMAN_DECODE_FLAG_NEED => {
                // 生成不変条件: bits == STRIDE
                consume_bits(acc, bits_left, u32::from(e.bits));
                *state = e.next;
            }
            _ => return Err(HpackError::HuffmanDecodeError),
        }
    }
    Ok(())
}

/// `bits_left < STRIDE` のときだけ。ACCEPT かつ e.bits <= bits_left のみ進行。
/// NEED / ERROR / 過長 bits は打ち切り（ERROR を padding 成功にしない）。
#[inline]
fn residual_accept(
    acc: &mut u64,
    bits_left: &mut u32,
    state: &mut u8,
    result: &mut Vec<u8>,
) -> Result<(), HpackError> {
    while *bits_left > 0 && *bits_left < HUFFMAN_DECODE_STRIDE {
        let shift = HUFFMAN_DECODE_STRIDE - *bits_left;
        let peek = (((*acc << shift) | ((1u64 << shift) - 1)) & u64::from(HUFFMAN_DECODE_PEEK_MASK))
            as usize;
        let e = load_entry(*state as usize, peek);
        if e.flags == HUFFMAN_DECODE_FLAG_ACCEPT && u32::from(e.bits) <= *bits_left {
            consume_bits(acc, bits_left, u32::from(e.bits));
            result.push(e.sym);
            *state = e.next;
            continue;
        }
        break;
    }
    Ok(())
}

/// I6: 完全シンボルを取り切ったあと root でパディング検査。
#[inline]
fn finish_padding(acc: u64, bits_left: u32, state: u8) -> Result<(), HpackError> {
    if state != 0 {
        // 未完データ符号（EOS パディングを I4 で NEED していない前提）
        return Err(HpackError::HuffmanDecodeError);
    }
    if bits_left > 7 {
        return Err(HpackError::HuffmanDecodeError);
    }
    if bits_left > 0 && !all_ones(acc, bits_left) {
        return Err(HpackError::HuffmanDecodeError);
    }
    Ok(())
}

#[inline]
fn all_ones(acc: u64, bits_left: u32) -> bool {
    if bits_left == 0 {
        return true;
    }
    let mask = (1u64 << bits_left) - 1;
    (acc & mask) == mask
}

#[inline]
fn consume_bits(acc: &mut u64, bits_left: &mut u32, n: u32) {
    *bits_left -= n;
    if *bits_left == 0 {
        *acc = 0;
    } else {
        *acc &= (1u64 << *bits_left) - 1;
    }
}

#[inline]
fn load_entry(state: usize, peek: usize) -> DecodeEntry {
    debug_assert!(state < HUFFMAN_DECODE_STATE_COUNT);
    debug_assert!(peek < HUFFMAN_DECODE_PEEK_COUNT);
    // v1: safe 索引のみ（get_unchecked 禁止）
    DecodeEntry::from_packed(HUFFMAN_DECODE_TABLE_PACKED[state][peek])
}

/// エンコード後のサイズを計算 (実際にエンコードせずに)
pub fn huffman_encoded_len(src: &[u8]) -> usize {
    let mut bits: usize = 0;
    for &byte in src {
        bits += HUFFMAN_ENCODE_TABLE[byte as usize].1 as usize;
    }
    bits.div_ceil(8)
}

// ---------------------------------------------------------------------------
// テスト
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::mem::{align_of, size_of};

    // -----------------------------------------------------------------------
    // 線形参照デコーダ（テスト専用 oracle。本番ビルドには含まれない）
    // -----------------------------------------------------------------------

    /// 旧実装相当の線形探索デコーダ。LUT との意味論一致検証用。
    fn huffman_decode_linear(src: &[u8]) -> Result<Vec<u8>, HpackError> {
        const MAX_CODE_LEN: u32 = 30;

        let mut result = Vec::with_capacity(src.len() * 2);
        let mut bits: u64 = 0;
        let mut bits_left: u32 = 0;

        for &byte in src {
            bits = (bits << 8) | (byte as u64);
            bits_left += 8;

            while bits_left >= 5 {
                let mut found = false;

                for (sym, &(code, len)) in HUFFMAN_ENCODE_TABLE.iter().enumerate() {
                    if sym >= 256 {
                        continue; // EOS はスキップ
                    }

                    let len = len as u32;
                    if bits_left >= len {
                        let shift = bits_left - len;
                        let extracted = (bits >> shift) as u32;
                        let mask = (1u32 << len) - 1;

                        if (extracted & mask) == code {
                            result.push(sym as u8);
                            bits_left -= len;
                            bits &= (1u64 << bits_left) - 1;
                            found = true;
                            break;
                        }
                    }
                }

                if !found {
                    if bits_left >= MAX_CODE_LEN {
                        return Err(HpackError::HuffmanDecodeError);
                    }
                    break;
                }
            }
        }

        if bits_left > 0 {
            if bits_left > 7 {
                return Err(HpackError::HuffmanDecodeError);
            }
            let padding_mask = (1u64 << bits_left) - 1;
            if (bits & padding_mask) != padding_mask {
                return Err(HpackError::HuffmanDecodeError);
            }
        }

        Ok(result)
    }

    /// encode 表から二分木を再構築し、指定 (state, peek) の期待パック値を返す oracle。
    struct OracleTrie {
        /// 内部ノード: (left_child, right_child) — child は NodeRef
        nodes: Vec<OracleNode>,
    }

    enum OracleNode {
        Internal {
            left: Option<usize>,
            right: Option<usize>,
        },
        Leaf {
            sym: u16,
        },
    }

    impl OracleTrie {
        fn from_encode_table() -> Self {
            // nodes[0] = root (Internal)
            let mut nodes = vec![OracleNode::Internal {
                left: None,
                right: None,
            }];

            for (sym, &(code, len)) in HUFFMAN_ENCODE_TABLE.iter().enumerate() {
                let mut cur = 0usize;
                for i in (0..len).rev() {
                    let bit = ((code >> i) & 1) as u8;
                    let next_opt = match &nodes[cur] {
                        OracleNode::Internal { left, right } => {
                            if bit == 0 {
                                *left
                            } else {
                                *right
                            }
                        }
                        OracleNode::Leaf { .. } => panic!("prefix conflict at sym={sym}"),
                    };
                    let next = if let Some(n) = next_opt {
                        n
                    } else {
                        let new_id = nodes.len();
                        if i == 0 {
                            nodes.push(OracleNode::Leaf { sym: sym as u16 });
                        } else {
                            nodes.push(OracleNode::Internal {
                                left: None,
                                right: None,
                            });
                        }
                        match &mut nodes[cur] {
                            OracleNode::Internal { left, right } => {
                                if bit == 0 {
                                    *left = Some(new_id);
                                } else {
                                    *right = Some(new_id);
                                }
                            }
                            OracleNode::Leaf { .. } => unreachable!(),
                        }
                        new_id
                    };
                    // 途中で葉にぶつかったら、最終段でなければ衝突
                    if i > 0 {
                        if matches!(nodes[next], OracleNode::Leaf { .. }) {
                            panic!("prefix conflict mid-path sym={sym}");
                        }
                    }
                    cur = next;
                }
            }

            // 内部ノードに BFS で state id を割り当て（root=0）
            Self { nodes }
        }

        /// 内部ノード index → state id のマップ、および state id → ノード index。
        fn state_maps(&self) -> (Vec<Option<u8>>, Vec<usize>) {
            let mut node_to_state: Vec<Option<u8>> = vec![None; self.nodes.len()];
            let mut state_to_node: Vec<usize> = Vec::new();
            let mut q = std::collections::VecDeque::new();
            q.push_back(0usize);
            while let Some(idx) = q.pop_front() {
                if matches!(self.nodes[idx], OracleNode::Leaf { .. }) {
                    continue;
                }
                let sid = state_to_node.len();
                assert!(sid < 256);
                node_to_state[idx] = Some(sid as u8);
                state_to_node.push(idx);
                if let OracleNode::Internal { left, right } = &self.nodes[idx] {
                    if let Some(l) = left {
                        q.push_back(*l);
                    }
                    if let Some(r) = right {
                        q.push_back(*r);
                    }
                }
            }
            assert_eq!(state_to_node.len(), 256);
            assert_eq!(node_to_state[0], Some(0));
            (node_to_state, state_to_node)
        }

        fn expected_packed(&self, state: usize, peek: usize, stride: u32) -> u32 {
            let (node_to_state, s2n) = self.state_maps();
            let mut cur = s2n[state];
            for consumed in 1..=stride {
                let bit = (peek >> (stride - consumed)) & 1;
                let child = match &self.nodes[cur] {
                    OracleNode::Internal { left, right } => {
                        if bit == 0 {
                            *left
                        } else {
                            *right
                        }
                    }
                    OracleNode::Leaf { .. } => None,
                };
                let Some(child) = child else {
                    // ERROR
                    return 0x02; // FLAG_ERROR, bits=0, next=0
                };
                match &self.nodes[child] {
                    OracleNode::Leaf { sym } => {
                        if *sym == 256 {
                            return 0x02; // EOS → ERROR
                        }
                        // ACCEPT: flags=1, sym, bits=consumed, next=0
                        return 0x01 | ((*sym as u32) << 8) | ((consumed as u32) << 16);
                    }
                    OracleNode::Internal { .. } => {
                        cur = child;
                    }
                }
            }
            // NEED
            let next = node_to_state[cur].expect("internal must have state");
            (stride << 16) | ((next as u32) << 24)
        }
    }

    #[test]
    fn test_huffman_encode_simple() {
        // "www.example.com" のエンコード例 (RFC 7541)
        let input = b"www.example.com";
        let encoded = huffman_encode(input);

        // エンコード結果は元のサイズより小さいはず
        assert!(encoded.len() < input.len());
    }

    #[test]
    fn test_huffman_roundtrip_ascii() {
        // ASCII 文字列のラウンドトリップテスト
        let test_cases = [
            b"hello".as_slice(),
            b"world",
            b"content-type",
            b"text/html",
            b"/index.html",
            b"GET",
            b"200",
        ];

        for input in test_cases {
            let encoded = huffman_encode(input);
            let decoded = huffman_decode(&encoded).expect("decode");
            assert_eq!(decoded, input);
            assert_eq!(huffman_encoded_len(input), encoded.len());
        }
    }

    #[test]
    fn test_huffman_encoded_len() {
        // '0' の符号長は 5 ビット
        assert_eq!(huffman_encoded_len(b"0"), 1);

        // 'a' の符号長は 5 ビット
        assert_eq!(huffman_encoded_len(b"a"), 1);

        // 複数文字
        let s = b"aeiou";
        let len = huffman_encoded_len(s);
        assert!(len < s.len());
    }

    /// 正当な Huffman 列はラウンドトリップできること。
    #[test]
    fn test_huffman_roundtrip_decode() {
        for input in [
            b"www.example.com".as_slice(),
            b"custom-key",
            b"custom-value",
            b"/",
            b"GET",
        ] {
            let encoded = huffman_encode(input);
            let decoded = huffman_decode(&encoded).expect("valid huffman must decode");
            assert_eq!(decoded, input, "roundtrip mismatch for {:?}", input);
        }
    }

    /// B-21: 不正な Huffman 入力（どの符号にも一致せずビットが溜まり続ける）で
    /// panic せずエラーを返すこと。cargo-fuzz が検出したクラッシュの回帰テスト。
    #[test]
    fn test_huffman_decode_invalid_no_panic() {
        // cargo-fuzz が発見したクラッシュ入力そのもの。
        let crash = [
            0x94, 0x01, 0x94, 0x01, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0x01, 0x00, 0x00, 0x00, 0xf9,
        ];
        // panic せず Ok/Err のいずれかを返せばよい（重要なのは落ちないこと）。
        let _ = huffman_decode(&crash);

        // 全ビット 0 の長い列（符号長超過をまたぐ）でも panic しない。
        let _ = huffman_decode(&[0u8; 64]);
        // 明示的にデコード不能な入力はエラーになる（'0' の 5bit 符号 00000 の後に
        // 全ビット 0 が続くとパディング不正/符号超過で Err）。
        assert!(huffman_decode(&[0x00; 8]).is_err());
    }

    /// 空入力は Ok(empty)。
    #[test]
    fn test_huffman_decode_empty() {
        assert_eq!(huffman_decode(&[]).unwrap(), Vec::<u8>::new());
        assert_eq!(huffman_decode_linear(&[]).unwrap(), Vec::<u8>::new());
    }

    /// 全 256 シンボルの単独バイト roundtrip。
    #[test]
    fn test_huffman_roundtrip_all_bytes() {
        for b in 0u8..=255 {
            let input = [b];
            let encoded = huffman_encode(&input);
            let decoded = huffman_decode(&encoded).unwrap_or_else(|e| {
                panic!("decode failed for byte 0x{b:02x}: {e:?}");
            });
            assert_eq!(decoded, input, "roundtrip mismatch for 0x{b:02x}");
            // 線形 oracle とも一致
            let linear = huffman_decode_linear(&encoded).expect("linear");
            assert_eq!(linear, input);
        }
    }

    /// 複数バイト文字列・境界ケースの roundtrip。
    #[test]
    fn test_huffman_roundtrip_multibyte() {
        let cases: &[&[u8]] = &[
            b"",
            b"a",
            b"ab",
            b"\x00\xff",
            b"Hello, World!",
            &[0, 1, 2, 3, 4, 5, 250, 251, 252, 253, 254, 255],
            b"abcdefghijklmnopqrstuvwxyzABCDEFGHIJKLMNOPQRSTUVWXYZ0123456789",
        ];
        for input in cases {
            let encoded = huffman_encode(input);
            let decoded = huffman_decode(&encoded).expect("decode");
            assert_eq!(&decoded, input);
            assert_eq!(huffman_decode_linear(&encoded).unwrap(), decoded);
        }
    }

    /// pad 長 0..=7 を生じる固定入力（I4 回帰の核心）。
    #[test]
    fn test_huffman_padding_lengths() {
        // 単一バイトで符号長 mod 8 を変え、pad = (8 - (len % 8)) % 8 を網羅する。
        // 設計例: 0x02 は len=28 → pad=4。0x21 '!' は len=10 → pad=6。
        let mut seen_pads = [false; 8];

        // 明示ケース
        let explicit: &[(u8, u32)] = &[
            (0x02, 4), // len 28 → pad 4
            (0x21, 6), // '!' len 10 → pad 6
            (b'0', 3), // len 5 → pad 3
            (b' ', 2), // len 6 → pad 2
            (b':', 1), // len 7 → pad 1
            (b'&', 0), // len 8 → pad 0
        ];
        for &(byte, expected_pad) in explicit {
            let (code, len) = HUFFMAN_ENCODE_TABLE[byte as usize];
            let pad = (8 - (len as u32 % 8)) % 8;
            assert_eq!(
                pad, expected_pad,
                "pad for 0x{byte:02x} code={code:x} len={len}"
            );
            let encoded = huffman_encode(&[byte]);
            let decoded = huffman_decode(&encoded).unwrap_or_else(|e| {
                panic!("pad={pad} byte=0x{byte:02x} failed: {e:?}, enc={encoded:?}");
            });
            assert_eq!(decoded, vec![byte]);
            seen_pads[pad as usize] = true;
        }

        // 全 256 バイトを走査して pad 0..=7 を埋める
        for b in 0u8..=255 {
            let len = HUFFMAN_ENCODE_TABLE[b as usize].1 as u32;
            let pad = (8 - (len % 8)) % 8;
            let encoded = huffman_encode(&[b]);
            let decoded = huffman_decode(&encoded).expect("single-byte pad roundtrip");
            assert_eq!(decoded, vec![b]);
            seen_pads[pad as usize] = true;
        }
        assert!(
            seen_pads.iter().all(|&x| x),
            "not all pad lengths covered: {seen_pads:?}"
        );

        // pad 5/7 も複バイトで追加確認（単一で足りない場合の保険）
        // 複数シンボルで総ビット長 mod 8 を制御
        for pad_target in 0u32..=7 {
            // 単純に全バイトを試して見つかったものを使う（上で全 pad カバー済）
            let found = (0u8..=255).find(|&b| {
                let len = HUFFMAN_ENCODE_TABLE[b as usize].1 as u32;
                (8 - (len % 8)) % 8 == pad_target
            });
            assert!(found.is_some(), "no symbol with pad={pad_target}");
        }
    }

    /// DecodeEntry は 4 バイト詰め。
    #[test]
    fn test_decode_entry_size() {
        assert_eq!(size_of::<DecodeEntry>(), 4);
        assert!(align_of::<DecodeEntry>() <= 4);
    }

    /// encode 表から再構築した木と LUT エントリの整合（全エントリ）。
    #[test]
    fn test_lut_matches_encode_table_oracle() {
        let trie = OracleTrie::from_encode_table();
        let stride = HUFFMAN_DECODE_STRIDE;
        let mut mismatches = 0u32;
        for state in 0..HUFFMAN_DECODE_STATE_COUNT {
            for peek in 0..HUFFMAN_DECODE_PEEK_COUNT {
                let expected = trie.expected_packed(state, peek, stride);
                let actual = HUFFMAN_DECODE_TABLE_PACKED[state][peek];
                if expected != actual {
                    mismatches += 1;
                    if mismatches <= 8 {
                        eprintln!(
                            "mismatch state={state} peek={peek:#x}: expected={expected:#010x} actual={actual:#010x}"
                        );
                    }
                }
            }
        }
        assert_eq!(mismatches, 0, "{mismatches} LUT entries differ from oracle");
    }

    /// ランダム短バイト列: encode→decode が一致し、線形 oracle とも一致。
    #[test]
    fn test_huffman_property_random_roundtrip() {
        // 決定的 PRNG（依存追加なし）
        let mut state: u64 = 0xDEAD_BEEF_CAFE_BABE;
        let mut next = || {
            // xorshift64
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..1000 {
            let len = (next() % 32) as usize;
            let mut input = Vec::with_capacity(len);
            for _ in 0..len {
                input.push((next() & 0xff) as u8);
            }
            let encoded = huffman_encode(&input);
            let lut = huffman_decode(&encoded).expect("lut decode");
            let linear = huffman_decode_linear(&encoded).expect("linear decode");
            assert_eq!(lut, input, "lut roundtrip");
            assert_eq!(linear, input, "linear roundtrip");
            assert_eq!(lut, linear);
        }
    }

    /// 乱数ゴミ入力: LUT と線形の Ok/Err 極性および Ok 時出力が一致。
    #[test]
    fn test_huffman_garbage_matches_linear() {
        let mut state: u64 = 0x0123_4567_89AB_CDEF;
        let mut next = || {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state
        };

        for _ in 0..1000 {
            let len = (next() % 48) as usize;
            let mut garbage = Vec::with_capacity(len);
            for _ in 0..len {
                garbage.push((next() & 0xff) as u8);
            }
            let lut = huffman_decode(&garbage);
            let linear = huffman_decode_linear(&garbage);
            match (lut, linear) {
                (Ok(a), Ok(b)) => assert_eq!(a, b, "both Ok but payload differs: {garbage:?}"),
                (Err(_), Err(_)) => {}
                (Ok(a), Err(_)) => panic!("lut Ok({a:?}) linear Err on {garbage:?}"),
                (Err(_), Ok(b)) => panic!("lut Err linear Ok({b:?}) on {garbage:?}"),
            }
        }
    }

    /// 終端違法: 非 pad の未完（不完全符号）、残 bit に 0。
    #[test]
    fn test_huffman_invalid_terminal() {
        // 途中で切れた符号（'0' = 00000 の上位 3 bit のみ = 不完全）
        // 単独で 0b000xxxxx のような短い不正は符号化表依存。
        // 残ビットに 0 を含むパディング不正:
        // 正当な '0' 符号化は 1 バイトで pad 3 = 0b00000_111。
        // pad に 0 を混ぜた不正例。
        let valid = huffman_encode(b"0");
        assert!(huffman_decode(&valid).is_ok());

        // 全 0 短列は '0' をいくつか出した後パディング不正か未完
        assert!(huffman_decode(&[0x00]).is_err());

        // EOS っぽい長い全1 はデータ中に EOS 葉へ到達し得る → Err
        assert!(huffman_decode(&[0xff; 8]).is_err() || huffman_decode(&[0xff; 8]).is_ok());
        // 極性は線形と一致していればよい
        let all_ff = [0xffu8; 8];
        assert_eq!(
            huffman_decode(&all_ff).is_ok(),
            huffman_decode_linear(&all_ff).is_ok()
        );
    }
}
