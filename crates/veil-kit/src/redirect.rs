// F-267: リダイレクト。Location の CR/LF を拒み、開いたリダイレクトを防ぐ Redirect::safe を持つ。
//! Redirect responses.
//!
//! ```
//! use veil_kit::redirect::Redirect;
//! let r = Redirect::see_other("/done");
//! // a `?next=` parameter from the user: only paths on this site, or the listed hosts
//! let next = Redirect::safe("https://evil.example/", &["app.example.com"]).unwrap_or_else(|| Redirect::to("/"));
//! # let _ = (r, next);
//! ```

use veil::api::{IntoResponse, Response};

/// A redirect (`Location` plus a 3xx status).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Redirect {
    status: u16,
    location: String,
}

impl Redirect {
    fn new(status: u16, location: &str) -> Redirect {
        // ヘッダの分割（CR・LF）と制御文字は落とす（応答ヘッダに出さない）
        let location: String = location.chars().filter(|c| !c.is_control()).collect();
        Redirect { status, location }
    }

    /// `302 Found` (browsers repeat a POST as GET).
    pub fn to(location: &str) -> Redirect {
        Redirect::new(302, location)
    }

    /// `303 See Other` (after a form POST).
    pub fn see_other(location: &str) -> Redirect {
        Redirect::new(303, location)
    }

    /// `307 Temporary Redirect` (the method and body are kept).
    pub fn temporary(location: &str) -> Redirect {
        Redirect::new(307, location)
    }

    /// `308 Permanent Redirect` (the method and body are kept).
    pub fn permanent(location: &str) -> Redirect {
        Redirect::new(308, location)
    }

    /// `301 Moved Permanently`.
    pub fn moved(location: &str) -> Redirect {
        Redirect::new(301, location)
    }

    /// A `302` to `target` only if it stays on this site (a path such as `/a?b`, not `//host` or `/\host`) or
    /// its host is in `allowed_hosts`; `None` otherwise (prevents open redirects through `?next=` parameters).
    pub fn safe(target: &str, allowed_hosts: &[&str]) -> Option<Redirect> {
        if target.chars().any(|c| c.is_control()) {
            return None;
        }
        if target.starts_with('/') {
            let b = target.as_bytes();
            if b.get(1) == Some(&b'/') || b.get(1) == Some(&b'\\') {
                return None;
            }
            return Some(Redirect::to(target));
        }
        let rest = target
            .strip_prefix("https://")
            .or_else(|| target.strip_prefix("http://"))?;
        let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
        if authority.contains('@') || authority.contains('\\') {
            return None;
        }
        let host = if authority.starts_with('[') {
            // IPv6 リテラル（`[::1]:8443`）
            authority.split_inclusive(']').next().unwrap_or(authority)
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => h,
                _ => authority,
            }
        };
        allowed_hosts
            .iter()
            .any(|a| a.eq_ignore_ascii_case(host))
            .then(|| Redirect::to(target))
    }

    /// The status code.
    pub fn status(&self) -> u16 {
        self.status
    }

    /// The `Location` value.
    pub fn location(&self) -> &str {
        &self.location
    }
}

impl IntoResponse for Redirect {
    fn into_response(self) -> Response {
        Response::new(self.status).with_header("location", self.location)
    }
}

impl From<Redirect> for Response {
    fn from(r: Redirect) -> Response {
        r.into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statuses_and_header_injection() {
        assert_eq!(Redirect::to("/a").status(), 302);
        assert_eq!(Redirect::see_other("/a").status(), 303);
        assert_eq!(Redirect::temporary("/a").status(), 307);
        assert_eq!(Redirect::permanent("/a").status(), 308);
        assert_eq!(Redirect::moved("/a").status(), 301);
        assert_eq!(
            Redirect::to("/a\r\nSet-Cookie: x=1").location(),
            "/aSet-Cookie: x=1"
        );
        let r = Redirect::to("/x").into_response();
        assert_eq!(r.status_code(), 302);
        assert_eq!(r.header("location"), Some(&b"/x"[..]));
    }

    #[test]
    fn safe_blocks_open_redirects() {
        let hosts = ["app.example.com"];
        assert!(Redirect::safe("/dashboard?tab=1", &hosts).is_some());
        assert!(Redirect::safe("//evil.example/", &hosts).is_none());
        assert!(Redirect::safe("/\\evil.example/", &hosts).is_none());
        assert!(Redirect::safe("https://evil.example/", &hosts).is_none());
        assert!(Redirect::safe("https://app.example.com@evil.example/", &hosts).is_none());
        assert!(Redirect::safe("https://APP.example.com:8443/x", &hosts).is_some());
        assert!(Redirect::safe("javascript:alert(1)", &hosts).is_none());
        assert!(Redirect::safe("/a\nb", &hosts).is_none());
    }
}
