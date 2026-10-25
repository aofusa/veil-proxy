//! `[route.traffic.cors]` を実際の応答（プリフライト以外）にも効かせる（F-273）。
//!
//! - 要求の `Origin` の照合はルートの解決（`route_traffic::TrafficPolicy::select`）で 1 回だけ行い、照合の結果ごとに
//!   設定ロード時に作った `Backend` の写し（`SecurityConfig::cors_actual` に [`CorsActual`] を持つ）を選ぶ。
//!   応答を書く各経路は `security.cors_actual` を見て、ロード時に組み立てたヘッダを足すだけ（`Bytes` の参照カウント）。
//! - `[route.traffic.cors]` の無いルートの `cors_actual` は `None` で、各経路は分岐 1 個だけ増える。
//! - 上流（プロキシ・ハンドラ・固定応答の設定）が既に `Access-Control-Allow-Origin` を返したら足さない。
//!   `override_upstream = true` なら上流の CORS のヘッダを除いて足す。

use crate::config::SecurityConfig;
use bytes::Bytes;

/// 実際の応答に足す CORS のヘッダ（ロード時に組み立てて共有する）。
#[derive(Debug)]
pub struct CorsActual {
    /// HTTP/2・HTTP/3 のヘッダ（小文字の名前）
    fields: Box<[(Bytes, Bytes)]>,
    /// HTTP/1.1 の行（`Name: value\r\n` の連結）
    h1_lines: Box<[u8]>,
    /// 上流の CORS のヘッダを除いて足す
    override_upstream: bool,
}

/// 上書きのときに上流の応答から除くヘッダ（小文字）
const CORS_NAMES: [&[u8]; 3] = [
    b"access-control-allow-origin",
    b"access-control-allow-credentials",
    b"access-control-expose-headers",
];

#[inline]
fn is_cors_name(name: &[u8]) -> bool {
    CORS_NAMES.iter().any(|n| name.eq_ignore_ascii_case(n))
}

/// HTTP/1.1 の行に書く名前（小文字の名前を語の先頭だけ大文字にする）
fn h1_name(lower: &[u8]) -> Vec<u8> {
    let mut up = true;
    lower
        .iter()
        .map(|&b| {
            let c = if up { b.to_ascii_uppercase() } else { b };
            up = b == b'-';
            c
        })
        .collect()
}

impl CorsActual {
    /// `fields` は小文字の名前（設定ロード時に 1 回）
    pub fn new(fields: Vec<(Bytes, Bytes)>, override_upstream: bool) -> Self {
        let mut h1 = Vec::new();
        for (k, v) in &fields {
            h1.extend_from_slice(&h1_name(k));
            h1.extend_from_slice(b": ");
            h1.extend_from_slice(v);
            h1.extend_from_slice(b"\r\n");
        }
        CorsActual {
            fields: fields.into_boxed_slice(),
            h1_lines: h1.into_boxed_slice(),
            override_upstream,
        }
    }

    /// 足すヘッダ（小文字の名前）
    #[inline]
    pub fn fields(&self) -> &[(Bytes, Bytes)] {
        &self.fields
    }

    /// HTTP/1.1 の応答の head（ステータス行〜空行。後ろに本文が続いてもよい）へ足す。
    /// 空行が見つからなければ何もしない。
    pub fn apply_h1(&self, head: &mut Vec<u8>) {
        let Some(end) = find_head_end(head) else {
            return;
        };
        // ステータス行の後ろからヘッダの行を見る
        let Some(first) = head[..end].windows(2).position(|w| w == b"\r\n") else {
            return;
        };
        let mut has_origin = false;
        let mut has_cors = false;
        for line in head[first + 2..end].split(|&b| b == b'\n') {
            let name = line.split(|&b| b == b':').next().unwrap_or(b"");
            let name = name.trim_ascii();
            if name.eq_ignore_ascii_case(CORS_NAMES[0]) {
                has_origin = true;
            }
            if is_cors_name(name) {
                has_cors = true;
            }
        }
        if has_origin && !self.override_upstream {
            return;
        }
        let mut insert_at = end;
        if has_cors && self.override_upstream {
            // 上流の CORS のヘッダの行を除いて組み立て直す（上書きを選んだルートだけ）
            let mut out = Vec::with_capacity(head.len() + self.h1_lines.len());
            out.extend_from_slice(&head[..first + 2]);
            let mut pos = first + 2;
            while pos < end {
                let next = head[pos..end]
                    .windows(2)
                    .position(|w| w == b"\r\n")
                    .map(|i| pos + i + 2)
                    .unwrap_or(end);
                let line = &head[pos..next];
                let name = line
                    .split(|&b| b == b':')
                    .next()
                    .unwrap_or(b"")
                    .trim_ascii();
                if !is_cors_name(name) {
                    out.extend_from_slice(line);
                }
                pos = next;
            }
            insert_at = out.len();
            out.extend_from_slice(&head[end..]);
            *head = out;
        }
        head.splice(insert_at..insert_at, self.h1_lines.iter().copied());
    }

    /// 名前/値の組の列へ足す（HTTP/2・HTTP/3）。`name` は要素の名前、`make` は足す要素を作る。
    pub fn apply_with<T>(
        &self,
        headers: &mut Vec<T>,
        name: impl Fn(&T) -> &[u8],
        make: impl Fn(&Bytes, &Bytes) -> T,
    ) {
        let has_origin = headers
            .iter()
            .any(|h| name(h).eq_ignore_ascii_case(CORS_NAMES[0]));
        if has_origin {
            if !self.override_upstream {
                return;
            }
            headers.retain(|h| !is_cors_name(name(h)));
        }
        headers.reserve(self.fields.len());
        for (k, v) in self.fields.iter() {
            headers.push(make(k, v));
        }
    }

    /// `Vec<(Bytes, Bytes)>`（HTTP/2 の応答ヘッダ）へ足す
    #[inline]
    pub fn apply_bytes(&self, headers: &mut Vec<(Bytes, Bytes)>) {
        self.apply_with(headers, |h| &h.0, |k, v| (k.clone(), v.clone()));
    }
}

/// head の終わり（最後のヘッダの行の `\r\n` の直後、空行の `\r\n` の位置）
fn find_head_end(head: &[u8]) -> Option<usize> {
    head.windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 2)
}

/// HTTP/1.1 の応答の head へ、ルートの CORS のヘッダを足す（無いルートは分岐 1 個）
#[inline]
pub fn apply_h1(head: &mut Vec<u8>, security: &SecurityConfig) {
    if let Some(c) = &security.cors_actual {
        c.apply_h1(head);
    }
}

/// veil が組み立てている途中の HTTP/1.1 の head（空行の前）へ、CORS のヘッダの行をそのまま足す（静的配信・
/// リダイレクト等。上流のヘッダが無いので照合しない）
#[inline]
pub fn append_h1_lines(out: &mut Vec<u8>, security: &SecurityConfig) {
    if let Some(c) = &security.cors_actual {
        out.extend_from_slice(&c.h1_lines);
    }
}

/// HTTP/2 の応答ヘッダへ、ルートの CORS のヘッダを足す（無いルートは分岐 1 個）
#[inline]
pub fn apply_bytes(headers: &mut Vec<(Bytes, Bytes)>, security: &SecurityConfig) {
    if let Some(c) = &security.cors_actual {
        c.apply_bytes(headers);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn actual(override_upstream: bool) -> CorsActual {
        CorsActual::new(
            vec![
                (
                    Bytes::from_static(b"access-control-allow-origin"),
                    Bytes::from_static(b"https://a.example"),
                ),
                (Bytes::from_static(b"vary"), Bytes::from_static(b"Origin")),
            ],
            override_upstream,
        )
    }

    #[test]
    fn h1_appends_before_blank_line_and_keeps_body() {
        let mut head =
            b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nVary: Accept-Encoding\r\n\r\nhi".to_vec();
        actual(false).apply_h1(&mut head);
        assert_eq!(
            String::from_utf8(head).unwrap(),
            "HTTP/1.1 200 OK\r\nContent-Length: 2\r\nVary: Accept-Encoding\r\n\
             Access-Control-Allow-Origin: https://a.example\r\nVary: Origin\r\n\r\nhi"
        );
    }

    #[test]
    fn h1_keeps_upstream_origin_unless_override() {
        let raw = b"HTTP/1.1 200 OK\r\naccess-control-allow-origin: https://u.example\r\nAccess-Control-Expose-Headers: x\r\nX: y\r\n\r\n";
        let mut head = raw.to_vec();
        actual(false).apply_h1(&mut head);
        assert_eq!(head, raw.to_vec());

        let mut head = raw.to_vec();
        actual(true).apply_h1(&mut head);
        assert_eq!(
            String::from_utf8(head).unwrap(),
            "HTTP/1.1 200 OK\r\nX: y\r\nAccess-Control-Allow-Origin: https://a.example\r\nVary: Origin\r\n\r\n"
        );
    }

    #[test]
    fn h1_without_blank_line_is_untouched() {
        let mut head = b"HTTP/1.1 200 OK\r\nX: y\r\n".to_vec();
        actual(false).apply_h1(&mut head);
        assert_eq!(head, b"HTTP/1.1 200 OK\r\nX: y\r\n".to_vec());
    }

    #[test]
    fn fields_respect_upstream_and_override() {
        let up = || {
            vec![
                (
                    Bytes::from_static(b"Access-Control-Allow-Origin"),
                    Bytes::from_static(b"*"),
                ),
                (Bytes::from_static(b"x"), Bytes::from_static(b"1")),
            ]
        };
        let mut h = up();
        actual(false).apply_bytes(&mut h);
        assert_eq!(h, up());

        let mut h = up();
        actual(true).apply_bytes(&mut h);
        assert_eq!(h.len(), 3);
        assert_eq!(h[0].0, Bytes::from_static(b"x"));
        assert_eq!(h[1].1, Bytes::from_static(b"https://a.example"));

        let mut h = vec![(
            Bytes::from_static(b"vary"),
            Bytes::from_static(b"Accept-Encoding"),
        )];
        actual(false).apply_bytes(&mut h);
        // 既存の Vary は残して Origin の行を足す
        assert_eq!(h.len(), 3);
        assert_eq!(
            h[2],
            (Bytes::from_static(b"vary"), Bytes::from_static(b"Origin"))
        );
    }

    #[test]
    fn h1_names_are_capitalized() {
        assert_eq!(
            h1_name(b"access-control-allow-origin"),
            b"Access-Control-Allow-Origin".to_vec()
        );
        assert_eq!(h1_name(b"vary"), b"Vary".to_vec());
    }
}
