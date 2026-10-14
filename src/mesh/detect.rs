//! プロトコル検出（F-216。Envoy の `tls_inspector`・`http_inspector`）。
//!
//! 接続の先頭を読んで TLS の ClientHello（SNI・ALPN）・HTTP/2 のプリフェース・HTTP/1.x のリクエスト行を
//! 見分ける。読んだバイトは捨てずに次の段（TLS の終端・HTTP のパーサ・L4 の転送）へそのまま渡す
//! （`MSG_PEEK` で読み直さない）。

use crate::runtime::tcp::TcpStream;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Detected {
    /// TLS（ClientHello の SNI と ALPN）
    Tls {
        sni: Option<String>,
        alpn: Vec<String>,
    },
    Http1,
    H2c,
    /// どれでもない（L4 で転送する）
    Other,
    /// 何も来ないまま閉じた
    Closed,
}

impl Detected {
    pub fn is_tls(&self) -> bool {
        matches!(self, Detected::Tls { .. })
    }
}

const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";
const METHODS: &[&[u8]] = &[
    b"GET ",
    b"POST ",
    b"PUT ",
    b"DELETE ",
    b"HEAD ",
    b"OPTIONS ",
    b"PATCH ",
    b"CONNECT ",
    b"TRACE ",
];

/// 先頭のバイト列を分類する。判定に足りなければ `None`（続きを読む）。
pub fn classify(buf: &[u8]) -> Option<Detected> {
    if buf.is_empty() {
        return None;
    }
    if buf[0] == 0x16 {
        // TLS レコード（handshake）
        return match parse_client_hello(buf) {
            Hello::Done { sni, alpn } => Some(Detected::Tls { sni, alpn }),
            Hello::NeedMore => None,
            Hello::Invalid => Some(Detected::Other),
        };
    }
    if buf[0] == b'P' {
        let n = buf.len().min(H2_PREFACE.len());
        if buf[..n] == H2_PREFACE[..n] {
            return if buf.len() >= H2_PREFACE.len() {
                Some(Detected::H2c)
            } else {
                None
            };
        }
    }
    let mut any_prefix = false;
    for m in METHODS {
        let n = buf.len().min(m.len());
        if buf[..n] == m[..n] {
            if buf.len() >= m.len() {
                // リクエスト行の末尾 " HTTP/1." を確かめる
                if let Some(eol) = memchr::memchr(b'\n', buf) {
                    let line = &buf[..eol];
                    let line = line.strip_suffix(b"\r").unwrap_or(line);
                    if line.len() >= 9 && &line[line.len() - 9..line.len() - 1] == b" HTTP/1." {
                        return Some(Detected::Http1);
                    }
                    return Some(Detected::Other);
                }
                if buf.len() > 8192 {
                    return Some(Detected::Other);
                }
                return None;
            }
            any_prefix = true;
        }
    }
    if any_prefix {
        None
    } else {
        Some(Detected::Other)
    }
}

enum Hello {
    Done {
        sni: Option<String>,
        alpn: Vec<String>,
    },
    NeedMore,
    Invalid,
}

/// TLS の ClientHello から SNI と ALPN を取り出す（複数レコードにまたがる場合は連結する）。
fn parse_client_hello(buf: &[u8]) -> Hello {
    // レコードを連結して handshake メッセージを組み立てる
    let mut hs: Vec<u8> = Vec::new();
    let mut p = 0usize;
    loop {
        if buf.len() < p + 5 {
            return Hello::NeedMore;
        }
        if buf[p] != 0x16 {
            return Hello::Invalid;
        }
        let len = u16::from_be_bytes([buf[p + 3], buf[p + 4]]) as usize;
        if buf.len() < p + 5 + len {
            // 1 つ目のレコードだけで判定できるなら途中でもよい
            if hs.is_empty() && buf.len() > p + 5 {
                let partial = &buf[p + 5..];
                if let Some(h) = try_hello(partial) {
                    return h;
                }
            }
            return Hello::NeedMore;
        }
        hs.extend_from_slice(&buf[p + 5..p + 5 + len]);
        p += 5 + len;
        if let Some(h) = try_hello(&hs) {
            return h;
        }
        if hs.len() > 64 * 1024 {
            return Hello::Invalid;
        }
    }
}

fn try_hello(hs: &[u8]) -> Option<Hello> {
    if hs.len() < 4 {
        return None;
    }
    if hs[0] != 0x01 {
        return Some(Hello::Invalid);
    }
    let len = ((hs[1] as usize) << 16) | ((hs[2] as usize) << 8) | hs[3] as usize;
    if hs.len() < 4 + len {
        return None;
    }
    let b = &hs[4..4 + len];
    let mut p = 2 + 32; // version + random
    let get = |p: usize, n: usize| -> Option<&[u8]> { b.get(p..p + n) };
    let sid = *b.get(p)? as usize;
    p += 1 + sid;
    let cs = u16::from_be_bytes(get(p, 2)?.try_into().ok()?) as usize;
    p += 2 + cs;
    let cm = *b.get(p)? as usize;
    p += 1 + cm;
    let mut sni = None;
    let mut alpn = Vec::new();
    if p + 2 <= b.len() {
        let ext_len = u16::from_be_bytes(get(p, 2)?.try_into().ok()?) as usize;
        p += 2;
        let end = (p + ext_len).min(b.len());
        while p + 4 <= end {
            let et = u16::from_be_bytes([b[p], b[p + 1]]);
            let el = u16::from_be_bytes([b[p + 2], b[p + 3]]) as usize;
            p += 4;
            let ed = b.get(p..p + el)?;
            p += el;
            match et {
                0 => {
                    // server_name: list_len(2) type(1) len(2) name
                    if ed.len() >= 5 && ed[2] == 0 {
                        let nl = u16::from_be_bytes([ed[3], ed[4]]) as usize;
                        if let Some(n) = ed.get(5..5 + nl) {
                            sni = std::str::from_utf8(n).ok().map(|s| s.to_ascii_lowercase());
                        }
                    }
                }
                16 => {
                    // ALPN: list_len(2) (len(1) proto)*
                    let mut q = 2usize;
                    while q < ed.len() {
                        let l = ed[q] as usize;
                        q += 1;
                        if let Some(proto) = ed.get(q..q + l) {
                            if let Ok(s) = std::str::from_utf8(proto) {
                                alpn.push(s.to_string());
                            }
                        }
                        q += l;
                    }
                }
                _ => {}
            }
        }
    }
    Some(Hello::Done { sni, alpn })
}

/// 接続の先頭を読み、分類と読んだバイトを返す。`timeout` は最初のバイトまでの待ち
/// （サーバが先に話すプロトコルは何も来ないので、期限で `Other` にする。None は待ち続ける）。
pub async fn sniff(stream: &TcpStream, timeout: Option<Duration>) -> (Detected, Vec<u8>) {
    let mut acc: Vec<u8> = Vec::new();
    let mut first = true;
    loop {
        let buf: Vec<u8> = Vec::with_capacity(4096);
        let read = stream.read(buf);
        let (res, buf) = match (first, timeout) {
            (true, Some(t)) => match crate::runtime::timeout(t, read).await {
                Ok(r) => r,
                Err(_) => return (Detected::Other, acc),
            },
            // 2 回目以降は idle の上限（相手が途中で止まった）
            _ => match crate::runtime::timeout(Duration::from_secs(30), read).await {
                Ok(r) => r,
                Err(_) => {
                    let d = if acc.is_empty() {
                        Detected::Closed
                    } else {
                        Detected::Other
                    };
                    return (d, acc);
                }
            },
        };
        first = false;
        match res {
            Ok(0) | Err(_) => {
                let d = if acc.is_empty() {
                    Detected::Closed
                } else {
                    classify(&acc).unwrap_or(Detected::Other)
                };
                return (d, acc);
            }
            Ok(n) => {
                acc.extend_from_slice(&buf[..n]);
                if let Some(d) = classify(&acc) {
                    return (d, acc);
                }
                if acc.len() > 64 * 1024 {
                    return (Detected::Other, acc);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn client_hello(sni: &str, alpn: &[&str]) -> Vec<u8> {
        let mut ext = Vec::new();
        // SNI
        let mut sn = vec![0u8];
        sn.extend_from_slice(&(sni.len() as u16).to_be_bytes());
        sn.extend_from_slice(sni.as_bytes());
        let mut sl = (sn.len() as u16).to_be_bytes().to_vec();
        sl.extend_from_slice(&sn);
        ext.extend_from_slice(&0u16.to_be_bytes());
        ext.extend_from_slice(&(sl.len() as u16).to_be_bytes());
        ext.extend_from_slice(&sl);
        // ALPN
        let mut al = Vec::new();
        for a in alpn {
            al.push(a.len() as u8);
            al.extend_from_slice(a.as_bytes());
        }
        let mut alv = (al.len() as u16).to_be_bytes().to_vec();
        alv.extend_from_slice(&al);
        ext.extend_from_slice(&16u16.to_be_bytes());
        ext.extend_from_slice(&(alv.len() as u16).to_be_bytes());
        ext.extend_from_slice(&alv);
        let mut body = vec![3, 3];
        body.extend_from_slice(&[0u8; 32]);
        body.push(0); // session id
        body.extend_from_slice(&2u16.to_be_bytes());
        body.extend_from_slice(&[0x13, 0x01]);
        body.extend_from_slice(&[1, 0]);
        body.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        body.extend_from_slice(&ext);
        let mut hs = vec![1u8];
        hs.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
        hs.extend_from_slice(&body);
        let mut rec = vec![0x16, 3, 1];
        rec.extend_from_slice(&(hs.len() as u16).to_be_bytes());
        rec.extend_from_slice(&hs);
        rec
    }

    #[test]
    fn detects_tls_with_sni_and_alpn() {
        let ch = client_hello(
            "outbound_.80_._.whoami.demo.svc.cluster.local",
            &["istio-http/1.1", "istio", "http/1.1"],
        );
        let d = classify(&ch).unwrap();
        assert_eq!(
            d,
            Detected::Tls {
                sni: Some("outbound_.80_._.whoami.demo.svc.cluster.local".into()),
                alpn: vec!["istio-http/1.1".into(), "istio".into(), "http/1.1".into()]
            }
        );
        // 途中で切れていれば続きを待つ
        assert!(classify(&ch[..20]).is_none());
        // 2 つのレコードに分かれていても読める
        let hs = &ch[5..];
        let (a, b) = hs.split_at(30);
        let mut two = vec![0x16, 3, 1];
        two.extend_from_slice(&(a.len() as u16).to_be_bytes());
        two.extend_from_slice(a);
        two.extend_from_slice(&[0x16, 3, 1]);
        two.extend_from_slice(&(b.len() as u16).to_be_bytes());
        two.extend_from_slice(b);
        assert!(classify(&two).unwrap().is_tls());
    }

    #[test]
    fn detects_http() {
        assert_eq!(
            classify(b"GET / HTTP/1.1\r\nHost: a\r\n\r\n"),
            Some(Detected::Http1)
        );
        assert_eq!(classify(b"GE"), None);
        assert_eq!(classify(b"GET /"), None);
        assert_eq!(classify(H2_PREFACE), Some(Detected::H2c));
        assert_eq!(classify(b"PRI * HTT"), None);
        assert_eq!(classify(b"\x00\x01binary"), Some(Detected::Other));
        assert_eq!(classify(b"GET foo bar\r\n"), Some(Detected::Other));
    }
}
