// F-267: veil の API ライブラリのヘルパークレート。veil 側の土台（Request::extensions・FromRequestAsync・
// Response::after_send・App::with / serve_dir / on_worker_start・veil::api::client）の上に作る。
//! Helpers for the [veil](https://docs.rs/veil-proxy) API server library.
//!
//! | Module | Feature | What it does |
//! |---|---|---|
//! | [`cookie`] | `cookie` (default) | `CookieJar` extractor, `Set-Cookie` builder, signed and private (encrypted) cookies |
//! | [`redirect`] | always | `Redirect` responses (301/302/303/307/308) and open-redirect protection |
//! | [`serve`] | always | static directories on veil's file server, and single files from a handler |
//! | [`cors`] | `cors` (default) | CORS headers and automatic preflight answers |
//! | [`auth`] | `auth` (default), `jwt` | `BasicAuth`, `BearerToken`, `ApiKey`, `Jwt<Claims>`, `Principal`, guards |
//! | [`session`] | `session` | cookie sessions with a pluggable store |
//! | [`client`] | always | `veil::api::client` plus JSON helpers and retries |
//! | [`grpc`] | `grpc` | `ProstCodec`, channels with round robin over several servers |
//! | [`graphql`] | `graphql` | async-graphql over HTTP, SSE and WebSocket (`graphql-transport-ws`) |
//! | [`di`] | `di` (default) | dependency injection with singleton, scoped and transient lifetimes |
//! | [`tasks`] | `tasks` (default), `cron` | background tasks after the response, and app tasks (`every`, `cron`) |
//!
//! Extractors of this crate are written as handler parameters of `#[veil::route]` like veil's own
//! (`jar: CookieJar`, `user: Auth<Principal>`, `repo: Dep<Repo>`, `tasks: BackgroundTasks`). Your own extractor
//! types (implementing `veil::api::FromRequestAsync`) take `#[inject]` in front of the parameter.
//!
//! Most features install themselves with [`veil::api::App::with`]:
//!
//! ```no_run
//! use veil::prelude::*;
//! use veil_kit::{cors::Cors, di::Container, tasks::Tasks};
//! App::new()
//!     .with(Cors::new().allow_origins(["https://app.example.com"]))
//!     .with(Container::new().value(42u32))
//!     .with(Tasks::new())
//!     .run()
//!     .unwrap();
//! ```
// F-241: docs.rs にソースの表示を出さない（コメントは日本語のため）。
#![doc(html_no_source)]
#![warn(missing_docs)]

pub mod client;
#[cfg(feature = "cookie")]
pub mod cookie;
#[cfg(any(feature = "cookie", feature = "auth", feature = "tasks"))]
mod crypto;
pub mod redirect;
pub mod serve;

#[cfg(feature = "auth")]
pub mod auth;
#[cfg(feature = "cors")]
pub mod cors;
#[cfg(feature = "di")]
pub mod di;
#[cfg(feature = "graphql")]
pub mod graphql;
#[cfg(feature = "grpc")]
pub mod grpc;
#[cfg(feature = "session")]
pub mod session;
#[cfg(feature = "tasks")]
pub mod tasks;

/// Base64 (URL-safe, no padding) used by signed cookies, sessions and JWT.
#[cfg(any(feature = "cookie", feature = "auth"))]
pub(crate) mod b64 {
    #[cfg(any(feature = "cookie", feature = "jwt", test))]
    const URL: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    #[cfg(test)]
    const STD: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

    #[cfg(any(feature = "cookie", feature = "jwt", test))]
    fn encode_with(t: &[u8; 64], input: &[u8], pad: bool) -> String {
        let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
        for c in input.chunks(3) {
            let n = (c[0] as u32) << 16
                | (*c.get(1).unwrap_or(&0) as u32) << 8
                | *c.get(2).unwrap_or(&0) as u32;
            out.push(t[(n >> 18) as usize & 63] as char);
            out.push(t[(n >> 12) as usize & 63] as char);
            if c.len() > 1 {
                out.push(t[(n >> 6) as usize & 63] as char);
            } else if pad {
                out.push('=');
            }
            if c.len() > 2 {
                out.push(t[n as usize & 63] as char);
            } else if pad {
                out.push('=');
            }
        }
        out
    }

    /// URL-safe, no padding (RFC 4648 section 5).
    #[cfg(any(feature = "cookie", feature = "jwt", test))]
    pub(crate) fn encode(input: &[u8]) -> String {
        encode_with(URL, input, false)
    }

    /// Standard alphabet with padding.
    #[cfg(test)]
    pub(crate) fn encode_std(input: &[u8]) -> String {
        encode_with(STD, input, true)
    }

    /// Decodes both alphabets, with or without padding. `None` for invalid input.
    pub(crate) fn decode(input: &[u8]) -> Option<Vec<u8>> {
        let mut out = Vec::with_capacity(input.len() * 3 / 4);
        let mut acc = 0u32;
        let mut bits = 0;
        let mut ended = false;
        for &c in input {
            let v = match c {
                b'A'..=b'Z' => c - b'A',
                b'a'..=b'z' => c - b'a' + 26,
                b'0'..=b'9' => c - b'0' + 52,
                b'+' | b'-' => 62,
                b'/' | b'_' => 63,
                b'=' => {
                    ended = true;
                    continue;
                }
                _ => return None,
            };
            if ended {
                return None;
            }
            acc = acc << 6 | v as u32;
            bits += 6;
            if bits >= 8 {
                bits -= 8;
                out.push((acc >> bits) as u8);
            }
        }
        // 余りのビットは 0 でなければならない（同じ値に複数の表記を許さない）
        if bits >= 6 || acc & ((1 << bits) - 1) != 0 {
            return None;
        }
        Some(out)
    }

    #[cfg(test)]
    mod tests {
        #[test]
        fn roundtrip() {
            for s in [
                &b""[..],
                b"f",
                b"fo",
                b"foo",
                b"foob",
                b"fooba",
                b"foobar",
                &[0xff, 0xfe, 0x00],
            ] {
                assert_eq!(super::decode(super::encode(s).as_bytes()).unwrap(), s);
                assert_eq!(super::decode(super::encode_std(s).as_bytes()).unwrap(), s);
            }
            assert_eq!(super::encode(b"\xfb\xff"), "-_8");
            assert_eq!(super::encode_std(b"f"), "Zg==");
            assert!(super::decode(b"Zh").is_none(), "non-zero trailing bits");
            assert!(super::decode(b"a*b").is_none());
        }
    }
}

// README のコード例を doctest で確かめる（例は JWT を使うので jwt feature のときだけ。既定の feature で
// `cargo test -p veil-proxy-kit` が import できずに落ちていた）
#[cfg(all(doctest, feature = "jwt"))]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;
