//! ハンドラのパスパターン（`/users/{id}`・`/files/{*rest}`）の解析と照合（F-188）。
//!
//! - `{name}`: 1 セグメント（空は不可）
//! - `{*name}`: 末尾のみ。残り全部（空も可）
//! - それ以外: 静的セグメント（完全一致）
//!
//! 照合は割り当て無しで行い、パラメータは元のパス `Bytes` のスライス（参照カウントのみ）で返す。

use bytes::Bytes;
use std::sync::Arc;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Static(Box<str>),
    Param(Arc<str>),
    CatchAll(Arc<str>),
}

/// 解析済みのパスパターン。
#[derive(Debug, Clone)]
pub struct PathPattern {
    raw: Arc<str>,
    segments: Vec<Segment>,
}

/// パターン照合で得たパスパラメータ（名前と、パスの該当部分）。
pub type PathParams = Vec<(Arc<str>, Bytes)>;

impl PathPattern {
    /// パターン文字列を解析する。構文エラーは説明付きの `Err`。
    pub fn parse(pattern: &str) -> Result<PathPattern, String> {
        if !pattern.starts_with('/') {
            return Err(format!("path pattern '{}' must start with '/'", pattern));
        }
        let mut segments = Vec::new();
        let body = &pattern[1..];
        if !body.is_empty() {
            let parts: Vec<&str> = body.split('/').collect();
            let last = parts.len() - 1;
            let mut names: Vec<&str> = Vec::new();
            for (i, part) in parts.iter().enumerate() {
                if let Some(inner) = part.strip_prefix('{').and_then(|p| p.strip_suffix('}')) {
                    let (catch_all, name) = match inner.strip_prefix('*') {
                        Some(n) => (true, n),
                        None => (false, inner),
                    };
                    if name.is_empty()
                        || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
                        || name.starts_with(|c: char| c.is_ascii_digit())
                    {
                        return Err(format!(
                            "invalid parameter name '{}' in path pattern '{}'",
                            inner, pattern
                        ));
                    }
                    if names.contains(&name) {
                        return Err(format!(
                            "duplicate parameter '{}' in path pattern '{}'",
                            name, pattern
                        ));
                    }
                    names.push(name);
                    if catch_all {
                        if i != last {
                            return Err(format!(
                                "catch-all '{{*{}}}' must be the last segment in '{}'",
                                name, pattern
                            ));
                        }
                        segments.push(Segment::CatchAll(Arc::from(name)));
                    } else {
                        segments.push(Segment::Param(Arc::from(name)));
                    }
                } else if part.contains('{') || part.contains('}') {
                    return Err(format!(
                        "a parameter must occupy a whole segment ('{}' in '{}')",
                        part, pattern
                    ));
                } else if part.is_empty() && i != last {
                    return Err(format!("empty segment in path pattern '{}'", pattern));
                } else {
                    segments.push(Segment::Static(Box::from(*part)));
                }
            }
        }
        Ok(PathPattern {
            raw: Arc::from(pattern),
            segments,
        })
    }

    /// 元のパターン文字列。
    pub fn as_str(&self) -> &str {
        &self.raw
    }

    /// パラメータ名（出現順）。
    pub fn param_names(&self) -> impl Iterator<Item = &str> {
        self.segments.iter().filter_map(|s| match s {
            Segment::Param(n) | Segment::CatchAll(n) => Some(&**n),
            Segment::Static(_) => None,
        })
    }

    /// パラメータを含むか（含まなければ静的パス）。
    pub fn has_params(&self) -> bool {
        self.segments
            .iter()
            .any(|s| !matches!(s, Segment::Static(_)))
    }

    /// `path`（クエリを除いたもの）がパターンに一致するか（パラメータは取り出さない）。
    pub fn matches(&self, path: &[u8]) -> bool {
        self.walk(path, |_, _, _| {})
    }

    /// `path` が一致すればパラメータを返す。`path` は `Bytes`（スライスは参照カウントのみ）。
    pub fn captures(&self, path: &Bytes) -> Option<PathParams> {
        let mut params: PathParams = Vec::new();
        if self.walk(path, |name, start, end| {
            params.push((name.clone(), path.slice(start..end)))
        }) {
            Some(params)
        } else {
            None
        }
    }

    /// セグメント単位で照合し、パラメータの (名前, 開始, 終了) を `on_param` に渡す。
    fn walk(&self, path: &[u8], mut on_param: impl FnMut(&Arc<str>, usize, usize)) -> bool {
        if path.first() != Some(&b'/') {
            return false;
        }
        let len = path.len();
        let last = match self.segments.len() {
            0 => return len == 1,
            n => n - 1,
        };
        let mut pos = 1usize; // 先頭の '/' の次
        for (i, seg) in self.segments.iter().enumerate() {
            let name = match seg {
                Segment::CatchAll(name) => {
                    on_param(name, pos.min(len), len);
                    return true;
                }
                Segment::Param(name) => Some(name),
                Segment::Static(_) => None,
            };
            if pos > len {
                return false;
            }
            let rest = &path[pos..];
            let seg_len = rest.iter().position(|&b| b == b'/').unwrap_or(rest.len());
            match (seg, name) {
                (Segment::Static(s), _) if rest[..seg_len] != *s.as_bytes() => return false,
                (_, Some(_)) if seg_len == 0 => return false,
                (_, Some(name)) => on_param(name, pos, pos + seg_len),
                _ => {}
            }
            let end = pos + seg_len;
            if i == last {
                return end == len;
            }
            if end == len {
                // パスが尽きた: 残りが末尾のキャッチオール 1 個だけなら空で一致
                return match self.segments.get(i + 1) {
                    Some(Segment::CatchAll(name)) if i + 1 == last => {
                        on_param(name, len, len);
                        true
                    }
                    _ => false,
                };
            }
            pos = end + 1; // '/' を飛ばす
        }
        true
    }

    /// ルート並べ替え用の具体性（大きいほど具体的）。
    /// (静的セグメント数, -パラメータ数, キャッチオールが無い)
    pub fn specificity(&self) -> (usize, isize, bool) {
        let mut statics = 0usize;
        let mut params = 0isize;
        let mut catch_all = false;
        for s in &self.segments {
            match s {
                Segment::Static(_) => statics += 1,
                Segment::Param(_) => params += 1,
                Segment::CatchAll(_) => catch_all = true,
            }
        }
        (statics, -params, !catch_all)
    }

    /// 衝突検査用の正規化形（パラメータ名を `{}` に置き換えたもの）。
    pub fn normalized(&self) -> String {
        let mut out = String::new();
        for s in &self.segments {
            out.push('/');
            match s {
                Segment::Static(v) => out.push_str(v),
                Segment::Param(_) => out.push_str("{}"),
                Segment::CatchAll(_) => out.push_str("{*}"),
            }
        }
        if out.is_empty() {
            out.push('/');
        }
        out
    }
}

/// ルーティング層（`routing::PathRouter` / `upstream::matches_path_pattern`）用:
/// `{` を含むパターンをセグメント単位で照合する（`handler` feature 時のみ呼ばれる）。
/// リクエストごとに呼ばれるため割り当てをしない（パターンを解析せずに文字列のまま辿る）。
/// 構文は設定ロード時に `PathPattern::parse` で検査済みの前提。
pub fn route_pattern_matches(pattern: &str, path: &[u8]) -> bool {
    let (mut pat, mut p) = match (pattern.as_bytes().split_first(), path.split_first()) {
        (Some((b'/', pr)), Some((b'/', rest))) => (pr, rest),
        _ => return false,
    };
    loop {
        let pend = pat.iter().position(|&b| b == b'/');
        let pseg = &pat[..pend.unwrap_or(pat.len())];
        if pseg.starts_with(b"{*") {
            return true;
        }
        let send = p.iter().position(|&b| b == b'/');
        let sseg = &p[..send.unwrap_or(p.len())];
        if pseg.first() == Some(&b'{') {
            if sseg.is_empty() {
                return false;
            }
        } else if pseg != sseg {
            return false;
        }
        match (pend, send) {
            (None, None) => return true,
            (Some(i), Some(j)) => {
                pat = &pat[i + 1..];
                p = &p[j + 1..];
            }
            // パスが尽きた: 残りが末尾のキャッチオール 1 個だけなら空で一致
            (Some(i), None) => {
                let rest = &pat[i + 1..];
                return rest.starts_with(b"{*") && !rest.contains(&b'/');
            }
            (None, Some(_)) => return false,
        }
    }
}

/// パーセントエンコードを復号する（`+` は空白にしない＝パス用）。不正な列はそのまま残す。
pub fn percent_decode(input: &[u8]) -> std::borrow::Cow<'_, str> {
    if !input.contains(&b'%') {
        return String::from_utf8_lossy(input);
    }
    let mut out = Vec::with_capacity(input.len());
    let mut i = 0;
    while i < input.len() {
        if input[i] == b'%' && i + 2 < input.len() {
            let hi = (input[i + 1] as char).to_digit(16);
            let lo = (input[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(input[i]);
        i += 1;
    }
    std::borrow::Cow::Owned(String::from_utf8_lossy(&out).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn caps(p: &str, path: &str) -> Option<Vec<(String, String)>> {
        let pat = PathPattern::parse(p).unwrap();
        pat.captures(&Bytes::copy_from_slice(path.as_bytes()))
            .map(|v| {
                v.into_iter()
                    .map(|(n, b)| (n.to_string(), String::from_utf8(b.to_vec()).unwrap()))
                    .collect()
            })
    }

    #[test]
    fn static_paths_match_exactly() {
        assert_eq!(caps("/", "/"), Some(vec![]));
        assert_eq!(caps("/", "/x"), None);
        assert_eq!(caps("/users", "/users"), Some(vec![]));
        assert_eq!(caps("/users", "/users/"), None);
        assert_eq!(caps("/users", "/users/1"), None);
        assert_eq!(caps("/a/b", "/a/b"), Some(vec![]));
        assert_eq!(caps("/a/b", "/a/c"), None);
    }

    #[test]
    fn params_capture_one_segment() {
        assert_eq!(
            caps("/users/{id}", "/users/42"),
            Some(vec![("id".into(), "42".into())])
        );
        assert_eq!(caps("/users/{id}", "/users/"), None);
        assert_eq!(caps("/users/{id}", "/users"), None);
        assert_eq!(caps("/users/{id}", "/users/1/2"), None);
        assert_eq!(
            caps("/u/{a}/p/{b}", "/u/x/p/y"),
            Some(vec![("a".into(), "x".into()), ("b".into(), "y".into())])
        );
    }

    #[test]
    fn catch_all_takes_rest_including_empty() {
        assert_eq!(
            caps("/files/{*rest}", "/files/a/b/c"),
            Some(vec![("rest".into(), "a/b/c".into())])
        );
        assert_eq!(
            caps("/files/{*rest}", "/files/"),
            Some(vec![("rest".into(), "".into())])
        );
        assert_eq!(
            caps("/files/{*rest}", "/files"),
            Some(vec![("rest".into(), "".into())])
        );
        assert_eq!(caps("/files/{*rest}", "/other"), None);
    }

    #[test]
    fn syntax_errors() {
        for bad in [
            "users",
            "/users/{}",
            "/users/{id",
            "/users/x{id}",
            "/{*a}/b",
            "/{a}/{a}",
            "/{1a}",
            "/a//b",
        ] {
            assert!(PathPattern::parse(bad).is_err(), "{} must be rejected", bad);
        }
    }

    #[test]
    fn specificity_counts_segments() {
        let s = |p: &str| PathPattern::parse(p).unwrap().specificity();
        // (静的セグメント数, -パラメータ数, キャッチオール無し)。並べ替え自体は routes.rs（深さ優先）。
        assert_eq!(s("/users/me"), (2, 0, true));
        assert_eq!(s("/users/{id}"), (1, -1, true));
        assert_eq!(s("/users/{*rest}"), (1, 0, false));
        assert!(s("/users/me") > s("/users/{id}"));
        assert_eq!(
            PathPattern::parse("/u/{a}").unwrap().normalized(),
            PathPattern::parse("/u/{b}").unwrap().normalized()
        );
    }

    #[test]
    fn route_matcher_agrees_with_path_pattern() {
        let patterns = [
            "/users/{id}",
            "/u/{a}/p/{b}",
            "/files/{*rest}",
            "/a/{b}/c",
            "/{x}",
            "/x/{y}/{*z}",
        ];
        let paths = [
            "/",
            "/users",
            "/users/",
            "/users/1",
            "/users/1/",
            "/users/1/2",
            "/u/x/p/y",
            "/u/x/p",
            "/files",
            "/files/",
            "/files/a/b",
            "/a/b/c",
            "/a//c",
            "/a/b/d",
            "/q",
            "/x/1",
            "/x/1/2/3",
            "x",
            "",
        ];
        for pat in patterns {
            let pp = PathPattern::parse(pat).unwrap();
            for path in paths {
                assert_eq!(
                    route_pattern_matches(pat, path.as_bytes()),
                    pp.matches(path.as_bytes()),
                    "pattern {} path {}",
                    pat,
                    path
                );
            }
        }
    }

    #[test]
    fn percent_decoding() {
        assert_eq!(percent_decode(b"a%20b"), "a b");
        assert_eq!(percent_decode(b"%E3%81%82"), "あ");
        assert_eq!(percent_decode(b"bad%zz"), "bad%zz");
        assert_eq!(percent_decode(b"end%2"), "end%2");
    }
}
