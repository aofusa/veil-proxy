//! 要求のパスの正規化（Envoy の HCM の `normalize_path`・`merge_slashes`・
//! `path_with_escaped_slashes_action`。F-236 の F5（メッシュ）・再走査の F7（TOML のルート））。
//!
//! ルート・RBAC・JWT・`[route.security]` の照合の前に 1 回だけ行い、正規化したパスを上流へも送る。照合は
//! 生のパスで、上流は正規化したパスで解釈すると、`/%61dmin`・`/./admin`・`/public/../admin` で `/admin/*` の
//! 制限を素通りできる。メッシュは xDS の HCM の設定で、TOML の設定は `[security] path_normalization` で決める。
//!
//! 正規化の中身（Envoy の `PathUtil::canonicalPath` と同じ範囲）:
//! - 予約されていない文字（`ALPHA DIGIT - . _ ~`）の `%XX` を元の文字に戻す。ほかの `%xx` は大文字の 16 進にする
//! - `\` を `/` にする
//! - ドットのセグメント（`.`・`..`）を RFC 3986 §5.2.4 の通りに取り除く
//! - `merge_slashes` なら連続する `/` を 1 つにする
//!
//! クエリ（`?` から後）は変えない。`/` で始まらないもの（`*`・絶対形式）は触らない。
//! ホットパス: 正規化の要らないパス（ほとんど全部）は 1 回走査するだけで確保しない。

/// `%2F`・`%5C`（エスケープされたスラッシュ）の扱い（Envoy の `PathWithEscapedSlashesAction`）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EscapedSlashes {
    /// そのまま残す（`IMPLEMENTATION_SPECIFIC_DEFAULT`・`KEEP_UNCHANGED`）
    #[default]
    Keep,
    /// 400 で拒否する（`REJECT_REQUEST`）
    Reject,
    /// `/`・`\` に戻してから正規化する（`UNESCAPE_AND_FORWARD`・`UNESCAPE_AND_REDIRECT`）
    Unescape,
}

impl EscapedSlashes {
    /// xDS の列挙値から
    pub fn from_xds(v: i32) -> Self {
        match v {
            2 => EscapedSlashes::Reject,
            3 | 4 => EscapedSlashes::Unescape,
            _ => EscapedSlashes::Keep,
        }
    }
}

/// 正規化の設定
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PathNormalization {
    pub normalize: bool,
    pub merge_slashes: bool,
    pub escaped_slashes: EscapedSlashes,
}

impl Default for PathNormalization {
    /// Istio の既定（`BASE`）と同じく、ドットのセグメントの除去と予約されていない文字の復号を行う
    fn default() -> Self {
        PathNormalization {
            normalize: true,
            merge_slashes: false,
            escaped_slashes: EscapedSlashes::Keep,
        }
    }
}

/// 拒否（`EscapedSlashes::Reject` で `%2F`・`%5C` があった）
#[derive(Debug, PartialEq, Eq)]
pub struct Rejected;

#[inline]
fn hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

#[inline]
fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// 正規化が要るかを確保なしで調べる
fn needs_work(path: &[u8], cfg: &PathNormalization) -> bool {
    let mut prev = 0u8;
    for (i, &b) in path.iter().enumerate() {
        match b {
            b'%' | b'\\' => return true,
            b'/' if cfg.merge_slashes && prev == b'/' => return true,
            b'.' if prev == b'/' => {
                // "/." の後が終端・"/"・".（終端か /）" ならドットのセグメント
                let rest = &path[i + 1..];
                if rest.is_empty()
                    || rest[0] == b'/'
                    || (rest[0] == b'.' && (rest.len() == 1 || rest[1] == b'/'))
                {
                    return true;
                }
            }
            _ => {}
        }
        prev = b;
    }
    false
}

/// RFC 3986 §5.2.4 のドットのセグメントの除去（入力は `/` で始まる）
fn remove_dot_segments(input: &[u8], merge_slashes: bool) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(input.len());
    // セグメントの単位で見る。input は "/" で始まるので split の最初は空
    let mut segs = input.split(|&b| b == b'/').skip(1).peekable();
    while let Some(seg) = segs.next() {
        let last = segs.peek().is_none();
        match seg {
            b"." => {
                if last {
                    out.push(b'/');
                }
            }
            b".." => {
                // 直前のセグメントを取り除く（ルートより上へは行かない）
                match out.iter().rposition(|&b| b == b'/') {
                    Some(p) => out.truncate(p),
                    None => out.clear(),
                }
                if last {
                    out.push(b'/');
                }
            }
            b"" if merge_slashes && !last => {}
            _ => {
                out.push(b'/');
                out.extend_from_slice(seg);
            }
        }
    }
    if out.is_empty() {
        out.push(b'/');
    }
    out
}

/// パス（クエリ付き）を正規化する。変わらなければ `Ok(None)`。
pub fn normalize(path: &[u8], cfg: &PathNormalization) -> Result<Option<Vec<u8>>, Rejected> {
    if !cfg.normalize && !cfg.merge_slashes && cfg.escaped_slashes == EscapedSlashes::Keep {
        return Ok(None);
    }
    let (p, query) = match memchr::memchr(b'?', path) {
        Some(i) => (&path[..i], &path[i..]),
        None => (path, &b""[..]),
    };
    if p.first() != Some(&b'/') || !needs_work(p, cfg) {
        return Ok(None);
    }
    // 1. %XX の復号と `\` の置き換え
    let mut decoded: Vec<u8> = Vec::with_capacity(p.len());
    let mut i = 0;
    while i < p.len() {
        let b = p[i];
        if b == b'%' && i + 2 < p.len() {
            if let (Some(h), Some(l)) = (hex(p[i + 1]), hex(p[i + 2])) {
                let c = (h << 4) | l;
                if c == b'/' || c == b'\\' {
                    match cfg.escaped_slashes {
                        EscapedSlashes::Reject => return Err(Rejected),
                        EscapedSlashes::Unescape => decoded.push(b'/'),
                        EscapedSlashes::Keep => {
                            decoded.push(b'%');
                            decoded.push(p[i + 1].to_ascii_uppercase());
                            decoded.push(p[i + 2].to_ascii_uppercase());
                        }
                    }
                } else if cfg.normalize && is_unreserved(c) {
                    decoded.push(c);
                } else {
                    decoded.push(b'%');
                    decoded.push(p[i + 1].to_ascii_uppercase());
                    decoded.push(p[i + 2].to_ascii_uppercase());
                }
                i += 3;
                continue;
            }
        }
        decoded.push(if b == b'\\' && cfg.normalize { b'/' } else { b });
        i += 1;
    }
    // 2. ドットのセグメント・連続するスラッシュ
    let mut out = if cfg.normalize {
        remove_dot_segments(&decoded, cfg.merge_slashes)
    } else if cfg.merge_slashes {
        let mut v = Vec::with_capacity(decoded.len());
        for &b in &decoded {
            if !(b == b'/' && v.last() == Some(&b'/')) {
                v.push(b);
            }
        }
        v
    } else {
        decoded
    };
    if out == p {
        return Ok(None);
    }
    out.extend_from_slice(query);
    Ok(Some(out))
}

/// TOML の設定（`[security] path_normalization`・`escaped_slashes`）。下位 2 ビットは 0 = off、1 = base、
/// 2 = merge_slashes。その上の 2 ビットは 0 = keep、1 = reject、2 = unescape（B-385）
static REQUEST_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// 設定のロード・リロードで TOML の経路のモードを決める
pub fn set_request_mode(
    mode: crate::config::PathNormalizationMode,
    escaped: crate::config::EscapedSlashesMode,
) {
    use crate::config::EscapedSlashesMode as E;
    use crate::config::PathNormalizationMode as M;
    let path = match mode {
        M::Off => 0,
        M::Base => 1,
        M::MergeSlashes => 2,
    };
    let esc = match escaped {
        E::Keep => 0,
        E::Reject => 1,
        E::Unescape => 2,
    };
    REQUEST_MODE.store(path | (esc << 2), std::sync::atomic::Ordering::Relaxed);
}

/// TOML の経路（HTTP/1.1・HTTP/2・HTTP/3）の要求のパスを正規化する。変わらなければ `Ok(None)`、断る要求は
/// `Err(Rejected)`（呼び出し側が 400・ストリームのエラーにする）。
///
/// 正規化の要らないパスは 1 回の走査だけで、確保しない（ホットパス）。
#[inline]
pub fn normalize_request_path(path: &[u8]) -> Result<Option<Vec<u8>>, Rejected> {
    normalize_request_path_in(
        path,
        REQUEST_MODE.load(std::sync::atomic::Ordering::Relaxed),
    )
}

#[inline]
fn normalize_request_path_in(path: &[u8], mode: u8) -> Result<Option<Vec<u8>>, Rejected> {
    let escaped_slashes = match mode >> 2 {
        1 => EscapedSlashes::Reject,
        2 => EscapedSlashes::Unescape,
        _ => EscapedSlashes::Keep,
    };
    let cfg = match mode & 3 {
        0 => PathNormalization {
            normalize: false,
            merge_slashes: false,
            escaped_slashes,
        },
        1 => PathNormalization {
            escaped_slashes,
            ..PathNormalization::default()
        },
        _ => PathNormalization {
            merge_slashes: true,
            escaped_slashes,
            ..PathNormalization::default()
        },
    };
    // B-385: `keep` でも、エスケープされたスラッシュがドットのセグメントを作るパスは断る（`/public/..%2Fadmin` は
    // `/public/*` に合うが、`%2F` を戻してからドットのセグメントを取り除く上流（nginx 等）は `/admin` を返す）
    if escaped_slashes == EscapedSlashes::Keep && cfg.normalize && escaped_slash_dot_segment(path) {
        return Err(Rejected);
    }
    normalize(path, &cfg)
}

/// パス（クエリの前まで）に、エスケープされたスラッシュ（`%2F`・`%5C`）と隣り合うドットのセグメント（`.`・`..`。
/// `%2E` も `.` と数える）があるか（B-385）。`%` の無いパス（ほとんど全部）は `memchr` 1 回で終わる
fn escaped_slash_dot_segment(path: &[u8]) -> bool {
    let p = match memchr::memchr(b'?', path) {
        Some(i) => &path[..i],
        None => path,
    };
    if memchr::memchr(b'%', p).is_none() {
        return false;
    }
    // 区切り（`/`・`\`・`%2F`・`%5C`）の間のセグメントを見る。区切りがエスケープかどうかを前後で覚える
    let mut prev_escaped = false;
    let mut dots = 0usize;
    let mut other = false;
    let mut i = 0;
    loop {
        // この位置の区切り（長さ, エスケープか）。終端は通常の区切りとして扱う
        let sep = if i >= p.len() {
            Some((0, false))
        } else if p[i] == b'/' || p[i] == b'\\' {
            Some((1, false))
        } else if p[i] == b'%' && i + 2 < p.len() {
            match (hex(p[i + 1]), hex(p[i + 2])) {
                (Some(h), Some(l)) if matches!((h << 4) | l, b'/' | b'\\') => Some((3, true)),
                _ => None,
            }
        } else {
            None
        };
        if let Some((len, escaped)) = sep {
            if !other && (dots == 1 || dots == 2) && (prev_escaped || escaped) {
                return true;
            }
            if i >= p.len() {
                return false;
            }
            prev_escaped = escaped;
            dots = 0;
            other = false;
            i += len;
            continue;
        }
        // セグメントの中身: `.` か `%2E` だけならドットを数える
        if p[i] == b'.' {
            dots += 1;
            i += 1;
        } else if p[i] == b'%'
            && i + 2 < p.len()
            && matches!((hex(p[i + 1]), hex(p[i + 2])), (Some(2), Some(0xE)))
        {
            dots += 1;
            i += 3;
        } else {
            other = true;
            i += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn n(path: &str, cfg: PathNormalization) -> String {
        match normalize(path.as_bytes(), &cfg).unwrap() {
            Some(v) => String::from_utf8(v).unwrap(),
            None => path.to_string(),
        }
    }

    #[test]
    fn dot_segments_and_unreserved_decoding() {
        let c = PathNormalization::default();
        assert_eq!(n("/admin/users", c), "/admin/users");
        assert_eq!(n("/%61dmin/users", c), "/admin/users");
        assert_eq!(n("/./admin/users", c), "/admin/users");
        assert_eq!(n("/public/../admin/users", c), "/admin/users");
        assert_eq!(n("/public/%2e%2e/admin", c), "/admin");
        assert_eq!(n("/a/b/..", c), "/a/");
        assert_eq!(n("/..", c), "/");
        assert_eq!(n("/../../x", c), "/x");
        assert_eq!(n("/a/./b/.", c), "/a/b/");
        assert_eq!(n("/a\\..\\admin", c), "/admin");
        assert_eq!(n("/a/%2f/b", c), "/a/%2F/b");
        assert_eq!(n("/a/%7euser?x=/../y", c), "/a/~user?x=/../y");
        assert_eq!(n("/a//b", c), "/a//b");
        assert_eq!(n("/.well-known/x", c), "/.well-known/x");
        assert_eq!(n("/a/..b/c", c), "/a/..b/c");
        assert_eq!(n("*", c), "*");
        assert_eq!(n("/bad%zz", c), "/bad%zz");
        assert_eq!(n("/end%4", c), "/end%4");
    }

    #[test]
    fn merge_and_escaped_slashes() {
        let mut c = PathNormalization {
            merge_slashes: true,
            ..Default::default()
        };
        assert_eq!(n("//admin///users", c), "/admin/users");
        c.escaped_slashes = EscapedSlashes::Unescape;
        assert_eq!(n("/a%2F..%2Fadmin", c), "/admin");
        c.escaped_slashes = EscapedSlashes::Reject;
        assert_eq!(normalize(b"/a%2fb", &c), Err(Rejected));
        let off = PathNormalization {
            normalize: false,
            merge_slashes: false,
            escaped_slashes: EscapedSlashes::Keep,
        };
        assert_eq!(n("/./a/../b", off), "/./a/../b");
    }

    // B-385: エスケープされたスラッシュと隣り合うドットのセグメント
    #[test]
    fn escaped_slash_dot_segments_are_found() {
        for bad in [
            "/public/..%2Fadmin",
            "/public/..%2fadmin",
            "/public%2F..%2Fadmin",
            "/public%2F../admin",
            "/a/%2e%2e%2fadmin",
            "/a/.%2Fb",
            "/a%5C..%5Cadmin",
            "/a/..%5c",
            "/x%2F..",
        ] {
            assert!(escaped_slash_dot_segment(bad.as_bytes()), "{bad}");
        }
        for ok in [
            "/a/b",
            "/group%2Fproject/x",
            "/a/../b",
            "/.well-known%2Fx",
            "/a/...%2Fb",
            "/a/..b%2Fc",
            "/a%2F.x",
            "/q?x=..%2F",
            "/bad%2",
        ] {
            assert!(!escaped_slash_dot_segment(ok.as_bytes()), "{ok}");
        }
    }

    // B-385: `[security] path_normalization`・`escaped_slashes` の組み合わせ（モードのビットは `set_request_mode` と同じ）
    #[test]
    fn request_path_modes() {
        const BASE_KEEP: u8 = 1;
        const BASE_REJECT: u8 = 1 | (1 << 2);
        const BASE_UNESCAPE: u8 = 1 | (2 << 2);
        const OFF_KEEP: u8 = 0;
        let r = |p: &str, m: u8| normalize_request_path_in(p.as_bytes(), m);
        assert_eq!(r("/a/b", BASE_KEEP), Ok(None));
        assert_eq!(
            r("/group%2fproject", BASE_KEEP),
            Ok(Some(b"/group%2Fproject".to_vec()))
        );
        assert_eq!(r("/public/..%2Fadmin", BASE_KEEP), Err(Rejected));
        assert_eq!(r("/a%2Fb", BASE_REJECT), Err(Rejected));
        assert_eq!(r("/a/b", BASE_REJECT), Ok(None));
        assert_eq!(
            r("/public/..%2Fadmin", BASE_UNESCAPE),
            Ok(Some(b"/admin".to_vec()))
        );
        // `off` は何もしない（以前の動作）
        assert_eq!(r("/public/..%2Fadmin", OFF_KEEP), Ok(None));
    }
}
