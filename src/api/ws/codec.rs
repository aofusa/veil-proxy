// F-262: RFC 6455 のフレームの復号・符号化（純関数と状態機械。I/O は持たない）。
// 受信はバッファの中でその場でマスクを外して `Bytes` で切り出す（断片化したメッセージだけ組み立て用に写す）。
//! WebSocket frame decoding and encoding (RFC 6455).

use bytes::{Buf, Bytes, BytesMut};

pub(crate) const OP_CONT: u8 = 0x0;
pub(crate) const OP_TEXT: u8 = 0x1;
pub(crate) const OP_BINARY: u8 = 0x2;
pub(crate) const OP_CLOSE: u8 = 0x8;
pub(crate) const OP_PING: u8 = 0x9;
pub(crate) const OP_PONG: u8 = 0xA;

/// Close codes used by the codec.
pub(crate) const CLOSE_NORMAL: u16 = 1000;
pub(crate) const CLOSE_PROTOCOL: u16 = 1002;
pub(crate) const CLOSE_INVALID_DATA: u16 = 1007;
pub(crate) const CLOSE_TOO_BIG: u16 = 1009;

/// A parsed frame header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Header {
    pub fin: bool,
    /// RSV1 (permessage-deflate: the message is compressed).
    pub rsv1: bool,
    pub opcode: u8,
    pub mask: Option<[u8; 4]>,
    /// Payload length.
    pub len: usize,
    /// Header length (2 to 14 bytes).
    pub hlen: usize,
}

/// Parses a frame header. `Ok(None)`: more bytes are needed. `Err(code)`: the close code to fail with.
pub(crate) fn parse_header(
    b: &[u8],
    max_frame: usize,
    require_mask: bool,
    rsv1_ok: bool,
) -> Result<Option<Header>, u16> {
    if b.len() < 2 {
        return Ok(None);
    }
    let fin = b[0] & 0x80 != 0;
    let rsv1 = b[0] & 0x40 != 0;
    // RSV1 は permessage-deflate を交渉したときだけ。RSV2・RSV3 を使う拡張は無い
    if b[0] & 0x30 != 0 || (rsv1 && !rsv1_ok) {
        return Err(CLOSE_PROTOCOL);
    }
    let opcode = b[0] & 0x0F;
    if !matches!(
        opcode,
        OP_CONT | OP_TEXT | OP_BINARY | OP_CLOSE | OP_PING | OP_PONG
    ) {
        return Err(CLOSE_PROTOCOL);
    }
    let masked = b[1] & 0x80 != 0;
    if require_mask && !masked {
        return Err(CLOSE_PROTOCOL);
    }
    let (len, mut hlen) = match b[1] & 0x7F {
        126 => {
            if b.len() < 4 {
                return Ok(None);
            }
            (u16::from_be_bytes([b[2], b[3]]) as u64, 4)
        }
        127 => {
            if b.len() < 10 {
                return Ok(None);
            }
            let mut a = [0u8; 8];
            a.copy_from_slice(&b[2..10]);
            let l = u64::from_be_bytes(a);
            if l >> 63 != 0 {
                return Err(CLOSE_PROTOCOL);
            }
            (l, 10)
        }
        n => (n as u64, 2),
    };
    // 制御フレームは断片化せず 125 バイトまで。RSV1 はメッセージの最初のフレームだけ（制御・継続には付かない）
    if opcode >= OP_CLOSE && (!fin || len > 125)
        || rsv1 && (opcode >= OP_CLOSE || opcode == OP_CONT)
    {
        return Err(CLOSE_PROTOCOL);
    }
    if len > max_frame as u64 {
        return Err(CLOSE_TOO_BIG);
    }
    let mask = if masked {
        if b.len() < hlen + 4 {
            return Ok(None);
        }
        let m = [b[hlen], b[hlen + 1], b[hlen + 2], b[hlen + 3]];
        hlen += 4;
        Some(m)
    } else {
        None
    };
    Ok(Some(Header {
        fin,
        rsv1,
        opcode,
        mask,
        len: len as usize,
        hlen,
    }))
}

/// Removes the mask in place (8 bytes at a time).
pub(crate) fn unmask(buf: &mut [u8], mask: [u8; 4]) {
    let m8 = u64::from_ne_bytes([
        mask[0], mask[1], mask[2], mask[3], mask[0], mask[1], mask[2], mask[3],
    ]);
    let (chunks, rest) = buf.as_chunks_mut::<8>();
    for c in chunks {
        *c = (u64::from_ne_bytes(*c) ^ m8).to_ne_bytes();
    }
    for (i, b) in rest.iter_mut().enumerate() {
        *b ^= mask[i & 3];
    }
}

/// Writes an unmasked (server-to-client) frame header into `out` and returns its length.
pub(crate) fn encode_header(out: &mut [u8; 10], fin: bool, opcode: u8, len: usize) -> usize {
    out[0] = if fin { 0x80 } else { 0 } | opcode;
    if len < 126 {
        out[1] = len as u8;
        2
    } else if len <= u16::MAX as usize {
        out[1] = 126;
        out[2..4].copy_from_slice(&(len as u16).to_be_bytes());
        4
    } else {
        out[1] = 127;
        out[2..10].copy_from_slice(&(len as u64).to_be_bytes());
        10
    }
}

/// Whether `code` may appear in a Close frame received from the peer (RFC 6455 section 7.4).
pub(crate) fn valid_close_code(code: u16) -> bool {
    matches!(code, 1000..=1003 | 1007..=1011 | 3000..=4999)
}

/// A decoded unit: a complete data message or a control frame.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Decoded {
    Text(Bytes),
    Binary(Bytes),
    Ping(Bytes),
    Pong(Bytes),
    /// `None`: no status code.
    Close(Option<(u16, Bytes)>),
    /// A permessage-deflate message (still compressed; UTF-8 is checked after inflating).
    Compressed {
        text: bool,
        data: Bytes,
    },
}

/// Reassembles messages from frames (state across fragments: the opcode, the partial payload and how far it
/// has been checked as UTF-8).
pub(crate) struct Decoder {
    pub max_frame: usize,
    pub max_message: usize,
    require_mask: bool,
    // permessage-deflate を交渉した（RSV1 を受け付ける）。
    pub deflate: bool,
    // 組み立て中のメッセージの opcode（0 = 無し）。
    frag_op: u8,
    // 組み立て中のメッセージが圧縮されている。
    frag_compressed: bool,
    frag: BytesMut,
    // `frag` の先頭から UTF-8 として正しいと確かめた長さ。
    utf8_checked: usize,
    // 次のフレームに要るバイト数（読み足すときの目安）。
    need: usize,
}

impl Decoder {
    pub(crate) fn new(max_frame: usize, max_message: usize, require_mask: bool) -> Decoder {
        Decoder {
            max_frame,
            max_message,
            require_mask,
            deflate: false,
            frag_op: 0,
            frag_compressed: false,
            frag: BytesMut::new(),
            utf8_checked: 0,
            need: 0,
        }
    }

    /// Bytes the next frame needs in total (0 if unknown); used to size the read buffer.
    pub(crate) fn need(&self) -> usize {
        self.need
    }

    /// Takes the next message or control frame out of `rbuf`. `Ok(None)`: more bytes are needed.
    /// `Err(code)`: the connection must be failed with `code`.
    pub(crate) fn decode(&mut self, rbuf: &mut BytesMut) -> Result<Option<Decoded>, u16> {
        loop {
            let Some(h) = parse_header(rbuf, self.max_frame, self.require_mask, self.deflate)?
            else {
                self.need = 0;
                return Ok(None);
            };
            let total = h.hlen + h.len;
            if rbuf.len() < total {
                self.need = total;
                return Ok(None);
            }
            self.need = 0;
            // フレームをバッファから切り出し、その場でマスクを外す（写さない）
            let mut f = rbuf.split_to(total);
            f.advance(h.hlen);
            if let Some(m) = h.mask {
                unmask(&mut f, m);
            }
            let payload = f.freeze();
            match h.opcode {
                OP_PING => return Ok(Some(Decoded::Ping(payload))),
                OP_PONG => return Ok(Some(Decoded::Pong(payload))),
                OP_CLOSE => return close_payload(payload).map(Some),
                OP_TEXT | OP_BINARY => {
                    if self.frag_op != 0 {
                        return Err(CLOSE_PROTOCOL);
                    }
                    if payload.len() > self.max_message {
                        return Err(CLOSE_TOO_BIG);
                    }
                    if h.fin && h.rsv1 {
                        return Ok(Some(Decoded::Compressed {
                            text: h.opcode == OP_TEXT,
                            data: payload,
                        }));
                    }
                    if h.fin {
                        return if h.opcode == OP_TEXT {
                            if std::str::from_utf8(&payload).is_err() {
                                return Err(CLOSE_INVALID_DATA);
                            }
                            Ok(Some(Decoded::Text(payload)))
                        } else {
                            Ok(Some(Decoded::Binary(payload)))
                        };
                    }
                    self.frag_op = h.opcode;
                    self.frag_compressed = h.rsv1;
                    self.frag.clear();
                    self.utf8_checked = 0;
                    self.append(&payload)?;
                }
                _ => {
                    // OP_CONT
                    if self.frag_op == 0 {
                        return Err(CLOSE_PROTOCOL);
                    }
                    self.append(&payload)?;
                    if h.fin {
                        let op = std::mem::replace(&mut self.frag_op, 0);
                        let data = self.frag.split().freeze();
                        if self.frag_compressed {
                            return Ok(Some(Decoded::Compressed {
                                text: op == OP_TEXT,
                                data,
                            }));
                        }
                        if op == OP_TEXT {
                            if self.utf8_checked != data.len() {
                                return Err(CLOSE_INVALID_DATA);
                            }
                            return Ok(Some(Decoded::Text(data)));
                        }
                        return Ok(Some(Decoded::Binary(data)));
                    }
                }
            }
        }
    }

    /// Appends a fragment, checking the message size and (for text) UTF-8 as far as it is complete.
    fn append(&mut self, payload: &[u8]) -> Result<(), u16> {
        if self.frag.len() + payload.len() > self.max_message {
            return Err(CLOSE_TOO_BIG);
        }
        self.frag.extend_from_slice(payload);
        if self.frag_op == OP_TEXT && !self.frag_compressed {
            match std::str::from_utf8(&self.frag[self.utf8_checked..]) {
                Ok(_) => self.utf8_checked = self.frag.len(),
                // 途中で切れている（続きの断片で完結する）なら、正しいところまで進める
                Err(e) if e.error_len().is_none() => self.utf8_checked += e.valid_up_to(),
                Err(_) => return Err(CLOSE_INVALID_DATA),
            }
        }
        Ok(())
    }
}

fn close_payload(p: Bytes) -> Result<Decoded, u16> {
    match p.len() {
        0 => Ok(Decoded::Close(None)),
        1 => Err(CLOSE_PROTOCOL),
        _ => {
            let code = u16::from_be_bytes([p[0], p[1]]);
            if !valid_close_code(code) {
                return Err(CLOSE_PROTOCOL);
            }
            let reason = p.slice(2..);
            if std::str::from_utf8(&reason).is_err() {
                return Err(CLOSE_INVALID_DATA);
            }
            Ok(Decoded::Close(Some((code, reason))))
        }
    }
}

/// `Sec-WebSocket-Accept` for a `Sec-WebSocket-Key` (base64 of SHA-1(key + GUID), RFC 6455 section 4.2.2).
pub(crate) fn accept_key(key: &[u8]) -> String {
    const GUID: &[u8] = b"258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
    let digest = crate::tls_provider::sha1_legacy(&[key, GUID]);
    base64_encode(&digest)
}

/// Standard base64 with padding.
pub(crate) fn base64_encode(data: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16
            | (*c.get(1).unwrap_or(&0) as u32) << 8
            | *c.get(2).unwrap_or(&0) as u32;
        out.push(T[(n >> 18) as usize & 63] as char);
        out.push(T[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 {
            T[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if c.len() > 2 {
            T[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

/// Length of the data a standard base64 string decodes to, if it is well formed (used to check the 16-byte key).
pub(crate) fn base64_decoded_len(s: &[u8]) -> Option<usize> {
    if s.is_empty() || !s.len().is_multiple_of(4) {
        return None;
    }
    let pad = s.iter().rev().take_while(|&&b| b == b'=').count();
    if pad > 2 {
        return None;
    }
    let body = &s[..s.len() - pad];
    if !body
        .iter()
        .all(|&b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return None;
    }
    Some(s.len() / 4 * 3 - pad)
}

#[cfg(test)]
mod tests {
    use super::*;

    // クライアントのフレーム（マスク付き）を作る。
    fn client_frame(fin: bool, op: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x37, 0xfa, 0x21, 0x3d];
        let mut h = [0u8; 10];
        let n = encode_header(&mut h, fin, op, payload.len());
        let mut out = h[..n].to_vec();
        out[1] |= 0x80;
        out.extend_from_slice(&mask);
        let mut p = payload.to_vec();
        unmask(&mut p, mask);
        out.extend_from_slice(&p);
        out
    }

    fn dec() -> Decoder {
        Decoder::new(1 << 20, 1 << 20, true)
    }

    #[test]
    fn rfc_accept_key_example() {
        // RFC 6455 §1.3
        assert_eq!(
            accept_key(b"dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
        assert_eq!(base64_decoded_len(b"dGhlIHNhbXBsZSBub25jZQ=="), Some(16));
        assert_eq!(base64_decoded_len(b"abc"), None);
        assert_eq!(base64_decoded_len(b"a=b="), None);
    }

    #[test]
    fn rfc_masked_hello() {
        // RFC 6455 §5.7: 「Hello」のマスク付き 1 フレーム
        let mut b = BytesMut::from(
            &[
                0x81, 0x85, 0x37, 0xfa, 0x21, 0x3d, 0x7f, 0x9f, 0x4d, 0x51, 0x58,
            ][..],
        );
        assert_eq!(
            dec().decode(&mut b),
            Ok(Some(Decoded::Text(Bytes::from_static(b"Hello"))))
        );
        assert!(b.is_empty());
    }

    #[test]
    fn unmask_matches_bytewise_for_all_lengths() {
        let mask = [1, 2, 3, 4];
        for n in 0..40 {
            let src: Vec<u8> = (0..n as u8).collect();
            let mut a = src.clone();
            unmask(&mut a, mask);
            let b: Vec<u8> = src
                .iter()
                .enumerate()
                .map(|(i, x)| x ^ mask[i % 4])
                .collect();
            assert_eq!(a, b, "len {}", n);
        }
    }

    #[test]
    fn partial_frames_wait_for_more() {
        let f = client_frame(true, OP_BINARY, &[7u8; 300]);
        let mut d = dec();
        for cut in [1, 2, 3, 5, 8, 100, f.len() - 1] {
            let mut b = BytesMut::from(&f[..cut]);
            assert_eq!(d.decode(&mut b), Ok(None), "cut {}", cut);
        }
        let mut b = BytesMut::from(&f[..]);
        assert_eq!(
            d.decode(&mut b),
            Ok(Some(Decoded::Binary(Bytes::from(vec![7u8; 300]))))
        );
    }

    #[test]
    fn fragmentation_with_interleaved_control() {
        let mut v = client_frame(false, OP_TEXT, b"Hel");
        v.extend(client_frame(true, OP_PING, b"p"));
        v.extend(client_frame(true, OP_CONT, b"lo"));
        let mut b = BytesMut::from(&v[..]);
        let mut d = dec();
        assert_eq!(
            d.decode(&mut b),
            Ok(Some(Decoded::Ping(Bytes::from_static(b"p"))))
        );
        assert_eq!(
            d.decode(&mut b),
            Ok(Some(Decoded::Text(Bytes::from_static(b"Hello"))))
        );
    }

    #[test]
    fn utf8_split_across_fragments() {
        let s = "κόσμε".as_bytes();
        let mut v = client_frame(false, OP_TEXT, &s[..3]);
        v.extend(client_frame(false, OP_CONT, &s[3..7]));
        v.extend(client_frame(true, OP_CONT, &s[7..]));
        let mut b = BytesMut::from(&v[..]);
        assert_eq!(
            dec().decode(&mut b),
            Ok(Some(Decoded::Text(Bytes::copy_from_slice(s))))
        );
        // 不正なバイトは最初の断片で 1007（終わりを待たない）
        let mut b = BytesMut::from(&client_frame(false, OP_TEXT, &[0xce, 0xba, 0xff])[..]);
        assert_eq!(dec().decode(&mut b), Err(CLOSE_INVALID_DATA));
        // 終わりで途中のまま
        let mut v = client_frame(false, OP_TEXT, &s[..1]);
        v.extend(client_frame(true, OP_CONT, b""));
        let mut b = BytesMut::from(&v[..]);
        assert_eq!(dec().decode(&mut b), Err(CLOSE_INVALID_DATA));
        let mut b = BytesMut::from(&client_frame(true, OP_TEXT, &[0xed, 0xa0, 0x80])[..]);
        assert_eq!(dec().decode(&mut b), Err(CLOSE_INVALID_DATA));
    }

    #[test]
    fn protocol_errors() {
        let cases: Vec<(Vec<u8>, u16)> = vec![
            // 予約ビット
            (
                {
                    let mut f = client_frame(true, OP_TEXT, b"x");
                    f[0] |= 0x40;
                    f
                },
                CLOSE_PROTOCOL,
            ),
            // 未知の opcode
            (client_frame(true, 0x3, b"x"), CLOSE_PROTOCOL),
            (client_frame(true, 0xB, b""), CLOSE_PROTOCOL),
            // 断片化した制御フレーム・125 バイト超の制御フレーム
            (client_frame(false, OP_PING, b"x"), CLOSE_PROTOCOL),
            (client_frame(true, OP_PING, &[0u8; 126]), CLOSE_PROTOCOL),
            // 組み立て中でないのに継続・組み立て中に新しいデータ
            (client_frame(true, OP_CONT, b"x"), CLOSE_PROTOCOL),
            (
                {
                    let mut f = client_frame(false, OP_TEXT, b"a");
                    f.extend(client_frame(true, OP_TEXT, b"b"));
                    f
                },
                CLOSE_PROTOCOL,
            ),
            // マスク無し
            (vec![0x81, 0x01, b'a'], CLOSE_PROTOCOL),
            // Close: 1 バイト・不正なコード・不正な理由
            (client_frame(true, OP_CLOSE, &[3]), CLOSE_PROTOCOL),
            (
                client_frame(true, OP_CLOSE, &1005u16.to_be_bytes()),
                CLOSE_PROTOCOL,
            ),
            (
                client_frame(true, OP_CLOSE, &999u16.to_be_bytes()),
                CLOSE_PROTOCOL,
            ),
            (
                client_frame(true, OP_CLOSE, &[0x03, 0xe8, 0xff]),
                CLOSE_INVALID_DATA,
            ),
        ];
        for (i, (f, code)) in cases.into_iter().enumerate() {
            let mut b = BytesMut::from(&f[..]);
            assert_eq!(dec().decode(&mut b), Err(code), "case {}", i);
        }
        let mut b = BytesMut::from(&client_frame(true, OP_CLOSE, &[0x03, 0xe8, b'o', b'k'])[..]);
        assert_eq!(
            dec().decode(&mut b),
            Ok(Some(Decoded::Close(Some((
                1000,
                Bytes::from_static(b"ok")
            )))))
        );
    }

    #[test]
    fn size_limits() {
        let mut d = Decoder::new(100, 150, true);
        let mut b = BytesMut::from(&client_frame(true, OP_BINARY, &[0u8; 101])[..]);
        assert_eq!(d.decode(&mut b), Err(CLOSE_TOO_BIG));
        let mut d = Decoder::new(100, 150, true);
        let mut v = client_frame(false, OP_BINARY, &[0u8; 100]);
        v.extend(client_frame(true, OP_CONT, &[0u8; 51]));
        let mut b = BytesMut::from(&v[..]);
        assert_eq!(d.decode(&mut b), Err(CLOSE_TOO_BIG));
        // 64 ビットの長さの最上位ビット
        let mut f = vec![0x82, 0xff];
        f.extend_from_slice(&(1u64 << 63).to_be_bytes());
        f.extend_from_slice(&[0; 4]);
        assert_eq!(
            parse_header(&f, usize::MAX, true, false),
            Err(CLOSE_PROTOCOL)
        );
    }

    #[test]
    fn header_encoding() {
        let mut h = [0u8; 10];
        assert_eq!(encode_header(&mut h, true, OP_TEXT, 5), 2);
        assert_eq!(&h[..2], &[0x81, 5]);
        assert_eq!(encode_header(&mut h, true, OP_BINARY, 256), 4);
        assert_eq!(&h[..4], &[0x82, 126, 1, 0]);
        assert_eq!(encode_header(&mut h, false, OP_BINARY, 65536), 10);
        assert_eq!(&h[..10], &[0x02, 127, 0, 0, 0, 0, 0, 1, 0, 0]);
        // 復号できる（マスク無しを許す復号器で）
        let n = encode_header(&mut h, true, OP_BINARY, 256);
        let mut d = Decoder::new(1 << 20, 1 << 20, false);
        let mut b = BytesMut::from(&h[..n]);
        b.extend_from_slice(&[9u8; 256]);
        assert_eq!(
            d.decode(&mut b),
            Ok(Some(Decoded::Binary(Bytes::from(vec![9u8; 256]))))
        );
    }
}
