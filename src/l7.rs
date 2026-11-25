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
    ERR_MSG_FORBIDDEN, ERR_MSG_METHOD_NOT_ALLOWED, ERR_MSG_REQUEST_HEADER_TOO_LARGE,
    ERR_MSG_REQUEST_TOO_LARGE, ERR_MSG_TOO_MANY_REQUESTS,
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

/// 照合したルートの値: パスの前置き・Backend・圧縮の設定。
pub struct RouteTarget {
    pub prefix: Arc<[u8]>,
    pub backend: Backend,
    pub compression: Arc<CompressionConfig>,
}

/// 要求に合ったルート（[`RouteTarget`] を参照で見せる）。
///
/// F-321: ふつうはワーカーのルートの表（`upstream` の `WORKER_ROUTES`）の値を `Rc` で共有する。以前は要求ごとに
/// ルートの値の `Arc`（Backend の中の数本・前置き・圧縮の設定）を複製していて、全ワーカーが同じ参照カウントへ
/// 書いていた（多コアではコアの間でキャッシュラインを奪い合う）。トラフィック制御（F-219）が要求ごとに選んだ
/// Backend だけは所有する。
pub enum RouteMatch {
    /// ワーカーの表の値（複製はワーカーの中の参照カウントだけ）
    Worker(std::rc::Rc<RouteTarget>),
    /// 要求ごとに選んだ値
    Owned(RouteTarget),
}

impl std::ops::Deref for RouteMatch {
    type Target = RouteTarget;

    #[inline]
    fn deref(&self) -> &RouteTarget {
        match self {
            RouteMatch::Worker(t) => t,
            RouteMatch::Owned(t) => t,
        }
    }
}

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
    resolve_route_in(
        &CURRENT_CONFIG.load(),
        authority,
        path,
        method,
        headers,
        raw_query,
        client_addr,
        draw,
    )
}

/// [`resolve_route`] と同じだが、呼び出し側が読んだ設定で引く（`CURRENT_CONFIG` の読み取りは 1 回 100 命令ほど
/// かかるので、要求の中で読み直さない）。
#[allow(clippy::too_many_arguments)]
#[inline]
pub fn resolve_route_in(
    config: &crate::config::RuntimeConfig,
    authority: &[u8],
    path: &[u8],
    method: &[u8],
    headers: &[(&[u8], &[u8])],
    raw_query: &[u8],
    client_addr: &SocketAddr,
    draw: bool,
) -> Option<RouteMatch> {
    if draw {
        crate::upstream::find_backend_unified(
            authority,
            path,
            method,
            headers,
            raw_query,
            client_addr,
            config,
        )
    } else {
        crate::upstream::find_backend_preview(
            authority,
            path,
            method,
            headers,
            raw_query,
            client_addr,
            config,
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
    /// 要求ヘッダの大きさがルートの `max_request_header_size` を超えた（431。B-482）
    HeaderTooLarge,
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
            SecurityCheckResult::HeaderTooLarge => 431,
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
            SecurityCheckResult::HeaderTooLarge => b"Request Header Fields Too Large",
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
            SecurityCheckResult::HeaderTooLarge => Some(ERR_MSG_REQUEST_HEADER_TOO_LARGE),
        }
    }
}

/// 統合セキュリティチェック（すべてのプロトコル共用）
///
/// ## チェック項目（この順。B-132 で 3 経路を揃えた）
/// 1. IP制限（allowed_ips, denied_ips）
/// 2. HTTPメソッド制限（allowed_methods）
/// 3. ヘッダの大きさ制限（max_request_header_size。B-482）
/// 4. ボディサイズ制限（max_request_body_size。chunked は本文を読みながら別に数える）
/// 5. レートリミット（rate_limit_requests_per_min）
///
/// `header_size` は要求ヘッダの大きさ（HTTP/1.1 は要求行から空行までのバイト数、HTTP/2・HTTP/3 は復号した後の
/// 名前と値の合計。擬似ヘッダを含む）。veil 全体の上限（`request_header_limit_status`）は各プロトコルの入口で先に見る。
///
/// 状態を持たない検査（1〜4）を先にし、ほかの理由で拒否する要求にレート制限の枠を使わせない
/// （nginx も `client_max_body_size` の検査は `limit_req` より前）。
///
/// ## パフォーマンス
/// 設定がデフォルトの場合、各検査は設定の有無を見る分岐だけで抜ける。
#[inline]
pub fn check_security(
    security: &SecurityConfig,
    client_ip: &str,
    method: &[u8],
    header_size: usize,
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

    // ヘッダの大きさ（ルートの上限。B-482。以前は設定を読むだけでどのプロトコルも見ていなかった）
    if header_size > security.max_request_header_size {
        return SecurityCheckResult::HeaderTooLarge;
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

/// veil 全体の要求ヘッダの上限（HTTP/1.1・HTTP/2・HTTP/3 で共通。B-482）。超えていれば応答のステータスを返す
/// （`:path` が `MAX_HEADER_SIZE` を超えれば 414、ヘッダの大きさが `MAX_HEADER_SIZE` を超えるかフィールドが
/// `MAX_HEADER_COUNT` 個を超えれば 431）。`size` は [`check_security`] の `header_size` と同じ数え方、`count` は
/// 擬似ヘッダを除くフィールドの数
#[cfg(any(feature = "http2", feature = "http3"))]
#[inline]
pub(crate) fn request_header_limit_status(
    size: usize,
    count: usize,
    path_len: usize,
) -> Option<u16> {
    if path_len > crate::pool::MAX_HEADER_SIZE {
        Some(414)
    } else if size > crate::pool::MAX_HEADER_SIZE || count > crate::pool::MAX_HEADER_COUNT {
        Some(431)
    } else {
        None
    }
}

/// [`request_header_limit_status`] の応答の本文（`text/plain`）
#[cfg(any(feature = "http2", feature = "http3"))]
#[inline]
pub(crate) fn request_header_limit_message(status: u16) -> &'static [u8] {
    if status == 414 {
        b"URI Too Long"
    } else {
        b"Request Header Fields Too Large"
    }
}

/// 応答を送れない状態の見張り（HTTP/2・HTTP/3 のストリーム。B-487）。
///
/// 応答の続き（本文・見出し・終端）をフロー制御（HTTP/3 は輻輳制御も）で送れない間だけ、接続の見回り（1 秒ごと。
/// 要求の受信の止まりの見回りと同じタイマー）のたびに `sent` が増えたかを見て、`secs` 秒を超えて増えなければ
/// 打ち切らせる（HTTP/2 は `RST_STREAM(CANCEL)`、HTTP/3 は `H3_REQUEST_CANCELLED`）。送れている間は送った量を
/// 足すだけで、要求ごとのタイマーは張らない。HTTP/1.1 の応答の書き込みの時間切れ（`WRITE_TIMEOUT`・
/// `client_write_timeout_secs`。WebSocket の中継も同じ）に当たる。
///
/// ストリーム自身のウィンドウは残っていて接続のウィンドウ（HTTP/3 は輻輳窓も）を待つストリームは、接続のほかの
/// ストリームが進んでいれば止まっていないとみなす（呼び出し側が `stalled_tick` の引数で渡す）。空いたクレジットを
/// どのストリームが先に使うかは veil の順番で決まるので、読む速さが遅いだけのクライアントのストリームを打ち切らない。
#[cfg(any(feature = "http2", feature = "http3"))]
#[derive(Clone, Copy, Debug)]
pub(crate) struct SendStallWatch {
    /// 打ち切るまでの秒数
    secs: u32,
    /// 送った量（本文のバイト数と、見出し 1 つにつき 1）
    pub(crate) sent: u64,
    /// 前回の見回りで見た `sent`
    mark: u64,
    /// 進まなかった見回りの回数
    ticks: u32,
}

#[cfg(any(feature = "http2", feature = "http3"))]
impl SendStallWatch {
    /// 既定の秒数（`WRITE_TIMEOUT`。HTTP/1.1 の応答の書き込みの時間切れと同じ）で見張る
    pub(crate) const fn new() -> Self {
        Self {
            secs: crate::pool::WRITE_TIMEOUT.as_secs() as u32,
            sent: 0,
            mark: 0,
            ticks: 0,
        }
    }

    /// 秒数を変える（`buffering = "full"` の応答は `[route.buffering] client_write_timeout_secs`。HTTP/1.1 と同じ）
    pub(crate) fn set_secs(&mut self, secs: u64) {
        self.secs = secs.min(u64::from(u32::MAX)) as u32;
    }

    /// 送れない状態での見回り 1 回。打ち切るなら `true`。前回から `sent` が増えたか、`conn_progress`（このストリームは
    /// 接続のクレジットを待っていて、接続のほかのストリームが進んだ）なら数え直す。見回りは 1 秒ごとなので、
    /// 送れなくなってから `secs` 秒を超え、`secs + 2` 秒までに打ち切る
    pub(crate) fn stalled_tick(&mut self, conn_progress: bool) -> bool {
        if self.sent != self.mark || conn_progress {
            self.mark = self.sent;
            self.ticks = 0;
            return false;
        }
        self.ticks = self.ticks.saturating_add(1);
        self.ticks > self.secs
    }

    /// 送れる状態での見回り（数え直す）
    pub(crate) fn clear(&mut self) {
        self.mark = self.sent;
        self.ticks = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// B-487: 送った量が増えない見回りが `secs` 回を超えたら打ち切る。増えたか、接続のクレジットを待つ間に接続が
    /// 進んだら数え直す
    #[cfg(any(feature = "http2", feature = "http3"))]
    #[test]
    fn send_stall_watch_counts_ticks_without_progress() {
        let mut w = SendStallWatch::new();
        w.set_secs(2);
        // 送ってから止まった: 最初の見回りは前回からの進みを見て数え直す
        w.sent = 10;
        assert!(!w.stalled_tick(false));
        assert!(!w.stalled_tick(false));
        assert!(!w.stalled_tick(false));
        assert!(w.stalled_tick(false));
        // 進んだら数え直す
        let mut w = SendStallWatch::new();
        w.set_secs(1);
        assert!(!w.stalled_tick(false));
        w.sent += 1;
        assert!(!w.stalled_tick(false));
        assert!(!w.stalled_tick(false));
        assert!(w.stalled_tick(false));
        // 送れる状態に戻ったら数え直す
        w.clear();
        assert!(!w.stalled_tick(false));
        // 接続のクレジットを待つ間は、接続が進んでいれば打ち切らない
        let mut c = SendStallWatch::new();
        c.set_secs(1);
        for _ in 0..10 {
            assert!(!c.stalled_tick(true));
        }
        assert!(!c.stalled_tick(false));
        assert!(c.stalled_tick(false));
        // 既定は WRITE_TIMEOUT の秒数
        let mut d = SendStallWatch::new();
        for _ in 0..crate::pool::WRITE_TIMEOUT.as_secs() {
            assert!(!d.stalled_tick(false));
        }
        assert!(d.stalled_tick(false));
    }

    /// B-482: ルートの `max_request_header_size` を超えた要求は 431 で、レート制限の枠を使わない
    #[test]
    fn route_header_limit_is_checked_before_rate_limit() {
        let security = SecurityConfig {
            max_request_header_size: 1024,
            rate_limit_requests_per_min: 1,
            ..Default::default()
        };
        let ip = "192.0.2.78";
        for _ in 0..3 {
            let r = check_security(&security, ip, b"GET", 2048, 0, false);
            assert_eq!(r, SecurityCheckResult::HeaderTooLarge);
            assert_eq!(r.status_code(), 431);
            assert!(r.http1_response().unwrap().starts_with(b"HTTP/1.1 431 "));
        }
        assert_eq!(
            check_security(&security, ip, b"GET", 1024, 0, false),
            SecurityCheckResult::Allowed
        );
    }

    /// B-482: veil 全体の上限（大きさ・数・`:path`）
    #[cfg(any(feature = "http2", feature = "http3"))]
    #[test]
    fn request_header_limit_status_by_size_count_and_path() {
        use crate::pool::{MAX_HEADER_COUNT, MAX_HEADER_SIZE};
        assert_eq!(request_header_limit_status(100, 10, 1), None);
        assert_eq!(
            request_header_limit_status(MAX_HEADER_SIZE, MAX_HEADER_COUNT, 1),
            None
        );
        assert_eq!(
            request_header_limit_status(MAX_HEADER_SIZE + 1, 1, 1),
            Some(431)
        );
        assert_eq!(
            request_header_limit_status(100, MAX_HEADER_COUNT + 1, 1),
            Some(431)
        );
        assert_eq!(
            request_header_limit_status(MAX_HEADER_SIZE + 10, 1, MAX_HEADER_SIZE + 1),
            Some(414)
        );
        assert_eq!(request_header_limit_message(414), b"URI Too Long");
    }

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
                check_security(&security, ip, b"POST", 0, 100, false),
                SecurityCheckResult::RequestTooLarge
            );
        }
        // 枠はまだ 1 回分残っている
        assert_eq!(
            check_security(&security, ip, b"POST", 0, 1, false),
            SecurityCheckResult::Allowed
        );
        assert_eq!(
            check_security(&security, ip, b"POST", 0, 1, false),
            SecurityCheckResult::RateLimitExceeded
        );
    }
}
