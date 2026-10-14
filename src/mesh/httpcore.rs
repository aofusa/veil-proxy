//! メッシュの HTTP で共有する小物（乱数・要求 ID・base64・時刻・ローカル応答）。

use bytes::{BufMut, Bytes, BytesMut};
use std::cell::Cell;

thread_local! {
    static RNG: Cell<u64> = Cell::new(seed());
}

fn seed() -> u64 {
    let t = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9e37_79b9_7f4a_7c15);
    let tid = std::thread::current().id();
    let h = {
        use std::hash::{Hash, Hasher};
        let mut s = std::collections::hash_map::DefaultHasher::new();
        tid.hash(&mut s);
        s.finish()
    };
    (t ^ h) | 1
}

/// スレッドローカルの xorshift64*（暗号用途ではない。LB・割合・要求 ID）。
#[inline]
pub fn rand_u64() -> u64 {
    RNG.with(|c| {
        let mut x = c.get();
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        c.set(x);
        x.wrapping_mul(0x2545_f491_4f6c_dd1d)
    })
}

#[inline]
pub fn rand_u32() -> u32 {
    (rand_u64() >> 32) as u32
}

/// UUID v4 の文字列（`x-request-id`）。
pub fn uuid_v4() -> Bytes {
    let a = rand_u64();
    let b = rand_u64();
    let mut raw = [0u8; 16];
    raw[..8].copy_from_slice(&a.to_le_bytes());
    raw[8..].copy_from_slice(&b.to_le_bytes());
    raw[6] = (raw[6] & 0x0f) | 0x40;
    raw[8] = (raw[8] & 0x3f) | 0x80;
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = BytesMut::with_capacity(36);
    for (i, b) in raw.iter().enumerate() {
        if i == 4 || i == 6 || i == 8 || i == 10 {
            out.put_u8(b'-');
        }
        out.put_u8(HEX[(b >> 4) as usize]);
        out.put_u8(HEX[(b & 15) as usize]);
    }
    out.freeze()
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(input: &[u8]) -> String {
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | b[2] as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            B64[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            B64[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

pub fn base64_decode(input: &[u8]) -> Option<Vec<u8>> {
    fn val(c: u8) -> Option<u32> {
        Some(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        })
    }
    let input: Vec<u8> = input
        .iter()
        .copied()
        .filter(|c| !c.is_ascii_whitespace() && *c != b'=')
        .collect();
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    for chunk in input.chunks(4) {
        let mut n = 0u32;
        for (i, &c) in chunk.iter().enumerate() {
            n |= val(c)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
        if chunk.len() == 1 {
            return None;
        }
    }
    Some(out)
}

/// プロセス起動からの経過ミリ秒（単調）。
pub fn now_ms() -> u64 {
    static START: once_cell::sync::Lazy<std::time::Instant> =
        once_cell::sync::Lazy::new(std::time::Instant::now);
    START.elapsed().as_millis() as u64
}

pub fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// ステータスの理由句（HTTP/1.1 の応答行）。
pub fn reason(status: u16) -> &'static str {
    match status {
        100 => "Continue",
        101 => "Switching Protocols",
        200 => "OK",
        201 => "Created",
        202 => "Accepted",
        204 => "No Content",
        206 => "Partial Content",
        301 => "Moved Permanently",
        302 => "Found",
        303 => "See Other",
        304 => "Not Modified",
        307 => "Temporary Redirect",
        308 => "Permanent Redirect",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        408 => "Request Timeout",
        413 => "Payload Too Large",
        414 => "URI Too Long",
        429 => "Too Many Requests",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        501 => "Not Implemented",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        504 => "Gateway Timeout",
        _ => "Unknown",
    }
}

/// ホップバイホップのヘッダか（転送しない）。
pub fn is_hop_by_hop(name: &[u8]) -> bool {
    name.eq_ignore_ascii_case(b"connection")
        || name.eq_ignore_ascii_case(b"keep-alive")
        || name.eq_ignore_ascii_case(b"proxy-connection")
        || name.eq_ignore_ascii_case(b"transfer-encoding")
        || name.eq_ignore_ascii_case(b"upgrade")
        || name.eq_ignore_ascii_case(b"te")
        || name.eq_ignore_ascii_case(b"trailer")
        || name.eq_ignore_ascii_case(b"proxy-authenticate")
        || name.eq_ignore_ascii_case(b"proxy-authorization")
}

/// Envoy の応答フラグ（`response_flags`。観測のラベル）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum ResponseFlag {
    #[default]
    None,
    /// NR: ルートが無い
    NoRoute,
    /// UH: 健康な上流が無い
    NoHealthyUpstream,
    /// UF: 上流への接続失敗
    UpstreamConnectFailure,
    /// URX: リトライ上限
    UpstreamRetryLimit,
    /// UT: 上流のタイムアウト
    UpstreamTimeout,
    /// UC: 上流が接続を切った
    UpstreamConnectionTermination,
    /// DC: 下流が接続を切った
    DownstreamConnectionTermination,
    /// FI / DI: フォルト注入
    FaultInjected,
    /// UAEX: 認可の拒否（RBAC は RBAC フラグ）
    RbacDenied,
    /// NC: クラスタが無い
    NoCluster,
}

impl ResponseFlag {
    pub fn as_str(self) -> &'static str {
        match self {
            ResponseFlag::None => "-",
            ResponseFlag::NoRoute => "NR",
            ResponseFlag::NoHealthyUpstream => "UH",
            ResponseFlag::UpstreamConnectFailure => "UF",
            ResponseFlag::UpstreamRetryLimit => "URX",
            ResponseFlag::UpstreamTimeout => "UT",
            ResponseFlag::UpstreamConnectionTermination => "UC",
            ResponseFlag::DownstreamConnectionTermination => "DC",
            ResponseFlag::FaultInjected => "FI",
            ResponseFlag::RbacDenied => "RBAC",
            ResponseFlag::NoCluster => "NC",
        }
    }
}

/// Envoy 風のローカル応答の本文（`no healthy upstream` 等）。
pub fn local_reply_body(status: u16, flag: ResponseFlag) -> &'static [u8] {
    match flag {
        ResponseFlag::NoHealthyUpstream => b"no healthy upstream",
        ResponseFlag::UpstreamConnectFailure | ResponseFlag::UpstreamRetryLimit => {
            b"upstream connect error or disconnect/reset before headers"
        }
        ResponseFlag::UpstreamTimeout => b"upstream request timeout",
        ResponseFlag::RbacDenied => b"RBAC: access denied",
        ResponseFlag::FaultInjected => b"fault filter abort",
        ResponseFlag::NoCluster => b"no cluster",
        _ => match status {
            404 => b"",
            _ => b"",
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_roundtrip() {
        for s in [
            &b""[..],
            b"a",
            b"ab",
            b"abc",
            b"hello world",
            &[0u8, 255, 1, 2],
        ] {
            let e = base64_encode(s);
            assert_eq!(base64_decode(e.as_bytes()).unwrap(), s);
        }
        assert_eq!(base64_encode(b"abc"), "YWJj");
    }

    #[test]
    fn uuid_shape() {
        let u = uuid_v4();
        assert_eq!(u.len(), 36);
        assert_eq!(u[14], b'4');
        assert_ne!(uuid_v4(), u);
    }
}
