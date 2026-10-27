//! B-222: HTTP/3 の新しい接続の受け入れ制御。
//!
//! - ワーカーごとの接続数の上限（`[http3] max_connections`）。超えた分の Initial は捨てる。
//! - ハンドシェイク中の接続数が `[http3] retry_threshold` に達したら、トークンの無い Initial には
//!   ステートレスな Retry を返してアドレスを検証する（送信元を偽装した Initial は接続を作れない）。
//! - ハンドシェイクが `[http3] handshake_timeout` までに終わらない接続は捨てる（`max_idle_timeout` まで残さない）。
//!
//! ハンドシェイク中の数は接続の生成・確立・破棄で増減する（全接続を数え直さない。F-151）。
//! 状態はワーカースレッドごと（スレッドローカル）で、Retry のトークンの鍵だけプロセスで共有する。

use std::cell::{Cell, RefCell};
use std::net::SocketAddr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::tls_provider::{hmac, SecureRandom, SystemRandom};

/// Retry のトークンの有効期間（秒）。クライアントは Retry を受けてすぐ Initial を送り直す。
const TOKEN_LIFETIME_SECS: u64 = 10;
/// トークンの認証タグの長さ（HMAC-SHA256 の先頭）。
const TOKEN_TAG_LEN: usize = 16;
/// トークンの最大長（期限 8 + 長さ 1 + 元の DCID 20 + タグ 16）。
const TOKEN_MAX_LEN: usize = 8 + 1 + quiche::MAX_CONN_ID_LEN + TOKEN_TAG_LEN;
/// Retry パケットの最大長（長いヘッダ 7 + 接続 ID 2 本 + トークン + 完全性タグ 16。余裕を持たせる）。
const RETRY_PACKET_MAX: usize = 128;
/// 送る前に溜める Retry の上限（洪水のときに 1 周回で溜まる分を抑える。超えた分は送らない）。
const RETRY_QUEUE_MAX: usize = 1024;

/// 既定値（`config` の `[http3]` と同じ）。
pub const DEFAULT_MAX_CONNECTIONS: usize = 10_000;
pub const DEFAULT_RETRY_THRESHOLD: usize = 1_000;
pub const DEFAULT_HANDSHAKE_TIMEOUT_MS: u64 = 10_000;

/// Retry のトークンの鍵（プロセスで 1 個。SO_REUSEPORT の振り分けが変わっても検証できる）。
static RETRY_TOKEN_KEY: once_cell::sync::Lazy<hmac::Key> = once_cell::sync::Lazy::new(|| {
    let mut seed = [0u8; 32];
    SystemRandom::new()
        .fill(&mut seed)
        .expect("HTTP/3: failed to generate retry token key");
    hmac::Key::new(hmac::HMAC_SHA256, &seed)
});

/// 送り待ちの Retry パケット 1 通。
pub(super) struct RetryPacket {
    pub(super) to: SocketAddr,
    pub(super) len: usize,
    pub(super) buf: [u8; RETRY_PACKET_MAX],
}

struct Admission {
    max_connections: Cell<usize>,
    retry_threshold: Cell<usize>,
    handshake_timeout: Cell<Duration>,
    handshaking: Cell<usize>,
    retries: RefCell<Vec<RetryPacket>>,
}

thread_local! {
    static ADMISSION: Admission = const {
        Admission {
            max_connections: Cell::new(DEFAULT_MAX_CONNECTIONS),
            retry_threshold: Cell::new(DEFAULT_RETRY_THRESHOLD),
            handshake_timeout: Cell::new(Duration::from_millis(DEFAULT_HANDSHAKE_TIMEOUT_MS)),
            handshaking: Cell::new(0),
            retries: RefCell::new(Vec::new()),
        }
    };
}

/// ワーカーの起動時に設定値を入れる（0 の `max_connections` は上限なし、0 の `retry_threshold` は常に Retry）。
pub(super) fn configure(max_connections: usize, retry_threshold: usize, handshake_timeout_ms: u64) {
    ADMISSION.with(|a| {
        a.max_connections.set(if max_connections == 0 {
            usize::MAX
        } else {
            max_connections
        });
        a.retry_threshold.set(retry_threshold);
        a.handshake_timeout
            .set(Duration::from_millis(handshake_timeout_ms.max(1)));
    });
    // 鍵の生成（乱数）を最初の Initial の処理から外す
    once_cell::sync::Lazy::force(&RETRY_TOKEN_KEY);
}

/// 新しい接続を受け入れる前の判定。
pub(super) enum Decision {
    /// そのまま受け入れる（`odcid` は Retry を経た接続の元の DCID）。
    Accept,
    /// Retry を返す（トークンの無い Initial）。
    Retry,
    /// 捨てる（上限・不正なトークン）。
    Drop,
}

/// 未知の接続 ID の Initial をどう扱うか（`has_token` はトークンを持つか）。
pub(super) fn decide(open_connections: usize, has_token: bool) -> Decision {
    ADMISSION.with(|a| {
        if open_connections >= a.max_connections.get() {
            return Decision::Drop;
        }
        if !has_token && a.handshaking.get() >= a.retry_threshold.get() {
            return Decision::Retry;
        }
        Decision::Accept
    })
}

/// ハンドシェイク中の接続 1 本を数える印。確立（`Http3Handler::init_h3`）か破棄で数を戻す。
pub(super) struct HandshakeSlot {
    deadline: Instant,
}

impl HandshakeSlot {
    pub(super) fn new(now: Instant) -> Self {
        let timeout = ADMISSION.with(|a| {
            a.handshaking.set(a.handshaking.get() + 1);
            a.handshake_timeout.get()
        });
        Self {
            deadline: now + timeout,
        }
    }

    /// これを過ぎても確立していなければ接続を捨てる。
    pub(super) fn deadline(&self) -> Instant {
        self.deadline
    }
}

impl Drop for HandshakeSlot {
    fn drop(&mut self) {
        // スレッドの終了でスレッドローカルが先に破棄されていても panic しない
        let _ = ADMISSION.try_with(|a| a.handshaking.set(a.handshaking.get().saturating_sub(1)));
    }
}

/// 試験用: このワーカーのハンドシェイク中の接続数。
#[cfg(test)]
pub(super) fn handshaking() -> usize {
    ADMISSION.with(|a| a.handshaking.get())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn token_tag(peer: &SocketAddr, body: &[u8]) -> [u8; TOKEN_TAG_LEN] {
    let mut ctx = hmac::Context::with_key(&RETRY_TOKEN_KEY);
    ctx.update(b"veil-h3-retry\0");
    match peer {
        SocketAddr::V4(v4) => ctx.update(&v4.ip().octets()),
        SocketAddr::V6(v6) => ctx.update(&v6.ip().octets()),
    }
    ctx.update(&peer.port().to_be_bytes());
    ctx.update(body);
    let tag = ctx.sign();
    let mut out = [0u8; TOKEN_TAG_LEN];
    out.copy_from_slice(&tag.as_ref()[..TOKEN_TAG_LEN]);
    out
}

/// トークンを作る（期限・元の DCID・送信元のアドレスに対するタグ）。返り値は `out` に書いた長さ。
fn mint_token(
    peer: &SocketAddr,
    odcid: &[u8],
    expires: u64,
    out: &mut [u8; TOKEN_MAX_LEN],
) -> usize {
    let n = 8 + 1 + odcid.len();
    out[..8].copy_from_slice(&expires.to_be_bytes());
    out[8] = odcid.len() as u8;
    out[9..n].copy_from_slice(odcid);
    let tag = token_tag(peer, &out[..n]);
    out[n..n + TOKEN_TAG_LEN].copy_from_slice(&tag);
    n + TOKEN_TAG_LEN
}

/// トークンを検証して元の DCID を返す（期限切れ・別の送信元・改ざんは `None`）。
pub(super) fn validate_token<'a>(peer: &SocketAddr, token: &'a [u8]) -> Option<&'a [u8]> {
    validate_token_at(peer, token, now_secs())
}

fn validate_token_at<'a>(peer: &SocketAddr, token: &'a [u8], now: u64) -> Option<&'a [u8]> {
    if token.len() < 8 + 1 + TOKEN_TAG_LEN {
        return None;
    }
    let odcid_len = token[8] as usize;
    if odcid_len > quiche::MAX_CONN_ID_LEN || token.len() != 8 + 1 + odcid_len + TOKEN_TAG_LEN {
        return None;
    }
    let n = 9 + odcid_len;
    let expected = token_tag(peer, &token[..n]);
    // 定数時間で比べる
    let diff = expected
        .iter()
        .zip(&token[n..])
        .fold(0u8, |acc, (a, b)| acc | (a ^ b));
    if diff != 0 {
        return None;
    }
    let mut exp = [0u8; 8];
    exp.copy_from_slice(&token[..8]);
    if u64::from_be_bytes(exp) < now {
        return None;
    }
    Some(&token[9..n])
}

/// Retry パケットを組み立てて送り待ちに積む（送るのは `send_pending_packets`）。
/// `scid`・`dcid`・`version` はクライアントの Initial のもの。
pub(super) fn queue_retry(
    scid: &quiche::ConnectionId<'_>,
    dcid: &quiche::ConnectionId<'_>,
    version: u32,
    new_scid: &quiche::ConnectionId<'_>,
    peer: SocketAddr,
) {
    ADMISSION.with(|a| {
        let mut q = a.retries.borrow_mut();
        if q.len() >= RETRY_QUEUE_MAX {
            return;
        }
        let mut token = [0u8; TOKEN_MAX_LEN];
        let tlen = mint_token(&peer, dcid, now_secs() + TOKEN_LIFETIME_SECS, &mut token);
        let mut pkt = RetryPacket {
            to: peer,
            len: 0,
            buf: [0u8; RETRY_PACKET_MAX],
        };
        match quiche::retry(scid, dcid, new_scid, &token[..tlen], version, &mut pkt.buf) {
            Ok(len) => {
                pkt.len = len;
                q.push(pkt);
            }
            Err(e) => ftlog::debug!("[HTTP/3] failed to build Retry: {}", e),
        }
    });
}

/// 送り待ちの Retry を `f` へ渡して空にする（無ければ何もしない）。
pub(super) fn drain_retries(mut f: impl FnMut(&RetryPacket)) {
    ADMISSION.with(|a| {
        let mut q = a.retries.borrow_mut();
        if q.is_empty() {
            return;
        }
        for p in q.iter() {
            f(p);
        }
        q.clear();
        // 洪水の後に大きな容量を持ち続けない
        if q.capacity() > 64 {
            q.shrink_to(64);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(port: u16) -> SocketAddr {
        SocketAddr::from(([192, 0, 2, 1], port))
    }

    #[test]
    fn token_roundtrip_and_rejections() {
        let odcid = [7u8; 20];
        let mut t = [0u8; TOKEN_MAX_LEN];
        let n = mint_token(&peer(4433), &odcid, 1_000, &mut t);
        assert_eq!(
            validate_token_at(&peer(4433), &t[..n], 999),
            Some(&odcid[..])
        );
        // 期限切れ
        assert_eq!(validate_token_at(&peer(4433), &t[..n], 1_001), None);
        // 別の送信元（ポート違い）
        assert_eq!(validate_token_at(&peer(4434), &t[..n], 999), None);
        // 改ざん（元の DCID・タグ）
        let mut bad = t;
        bad[10] ^= 1;
        assert_eq!(validate_token_at(&peer(4433), &bad[..n], 999), None);
        let mut bad = t;
        bad[n - 1] ^= 1;
        assert_eq!(validate_token_at(&peer(4433), &bad[..n], 999), None);
        // 長さの不整合
        assert_eq!(validate_token_at(&peer(4433), &t[..n - 1], 999), None);
        assert_eq!(validate_token_at(&peer(4433), &[], 999), None);
    }

    #[test]
    fn decide_respects_limits_and_counts_handshakes() {
        configure(2, 1, 1_000);
        assert!(matches!(decide(0, false), Decision::Accept));
        let slot = HandshakeSlot::new(Instant::now());
        assert_eq!(handshaking(), 1);
        // ハンドシェイク中が閾値に達したらトークンの無い Initial は Retry、トークン付きは受け入れる
        assert!(matches!(decide(1, false), Decision::Retry));
        assert!(matches!(decide(1, true), Decision::Accept));
        // 接続数の上限
        assert!(matches!(decide(2, true), Decision::Drop));
        drop(slot);
        assert_eq!(handshaking(), 0);
        assert!(matches!(decide(1, false), Decision::Accept));
        // 0 は上限なし・常に Retry
        configure(0, 0, 1_000);
        assert!(matches!(decide(usize::MAX - 1, false), Decision::Retry));
        configure(
            DEFAULT_MAX_CONNECTIONS,
            DEFAULT_RETRY_THRESHOLD,
            DEFAULT_HANDSHAKE_TIMEOUT_MS,
        );
    }

    #[test]
    fn retry_packet_is_queued_and_drained() {
        let mut dcid = [0u8; 16];
        dcid[0] = 1;
        let scid = [2u8; 16];
        let new_scid = [3u8; quiche::MAX_CONN_ID_LEN];
        queue_retry(
            &quiche::ConnectionId::from_ref(&scid),
            &quiche::ConnectionId::from_ref(&dcid),
            quiche::PROTOCOL_VERSION,
            &quiche::ConnectionId::from_ref(&new_scid),
            peer(5000),
        );
        let mut seen = 0;
        drain_retries(|p| {
            seen += 1;
            assert_eq!(p.to, peer(5000));
            assert!(p.len > 0 && p.len <= RETRY_PACKET_MAX);
            // 送ったパケットは Retry として読める
            let mut b = p.buf;
            let h = quiche::Header::from_slice(&mut b[..p.len], quiche::MAX_CONN_ID_LEN).unwrap();
            assert_eq!(h.ty, quiche::Type::Retry);
        });
        assert_eq!(seen, 1);
        drain_retries(|_| panic!("queue must be empty"));
    }
}
