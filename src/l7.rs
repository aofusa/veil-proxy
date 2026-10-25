//! HTTP/1.1・HTTP/2・HTTP/3 で共通の L7 の段（F-234）。
//!
//! ルートの照合と、ルートのセキュリティ設定による要求の検査を 1 箇所にまとめる。各プロトコルは結果を受けて
//! 自分の形（HTTP/1.1 は既製の応答のバイト列、HTTP/2 はストリームへのフレーム、HTTP/3 は quiche のストリーム）で
//! 応答を書く。共通の段は同期の関数だけにし、`.await` を持ち込まない（HTTP/3 のメインループで待たない。B-97）。
//!
//! 以前は 3 経路がそれぞれ書いていて、次の違いがあった（B-132）。
//! - 検査の順: HTTP/1.1 は IP → メソッド → 本文の大きさ → レート制限、HTTP/2・HTTP/3 は本文の大きさの前に
//!   レート制限を数えていた（上限を超えた要求がレート制限の枠を使い、429 と 413 のどちらが返るかも違った）。
//! - HTTP/2・HTTP/3 は、ルートが見つからないとき authority を空にしてもう一度照合していた。ホストの条件の無い
//!   ルートは常に候補に入る（`routing::HostRouter::get_candidates` の any_host）ので、2 回目は何も見つけない
//!   照合をもう 1 回するだけだった（404 のたびにルートを 2 回引いていた）。

use crate::config::Backend;
use crate::config::{CompressionConfig, SecurityConfig, CURRENT_CONFIG};
use crate::constants::{
    ERR_MSG_FORBIDDEN, ERR_MSG_METHOD_NOT_ALLOWED, ERR_MSG_REQUEST_TOO_LARGE,
    ERR_MSG_TOO_MANY_REQUESTS,
};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

/// HTTP/2・HTTP/3 のバッファ経路で、ストリームに溜める要求本文の上限（全ルートの `max_request_body_size` の
/// 最大。最低でも [`crate::pool::MAX_BODY_SIZE`]）。
///
/// バッファ経路はルートの検査（`check_security` の本文の上限）が END_STREAM の後にしか走らず、受信の
/// フロー制御のクレジットは受け取るたびに返すので、上限が無いと終わらない本文で無限に溜められた
/// （F-236 の再走査の F3・F4）。ストリームに積む時点で、どのルートでも許されない大きさを断る。
static MAX_BUFFERED_REQUEST_BODY: AtomicUsize = AtomicUsize::new(crate::pool::MAX_BODY_SIZE);

/// 設定のロード・リロードで [`max_buffered_request_body`] を決め直す
pub fn set_max_buffered_request_body(routes: &[crate::config::Route]) {
    let max = routes
        .iter()
        .filter_map(|r| r.security.as_ref())
        .map(|s| s.max_request_body_size)
        .max()
        .unwrap_or(0)
        .max(crate::pool::MAX_BODY_SIZE);
    MAX_BUFFERED_REQUEST_BODY.store(max, Ordering::Relaxed);
}

/// バッファ経路の要求本文の上限（[`set_max_buffered_request_body`] 参照）
#[inline]
pub fn max_buffered_request_body() -> usize {
    MAX_BUFFERED_REQUEST_BODY.load(Ordering::Relaxed)
}

/// 照合したルート: パスの前置き・Backend・圧縮の設定（どれもルートが持つ解決済みの値の `Arc` の複製）。
pub type RouteMatch = (Arc<[u8]>, Backend, Arc<CompressionConfig>);

/// パスをクエリの前と後に分ける（`?` が無ければクエリは空）。
#[inline]
pub fn split_query(path: &[u8]) -> (&[u8], &[u8]) {
    match path.iter().position(|&b| b == b'?') {
        Some(i) => (&path[..i], &path[i + 1..]),
        None => (path, b""),
    }
}

/// 要求に合うルートを引く。
///
/// `path` はクエリを除いたパス、`raw_query` は `?` の後ろ。`draw` が偽なら、ルートのトラフィック制御
/// （F-219 の重み付き分割・フォルト）の乱数を引かない（HTTP/2 の下見・HTTP/3 の classify。下見と本番で
/// 2 回引かない）。
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn resolve_route(
    authority: &[u8],
    path: &[u8],
    method: &[u8],
    headers: &[(&[u8], &[u8])],
    raw_query: &[u8],
    client_addr: &SocketAddr,
    draw: bool,
) -> Option<RouteMatch> {
    let config = CURRENT_CONFIG.load();
    if draw {
        crate::upstream::find_backend_unified(
            authority,
            path,
            method,
            headers,
            raw_query,
            client_addr,
            config.route.as_slice(),
            &config.upstream_groups,
        )
    } else {
        crate::upstream::find_backend_preview(
            authority,
            path,
            method,
            headers,
            raw_query,
            client_addr,
            config.route.as_slice(),
            &config.upstream_groups,
        )
    }
}

/// セキュリティチェック結果
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SecurityCheckResult {
    /// 許可（処理を継続）
    Allowed,
    /// IP拒否（403 Forbidden）
    IpDenied,
    /// メソッド不許可（405 Method Not Allowed）
    MethodNotAllowed,
    /// レート制限超過（429 Too Many Requests）
    RateLimitExceeded,
    /// リクエストサイズ超過（413 Request Entity Too Large）
    RequestTooLarge,
}

impl SecurityCheckResult {
    /// HTTPステータスコードに変換
    #[inline]
    pub fn status_code(&self) -> u16 {
        match self {
            SecurityCheckResult::Allowed => 200,
            SecurityCheckResult::IpDenied => 403,
            SecurityCheckResult::MethodNotAllowed => 405,
            SecurityCheckResult::RateLimitExceeded => 429,
            SecurityCheckResult::RequestTooLarge => 413,
        }
    }

    /// エラーメッセージを取得
    #[inline]
    pub fn message(&self) -> &'static [u8] {
        match self {
            SecurityCheckResult::Allowed => b"OK",
            SecurityCheckResult::IpDenied => b"Forbidden",
            SecurityCheckResult::MethodNotAllowed => b"Method Not Allowed",
            SecurityCheckResult::RateLimitExceeded => b"Too Many Requests",
            SecurityCheckResult::RequestTooLarge => b"Request Entity Too Large",
        }
    }

    /// HTTP/1.1 の既製の応答（`Connection: close` 付き。`constants.rs`）。`Allowed` には無い。
    #[inline]
    pub fn http1_response(&self) -> Option<&'static [u8]> {
        match self {
            SecurityCheckResult::Allowed => None,
            SecurityCheckResult::IpDenied => Some(ERR_MSG_FORBIDDEN),
            SecurityCheckResult::MethodNotAllowed => Some(ERR_MSG_METHOD_NOT_ALLOWED),
            SecurityCheckResult::RateLimitExceeded => Some(ERR_MSG_TOO_MANY_REQUESTS),
            SecurityCheckResult::RequestTooLarge => Some(ERR_MSG_REQUEST_TOO_LARGE),
        }
    }
}

/// 統合セキュリティチェック（すべてのプロトコル共用）
///
/// ## チェック項目（この順。B-132 で 3 経路を揃えた）
/// 1. IP制限（allowed_ips, denied_ips）
/// 2. HTTPメソッド制限（allowed_methods）
/// 3. ボディサイズ制限（max_request_body_size。chunked は本文を読みながら別に数える）
/// 4. レートリミット（rate_limit_requests_per_min）
///
/// 状態を持たない検査（1〜3）を先にし、ほかの理由で拒否する要求にレート制限の枠を使わせない
/// （nginx も `client_max_body_size` の検査は `limit_req` より前）。
///
/// ## パフォーマンス
/// 設定がデフォルトの場合、各検査は設定の有無を見る分岐だけで抜ける。
#[inline]
pub fn check_security(
    security: &SecurityConfig,
    client_ip: &str,
    method: &[u8],
    content_length: usize,
    is_chunked: bool,
) -> SecurityCheckResult {
    // IP制限チェック
    let ip_filter = security.ip_filter();
    if ip_filter.is_configured() && !ip_filter.is_allowed(client_ip) {
        return SecurityCheckResult::IpDenied;
    }

    // 許可メソッドチェック
    if !security.allowed_methods.is_empty() {
        let method_str = std::str::from_utf8(method).unwrap_or("GET");
        let is_allowed = security
            .allowed_methods
            .iter()
            .any(|m| m.eq_ignore_ascii_case(method_str));
        if !is_allowed {
            return SecurityCheckResult::MethodNotAllowed;
        }
    }

    // ボディサイズ制限（chunked以外）
    if !is_chunked && content_length > security.max_request_body_size {
        return SecurityCheckResult::RequestTooLarge;
    }

    // レートリミットチェック（B-31: ルート × IP ごと、全ワーカー共通）
    if security.rate_limit_requests_per_min > 0
        && !crate::config::check_rate_limit(
            if security.rate_limit_zone != 0 {
                security.rate_limit_zone
            } else {
                security as *const SecurityConfig as usize
            },
            client_ip,
            security.rate_limit_requests_per_min,
        )
    {
        return SecurityCheckResult::RateLimitExceeded;
    }

    SecurityCheckResult::Allowed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn split_query_with_and_without_question_mark() {
        assert_eq!(
            split_query(b"/a/b?x=1&y=2"),
            (&b"/a/b"[..], &b"x=1&y=2"[..])
        );
        assert_eq!(split_query(b"/a/b"), (&b"/a/b"[..], &b""[..]));
        assert_eq!(split_query(b"/?"), (&b"/"[..], &b""[..]));
    }

    #[test]
    fn http1_responses_match_status_codes() {
        for r in [
            SecurityCheckResult::IpDenied,
            SecurityCheckResult::MethodNotAllowed,
            SecurityCheckResult::RateLimitExceeded,
            SecurityCheckResult::RequestTooLarge,
        ] {
            let resp = r.http1_response().unwrap();
            let line = format!("HTTP/1.1 {}", r.status_code());
            assert!(resp.starts_with(line.as_bytes()), "{:?}", r);
        }
        assert!(SecurityCheckResult::Allowed.http1_response().is_none());
    }

    /// 本文の大きさの上限を超えた要求は 413 で、レート制限の枠を使わない（B-132）
    #[test]
    fn oversized_request_does_not_consume_rate_limit() {
        let security = SecurityConfig {
            max_request_body_size: 10,
            rate_limit_requests_per_min: 1,
            ..Default::default()
        };
        let ip = "192.0.2.77";
        for _ in 0..3 {
            assert_eq!(
                check_security(&security, ip, b"POST", 100, false),
                SecurityCheckResult::RequestTooLarge
            );
        }
        // 枠はまだ 1 回分残っている
        assert_eq!(
            check_security(&security, ip, b"POST", 1, false),
            SecurityCheckResult::Allowed
        );
        assert_eq!(
            check_security(&security, ip, b"POST", 1, false),
            SecurityCheckResult::RateLimitExceeded
        );
    }
}
