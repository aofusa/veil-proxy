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

/// TOML の設定（`[security] path_normalization`）のモード。0 = off、1 = base、2 = merge_slashes
static REQUEST_MODE: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(1);

/// 設定のロード・リロードで TOML の経路のモードを決める
pub fn set_request_mode(mode: crate::config::PathNormalizationMode) {
    use crate::config::PathNormalizationMode as M;
    let v = match mode {
        M::Off => 0,
        M::Base => 1,
        M::MergeSlashes => 2,
    };
    REQUEST_MODE.store(v, std::sync::atomic::Ordering::Relaxed);
}

/// TOML の経路（HTTP/1.1・HTTP/2・HTTP/3）の要求のパスを正規化する。変わらなければ `None`。
///
/// 正規化の要らないパスは 1 回の走査だけで、確保しない（ホットパス）。`%2F`・`%5C` はそのまま残す。
#[inline]
pub fn normalize_request_path(path: &[u8]) -> Option<Vec<u8>> {
    let cfg = match REQUEST_MODE.load(std::sync::atomic::Ordering::Relaxed) {
        0 => return None,
        1 => PathNormalization::default(),
        _ => PathNormalization {
            merge_slashes: true,
            ..PathNormalization::default()
        },
    };
    // Keep のときは拒否しない
    normalize(path, &cfg).ok().flatten()
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
}
