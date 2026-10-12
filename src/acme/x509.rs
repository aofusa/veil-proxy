//! 証明書の有効期限（notAfter）を読む最小の DER 解析（更新時期の判定用）。

/// DER の TLV を 1 個読む（タグ, 中身, 残り）。
fn tlv(data: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let tag = *data.first()?;
    let first = *data.get(1)?;
    let (len, hdr) = if first < 0x80 {
        (first as usize, 2)
    } else {
        let n = (first & 0x7f) as usize;
        if n == 0 || n > 4 {
            return None;
        }
        let mut len = 0usize;
        for i in 0..n {
            len = (len << 8) | *data.get(2 + i)? as usize;
        }
        (len, 2 + n)
    };
    let body = data.get(hdr..hdr + len)?;
    Some((tag, body, &data[hdr + len..]))
}

/// 証明書（DER）の notAfter（UNIX 秒）。
pub fn not_after(der: &[u8]) -> Option<i64> {
    let (_, cert, _) = tlv(der)?; // Certificate SEQUENCE
    let (_, tbs, _) = tlv(cert)?; // tbsCertificate SEQUENCE
    let mut rest = tbs;
    // [0] version（任意）
    if rest.first() == Some(&0xa0) {
        rest = tlv(rest)?.2;
    }
    rest = tlv(rest)?.2; // serialNumber
    rest = tlv(rest)?.2; // signature AlgorithmIdentifier
    rest = tlv(rest)?.2; // issuer
    let (_, validity, _) = tlv(rest)?;
    let (_, _, after) = tlv(validity)?; // notBefore
    let (tag, t, _) = tlv(after)?; // notAfter
    let s = std::str::from_utf8(t).ok()?;
    let (year, rest) = match tag {
        0x17 => {
            // UTCTime YYMMDDHHMMSSZ
            let yy: i64 = s.get(0..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, s.get(2..)?)
        }
        0x18 => (s.get(0..4)?.parse().ok()?, s.get(4..)?), // GeneralizedTime
        _ => return None,
    };
    let num = |r: std::ops::Range<usize>| -> Option<i64> { rest.get(r)?.parse().ok() };
    let (mo, d, h, mi, se) = (num(0..2)?, num(2..4)?, num(4..6)?, num(6..8)?, num(8..10)?);
    Some(days_from_civil(year, mo, d) * 86400 + h * 3600 + mi * 60 + se)
}

/// 1970-01-01 からの日数（Howard Hinnant の days_from_civil）。
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

/// PEM（複数可）の先頭の証明書の notAfter。
pub fn pem_not_after(pem: &[u8]) -> Option<i64> {
    use rustls::pki_types::pem::PemObject;
    let cert = rustls::pki_types::CertificateDer::pem_slice_iter(pem)
        .next()?
        .ok()?;
    not_after(cert.as_ref())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_days() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2026, 10, 12), 20738);
    }

    #[test]
    fn reads_not_after_from_rcgen_certificate() {
        let mut params = rcgen::CertificateParams::new(vec!["a.example".into()]).unwrap();
        params.not_after = rcgen::date_time_ymd(2031, 5, 7);
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        let na = not_after(cert.der()).unwrap();
        assert_eq!(na, days_from_civil(2031, 5, 7) * 86400);
        assert_eq!(pem_not_after(cert.pem().as_bytes()), Some(na));
    }
}
