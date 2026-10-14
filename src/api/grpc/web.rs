//! gRPC-Web をハンドラで直接受ける（F-209 #4）。ブラウザ（grpc-web・Connect 等）から Envoy 等の変換無しで呼べる。
//!
//! - `application/grpc-web`・`application/grpc-web+proto` 等: 本文は gRPC と同じ 5 バイトの枠。トレーラは HTTP の
//!   トレーラではなく本文の最後の枠（先頭バイト 0x80、中身は `name: value\r\n` の並び）で返す。
//! - `application/grpc-web-text`（`+proto`）: 上の本文全体を base64 にしたもの（要求も応答も）。断片の境目は
//!   base64 の 4 文字 / 3 バイトの区切りと合わないので、余りを持ち越して途中に `=` を出さない。
//! - HTTP/1.1 でも受ける（ブラウザは HTTP/1.1 で来ることがある）。
//! - CORS: `[api] grpc_web_cors_origins` に許す Origin を並べる（`"*"` で全部）。空なら CORS のヘッダを付けず、
//!   プリフライト（`OPTIONS`）は 403。
//!
//! プロキシの gRPC-Web 変換（`src/grpc/web.rs`、`grpc-web` feature）とは独立（`grpc-handler` だけで使える）。

use super::Metadata;
use bytes::{BufMut, Bytes, BytesMut};
use std::sync::Arc;

/// gRPC-Web の要求の形。
#[derive(Clone, Debug)]
pub(crate) struct WebMode {
    /// `-text`（base64）か
    pub text: bool,
    /// 応答の `content-type`（要求と同じもの）
    pub content_type: Bytes,
    /// 応答に付ける CORS のヘッダ（Origin が許されているときだけ）
    pub cors: Metadata,
}

const PREFIX: &[u8] = b"application/grpc-web";

/// `content-type` が gRPC-Web なら `Some`（`-text` は base64）。
pub(crate) fn web_mode(content_type: &[u8]) -> Option<(bool, Bytes)> {
    if content_type.len() < PREFIX.len()
        || !content_type[..PREFIX.len()].eq_ignore_ascii_case(PREFIX)
    {
        return None;
    }
    let rest = &content_type[PREFIX.len()..];
    let text = rest.len() >= 5 && rest[..5].eq_ignore_ascii_case(b"-text");
    let after = if text { &rest[5..] } else { rest };
    // 残りは空か `+proto` 等の副型（`;` 以降の引数も許す）
    if !(after.is_empty() || after[0] == b'+' || after[0] == b';') {
        return None;
    }
    Some((text, Bytes::copy_from_slice(content_type)))
}

/// 本文の最後に置くトレーラの枠。
pub(crate) fn trailer_frame(trailers: &Metadata) -> Bytes {
    let len: usize = trailers.iter().map(|(n, v)| n.len() + v.len() + 4).sum();
    let mut b = BytesMut::with_capacity(5 + len);
    b.put_u8(0x80);
    b.put_u32(len as u32);
    for (n, v) in trailers {
        b.extend_from_slice(n);
        b.extend_from_slice(b": ");
        b.extend_from_slice(v);
        b.extend_from_slice(b"\r\n");
    }
    b.freeze()
}

// ===================== base64（断片をまたぐ） =====================

const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

fn sextet(c: u8) -> Option<u8> {
    match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' | b'-' => Some(62),
        b'/' | b'_' => Some(63),
        _ => None,
    }
}

/// 要求本文（`-text`）の base64 を断片ごとに戻す。4 文字に満たない余りは次の断片へ持ち越す。
/// 途中の `=`（独立に base64 した断片を連結したもの）も受ける。
#[derive(Default)]
pub(crate) struct Base64Decoder {
    quad: [u8; 4],
    n: usize,
}

impl Base64Decoder {
    pub(crate) fn push(&mut self, input: &[u8], out: &mut BytesMut) -> Result<(), ()> {
        for &c in input {
            if c.is_ascii_whitespace() {
                continue;
            }
            self.quad[self.n] = c;
            self.n += 1;
            if self.n == 4 {
                self.n = 0;
                let q = self.quad;
                let a = sextet(q[0]).ok_or(())?;
                let b = sextet(q[1]).ok_or(())?;
                out.put_u8((a << 2) | (b >> 4));
                if q[2] == b'=' {
                    if q[3] != b'=' {
                        return Err(());
                    }
                    continue;
                }
                let c2 = sextet(q[2]).ok_or(())?;
                out.put_u8((b << 4) | (c2 >> 2));
                if q[3] == b'=' {
                    continue;
                }
                let d = sextet(q[3]).ok_or(())?;
                out.put_u8((c2 << 6) | d);
            }
        }
        Ok(())
    }

    /// 本文の終わり。4 文字に満たない余りがあれば壊れている。
    pub(crate) fn finish(&self) -> Result<(), ()> {
        if self.n == 0 {
            Ok(())
        } else {
            Err(())
        }
    }
}

/// 応答本文（`-text`）を断片ごとに base64 にする。3 バイトに満たない余りは次へ持ち越し、最後に `finish` で出す。
#[derive(Default)]
pub(crate) struct Base64Encoder {
    carry: [u8; 2],
    n: usize,
}

impl Base64Encoder {
    fn quad(out: &mut BytesMut, b: &[u8]) {
        let b0 = b[0];
        let b1 = b.get(1).copied().unwrap_or(0);
        let b2 = b.get(2).copied().unwrap_or(0);
        out.put_u8(ALPHABET[(b0 >> 2) as usize]);
        out.put_u8(ALPHABET[(((b0 & 3) << 4) | (b1 >> 4)) as usize]);
        out.put_u8(if b.len() > 1 {
            ALPHABET[(((b1 & 0xf) << 2) | (b2 >> 6)) as usize]
        } else {
            b'='
        });
        out.put_u8(if b.len() > 2 {
            ALPHABET[(b2 & 0x3f) as usize]
        } else {
            b'='
        });
    }

    pub(crate) fn encode(&mut self, input: &[u8]) -> Bytes {
        let total = self.n + input.len();
        let mut out = BytesMut::with_capacity(total / 3 * 4);
        let mut rest = input;
        if self.n > 0 {
            let need = 3 - self.n;
            if rest.len() < need {
                self.carry[self.n..self.n + rest.len()].copy_from_slice(rest);
                self.n += rest.len();
                return Bytes::new();
            }
            let mut first = [0u8; 3];
            first[..self.n].copy_from_slice(&self.carry[..self.n]);
            first[self.n..].copy_from_slice(&rest[..need]);
            Self::quad(&mut out, &first);
            rest = &rest[need..];
            self.n = 0;
        }
        let whole = rest.len() / 3 * 3;
        for chunk in rest[..whole].chunks(3) {
            Self::quad(&mut out, chunk);
        }
        let tail = &rest[whole..];
        self.carry[..tail.len()].copy_from_slice(tail);
        self.n = tail.len();
        out.freeze()
    }

    pub(crate) fn finish(&mut self) -> Bytes {
        if self.n == 0 {
            return Bytes::new();
        }
        let mut out = BytesMut::with_capacity(4);
        let n = self.n;
        Self::quad(&mut out, &self.carry[..n]);
        self.n = 0;
        out.freeze()
    }
}

// ===================== CORS =====================

/// `[api] grpc_web_cors_origins`（空 = CORS 無し）。設定のロード・リロードで差し替える。
static CORS_ORIGINS: arc_swap::ArcSwapOption<Vec<String>> = arc_swap::ArcSwapOption::const_empty();

pub(crate) fn set_cors_origins(origins: Vec<String>) {
    CORS_ORIGINS.store((!origins.is_empty()).then(|| Arc::new(origins)));
}

/// 許された Origin なら、返す `access-control-allow-origin` の値。
fn allowed_origin(origin: &[u8]) -> Option<Bytes> {
    let guard = CORS_ORIGINS.load();
    let origins = guard.as_deref()?;
    if origins.iter().any(|o| o == "*") {
        return Some(Bytes::from_static(b"*"));
    }
    origins
        .iter()
        .any(|o| o.as_bytes() == origin)
        .then(|| Bytes::copy_from_slice(origin))
}

/// 実際の応答に付ける CORS のヘッダ。
pub(crate) fn cors_response_headers(origin: Option<&str>) -> Metadata {
    let Some(allow) = origin.and_then(|o| allowed_origin(o.as_bytes())) else {
        return Vec::new();
    };
    vec![
        (Bytes::from_static(b"access-control-allow-origin"), allow),
        (
            Bytes::from_static(b"access-control-expose-headers"),
            Bytes::from_static(b"grpc-status, grpc-message, grpc-encoding"),
        ),
        (Bytes::from_static(b"vary"), Bytes::from_static(b"origin")),
    ]
}

/// プリフライト（`OPTIONS`）への応答。
pub(crate) fn preflight(req: &crate::api::request::Request) -> crate::api::response::Response {
    let allow = req
        .header("origin")
        .and_then(|o| allowed_origin(o.as_bytes()));
    let Some(allow) = allow else {
        return crate::api::response::Response::error(403, "CORS origin not allowed");
    };
    let mut r = crate::api::response::Response::new(204);
    r.headers.extend([
        (Bytes::from_static(b"access-control-allow-origin"), allow),
        (
            Bytes::from_static(b"access-control-allow-methods"),
            Bytes::from_static(b"POST, OPTIONS"),
        ),
        (
            Bytes::from_static(b"access-control-allow-headers"),
            // 要求されたヘッダをそのまま許す（gRPC-Web のクライアントは独自のメタデータを送る）
            req.header("access-control-request-headers")
                .map(|h| Bytes::copy_from_slice(h.as_bytes()))
                .unwrap_or_else(|| {
                    Bytes::from_static(b"content-type, x-grpc-web, x-user-agent, grpc-timeout")
                }),
        ),
        (
            Bytes::from_static(b"access-control-max-age"),
            Bytes::from_static(b"86400"),
        ),
        (Bytes::from_static(b"vary"), Bytes::from_static(b"origin")),
    ]);
    r
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn content_types() {
        assert!(matches!(
            web_mode(b"application/grpc-web"),
            Some((false, _))
        ));
        assert!(matches!(
            web_mode(b"application/grpc-web+proto"),
            Some((false, _))
        ));
        assert!(matches!(
            web_mode(b"application/grpc-web-text"),
            Some((true, _))
        ));
        assert!(matches!(
            web_mode(b"Application/GRPC-Web-Text+proto"),
            Some((true, _))
        ));
        assert!(web_mode(b"application/grpc").is_none());
        assert!(web_mode(b"application/grpc+proto").is_none());
        assert!(web_mode(b"application/grpc-webx").is_none());
    }

    #[test]
    fn base64_roundtrip_across_arbitrary_splits() {
        let data: Vec<u8> = (0..=255u8).cycle().take(1000).collect();
        for split in [1usize, 2, 3, 4, 5, 7, 64, 999] {
            let mut enc = Base64Encoder::default();
            let mut b64 = Vec::new();
            for c in data.chunks(split) {
                b64.extend_from_slice(&enc.encode(c));
            }
            b64.extend_from_slice(&enc.finish());
            assert_eq!(b64.len(), data.len().div_ceil(3) * 4);
            assert!(
                !b64[..b64.len() - 4].contains(&b'='),
                "padding only at the end"
            );
            for dsplit in [1usize, 3, 4, 5, 333] {
                let mut dec = Base64Decoder::default();
                let mut out = BytesMut::new();
                for c in b64.chunks(dsplit) {
                    dec.push(c, &mut out).unwrap();
                }
                dec.finish().unwrap();
                assert_eq!(&out[..], &data[..], "split {} / {}", split, dsplit);
            }
        }
    }

    #[test]
    fn base64_decoder_accepts_concatenated_padded_chunks_and_rejects_garbage() {
        let mut dec = Base64Decoder::default();
        let mut out = BytesMut::new();
        // "a" と "bc" を別々に base64 して連結したもの
        dec.push(b"YQ==YmM=", &mut out).unwrap();
        dec.finish().unwrap();
        assert_eq!(&out[..], b"abc");
        let mut dec = Base64Decoder::default();
        assert!(dec.push(b"YQ=x", &mut BytesMut::new()).is_err());
        let mut dec = Base64Decoder::default();
        dec.push(b"YQ", &mut BytesMut::new()).unwrap();
        assert!(dec.finish().is_err());
    }

    #[test]
    fn trailer_frame_format() {
        let f = trailer_frame(&vec![
            (Bytes::from_static(b"grpc-status"), Bytes::from_static(b"0")),
            (Bytes::from_static(b"x-a"), Bytes::from_static(b"b")),
        ]);
        assert_eq!(f[0], 0x80);
        assert_eq!(&f[5..], b"grpc-status: 0\r\nx-a: b\r\n");
        assert_eq!(
            u32::from_be_bytes(f[1..5].try_into().unwrap()) as usize,
            f.len() - 5
        );
    }
}
