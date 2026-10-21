// F-209 #4
//! gRPC-Web served directly by handlers, so browsers (grpc-web, Connect and so on) can call without a translator such as Envoy.
//!
//! - `application/grpc-web`, `application/grpc-web+proto` and similar: the body uses the same 5-byte framing as
//!   gRPC. Trailers are not HTTP trailers; they are returned as the last frame of the body (first byte 0x80,
//!   content a sequence of `name: value\r\n`).
//! - `application/grpc-web-text` (`+proto`): the whole body above encoded as base64 (both request and response).
//!   Chunk boundaries do not line up with base64's 4-characters / 3-bytes groups, so the remainder is carried
//!   over and no `=` is emitted in the middle.
//! - HTTP/1.1 is also accepted (browsers may arrive over HTTP/1.1).
//! - CORS: list the allowed origins in `[api] grpc_web_cors_origins` (`"*"` allows all). When empty, no CORS
//!   headers are added and a preflight (`OPTIONS`) gets 403.
//!
//! This is independent of the proxy's gRPC-Web translation (`src/grpc/web.rs`, the `grpc-web` feature) and works with `grpc-handler` alone.

use super::Metadata;
use bytes::{BufMut, Bytes, BytesMut};
use std::sync::Arc;

/// The shape of a gRPC-Web request.
#[derive(Clone, Debug)]
pub(crate) struct WebMode {
    /// Whether it is `-text` (base64).
    pub text: bool,
    /// The `content-type` of the response (the same as the request's).
    pub content_type: Bytes,
    /// CORS headers for the response (only when the origin is allowed).
    pub cors: Metadata,
}

const PREFIX: &[u8] = b"application/grpc-web";

/// `Some` if the `content-type` is gRPC-Web (`-text` means base64).
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

/// The trailer frame placed at the end of the body.
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

/// Decodes the base64 of a request body (`-text`) chunk by chunk. A remainder shorter than 4 characters is
/// carried over to the next chunk. A `=` in the middle (chunks base64-encoded independently and concatenated) is accepted too.
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

    /// The end of the body. A remainder shorter than 4 characters means the body is corrupt.
    pub(crate) fn finish(&self) -> Result<(), ()> {
        if self.n == 0 {
            Ok(())
        } else {
            Err(())
        }
    }
}

/// Base64-encodes a response body (`-text`) chunk by chunk. A remainder shorter than 3 bytes is carried over and emitted by `finish` at the end.
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

/// `[api] grpc_web_cors_origins` (empty means no CORS). Replaced when the configuration is loaded or reloaded.
static CORS_ORIGINS: arc_swap::ArcSwapOption<Vec<String>> = arc_swap::ArcSwapOption::const_empty();

pub(crate) fn set_cors_origins(origins: Vec<String>) {
    CORS_ORIGINS.store((!origins.is_empty()).then(|| Arc::new(origins)));
}

/// The `access-control-allow-origin` value to return if the origin is allowed.
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

/// CORS headers for the actual response.
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

/// The response to a preflight (`OPTIONS`) request.
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
