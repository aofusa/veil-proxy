//! X.509 の SAN（URI・DNS・IP）と有効期限の取り出し（DER を直接読む。x509 のクレートに依存しない）。
//!
//! メッシュの mTLS（F-217。SPIFFE の URI SAN・Linkerd の DNS SAN）と、上流の mTLS の `expected_san`（TOML の一般機能）で使う。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum San {
    Uri(String),
    Dns(String),
    Ip(std::net::IpAddr),
}

/// DER の 1 要素を読む（タグ・中身・残り）。長さは 4 バイトまで。
fn der_next(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    let (&l0, mut rest) = rest.split_first()?;
    let len = if l0 & 0x80 == 0 {
        l0 as usize
    } else {
        let n = (l0 & 0x7f) as usize;
        if n == 0 || n > 4 || rest.len() < n {
            return None;
        }
        let mut len = 0usize;
        for &b in &rest[..n] {
            len = (len << 8) | b as usize;
        }
        rest = &rest[n..];
        len
    };
    if rest.len() < len {
        return None;
    }
    Some((tag, &rest[..len], &rest[len..]))
}

/// 証明書の TBSCertificate の要素を順に返す。
fn tbs_fields(cert: &[u8]) -> Option<Vec<(u8, &[u8])>> {
    let (tag, cert_body, _) = der_next(cert)?;
    if tag != 0x30 {
        return None;
    }
    let (tag, tbs, _) = der_next(cert_body)?;
    if tag != 0x30 {
        return None;
    }
    let mut out = Vec::new();
    let mut rest = tbs;
    while !rest.is_empty() {
        let (t, body, r) = der_next(rest)?;
        out.push((t, body));
        rest = r;
    }
    Some(out)
}

/// 証明書の SAN（URI・DNS・IP）を取り出す。解析できなければ空。
pub fn parse_sans(cert: &[u8]) -> Vec<San> {
    let mut out = Vec::new();
    let Some(fields) = tbs_fields(cert) else {
        return out;
    };
    // extensions は [3] EXPLICIT
    let Some((_, ext_wrap)) = fields.iter().find(|(t, _)| *t == 0xa3) else {
        return out;
    };
    let Some((0x30, exts, _)) = der_next(ext_wrap) else {
        return out;
    };
    let mut rest = exts;
    while let Some((0x30, ext, r)) = der_next(rest) {
        rest = r;
        let Some((0x06, oid, after_oid)) = der_next(ext) else {
            continue;
        };
        if oid != [0x55, 0x1d, 0x11] {
            continue;
        }
        // critical（BOOLEAN）を飛ばす
        let mut after = after_oid;
        if let Some((0x01, _, r2)) = der_next(after) {
            after = r2;
        }
        let Some((0x04, octets, _)) = der_next(after) else {
            continue;
        };
        let Some((0x30, names, _)) = der_next(octets) else {
            continue;
        };
        let mut nrest = names;
        while let Some((tag, v, r3)) = der_next(nrest) {
            nrest = r3;
            match tag {
                0x86 => {
                    if let Ok(s) = std::str::from_utf8(v) {
                        out.push(San::Uri(s.to_string()));
                    }
                }
                0x82 => {
                    if let Ok(s) = std::str::from_utf8(v) {
                        out.push(San::Dns(s.to_string()));
                    }
                }
                0x87 => match v.len() {
                    4 => out.push(San::Ip(std::net::IpAddr::from([v[0], v[1], v[2], v[3]]))),
                    16 => {
                        let mut a = [0u8; 16];
                        a.copy_from_slice(v);
                        out.push(San::Ip(std::net::IpAddr::from(a)));
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }
    out
}

/// 証明書の notAfter（UNIX 秒）。
pub fn parse_not_after(cert: &[u8]) -> Option<i64> {
    let fields = tbs_fields(cert)?;
    // version [0] があればずれる。validity は SEQUENCE で 2 つの時刻を持つ最初の要素
    let start = if fields.first().map(|f| f.0) == Some(0xa0) {
        1
    } else {
        0
    };
    let (_, validity) = fields.get(start + 3)?;
    let (_, _nb, rest) = der_next(validity)?;
    let (tag, na, _) = der_next(rest)?;
    let s = std::str::from_utf8(na).ok()?;
    // UTCTime YYMMDDHHMMSSZ / GeneralizedTime YYYYMMDDHHMMSSZ
    let (year, rest) = match tag {
        0x17 => {
            let y: i64 = s.get(0..2)?.parse().ok()?;
            (if y < 50 { 2000 + y } else { 1900 + y }, s.get(2..)?)
        }
        0x18 => (s.get(0..4)?.parse().ok()?, s.get(4..)?),
        _ => return None,
    };
    let p = |a: usize| -> Option<i64> { rest.get(a..a + 2)?.parse().ok() };
    let (mo, d, h, mi, se) = (p(0)?, p(2)?, p(4)?, p(6)?, p(8)?);
    // 日付 → UNIX 日数（civil_from_days の逆）
    let y = if mo <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(days * 86400 + h * 3600 + mi * 60 + se)
}

