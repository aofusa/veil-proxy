//! 組み込みのサービス（`grpc.health.v1` 等）のための最小の protobuf の符号化（F-208）。
//!
//! veil は protobuf の実装に依存しないので、組み込みのサービスの小さなメッセージ（文字列・列挙・入れ子のメッセージ）は
//! ここで手書きする。未知のフィールドは読み飛ばす（proto3 の前方互換）。

use super::Status;
use bytes::{BufMut, BytesMut};

/// ワイヤ型。
pub(crate) const VARINT: u8 = 0;
pub(crate) const I64: u8 = 1;
pub(crate) const LEN: u8 = 2;
pub(crate) const I32: u8 = 5;

pub(crate) fn put_varint(out: &mut BytesMut, mut v: u64) {
    while v >= 0x80 {
        out.put_u8((v as u8) | 0x80);
        v >>= 7;
    }
    out.put_u8(v as u8);
}

pub(crate) fn varint_len(mut v: u64) -> usize {
    let mut n = 1;
    while v >= 0x80 {
        v >>= 7;
        n += 1;
    }
    n
}

fn put_key(out: &mut BytesMut, field: u32, wire: u8) {
    put_varint(out, ((field as u64) << 3) | wire as u64);
}

/// 長さ付きのフィールド（文字列・バイト列・入れ子のメッセージ）。proto3 の既定値（空）は書かない。
pub(crate) fn put_bytes_field(out: &mut BytesMut, field: u32, v: &[u8]) {
    if v.is_empty() {
        return;
    }
    put_key(out, field, LEN);
    put_varint(out, v.len() as u64);
    out.put_slice(v);
}

/// 入れ子のメッセージ（空でも書く。oneof の選択を表すため）。
pub(crate) fn put_message_field(out: &mut BytesMut, field: u32, v: &[u8]) {
    put_key(out, field, LEN);
    put_varint(out, v.len() as u64);
    out.put_slice(v);
}

/// repeated int32（proto3 の既定の packed 形式）。空なら書かない。
pub(crate) fn put_packed_int32_field(out: &mut BytesMut, field: u32, v: &[i32]) {
    if v.is_empty() {
        return;
    }
    let len: usize = v.iter().map(|x| varint_len(*x as i64 as u64)).sum();
    put_key(out, field, LEN);
    put_varint(out, len as u64);
    for x in v {
        put_varint(out, *x as i64 as u64);
    }
}

/// int32・列挙（負の値は 10 バイトの varint）。proto3 の既定値（0）は書かない。
pub(crate) fn put_int32_field(out: &mut BytesMut, field: u32, v: i32) {
    if v == 0 {
        return;
    }
    put_key(out, field, VARINT);
    put_varint(out, v as i64 as u64);
}

pub(crate) fn bytes_field_len(field: u32, len: usize) -> usize {
    if len == 0 {
        0
    } else {
        varint_len((field as u64) << 3) + varint_len(len as u64) + len
    }
}

/// メッセージを先頭から読むカーソル。
pub(crate) struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

fn malformed() -> Status {
    Status::internal("failed to decode message: malformed protobuf")
}

impl<'a> Reader<'a> {
    pub(crate) fn new(buf: &'a [u8]) -> Reader<'a> {
        Reader { buf, pos: 0 }
    }

    fn varint(&mut self) -> Result<u64, Status> {
        let mut v: u64 = 0;
        for i in 0..10 {
            let b = *self.buf.get(self.pos).ok_or_else(malformed)?;
            self.pos += 1;
            v |= ((b & 0x7f) as u64) << (7 * i);
            if b < 0x80 {
                return Ok(v);
            }
        }
        Err(malformed())
    }

    /// 次のフィールドの (番号, ワイヤ型)。終わりなら `None`。
    pub(crate) fn next_key(&mut self) -> Result<Option<(u32, u8)>, Status> {
        if self.pos >= self.buf.len() {
            return Ok(None);
        }
        let k = self.varint()?;
        let field = (k >> 3) as u32;
        if field == 0 {
            return Err(malformed());
        }
        Ok(Some((field, (k & 7) as u8)))
    }

    pub(crate) fn read_varint(&mut self) -> Result<u64, Status> {
        self.varint()
    }

    pub(crate) fn read_bytes(&mut self) -> Result<&'a [u8], Status> {
        let n = self.varint()? as usize;
        let end = self.pos.checked_add(n).ok_or_else(malformed)?;
        let v = self.buf.get(self.pos..end).ok_or_else(malformed)?;
        self.pos = end;
        Ok(v)
    }

    pub(crate) fn read_string(&mut self) -> Result<String, Status> {
        let b = self.read_bytes()?;
        String::from_utf8(b.to_vec())
            .map_err(|_| Status::internal("failed to decode message: invalid UTF-8 in string"))
    }

    /// 知らないフィールドを読み飛ばす。
    pub(crate) fn skip(&mut self, wire: u8) -> Result<(), Status> {
        let n = match wire {
            VARINT => {
                self.varint()?;
                return Ok(());
            }
            I64 => 8,
            LEN => {
                self.read_bytes()?;
                return Ok(());
            }
            I32 => 4,
            _ => return Err(malformed()),
        };
        let end = self.pos.checked_add(n).ok_or_else(malformed)?;
        if end > self.buf.len() {
            return Err(malformed());
        }
        self.pos = end;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip_and_len() {
        for v in [0u64, 1, 127, 128, 300, 16_384, u32::MAX as u64, u64::MAX] {
            let mut b = BytesMut::new();
            put_varint(&mut b, v);
            assert_eq!(b.len(), varint_len(v));
            assert_eq!(Reader::new(&b).read_varint().unwrap(), v);
        }
    }

    #[test]
    fn skips_unknown_fields_and_rejects_truncated() {
        let mut b = BytesMut::new();
        put_int32_field(&mut b, 7, 5);
        b.put_u8((8 << 3) | I64);
        b.put_u64_le(1);
        b.put_u8((9 << 3) | I32);
        b.put_u32_le(1);
        put_bytes_field(&mut b, 1, b"svc");
        let mut r = Reader::new(&b);
        let mut got = None;
        while let Some((f, w)) = r.next_key().unwrap() {
            if f == 1 && w == LEN {
                got = Some(r.read_string().unwrap());
            } else {
                r.skip(w).unwrap();
            }
        }
        assert_eq!(got.as_deref(), Some("svc"));
        // 長さが本文を超える
        let mut r = Reader::new(&[(1 << 3) | LEN, 5, b'a']);
        r.next_key().unwrap();
        assert!(r.read_bytes().is_err());
        // 終わらない varint
        assert!(Reader::new(&[0xff; 11]).read_varint().is_err());
    }
}
