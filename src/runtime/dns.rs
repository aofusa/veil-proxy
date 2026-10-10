//! # 上流ホスト名の非同期解決（B-107）
//!
//! `TcpStream::connect_str("host:port")` は従来、ホスト名を `ToSocketAddrs`（同期
//! `getaddrinfo`）でワーカースレッド上で解決していた。DNS が遅い・落ちているとイベントループが
//! 丸ごと止まる（ホットパス絶対規則違反）。
//!
//! - IP アドレスリテラル（`"10.0.0.1:80"` / `"[::1]:443"`）はパースするだけ（syscall なし）。
//! - ホスト名は、ワーカーごとのキャッシュ（[`CACHE_TTL`]）に当たればそれを、外れたら
//!   `runtime::offload`（専用スレッド）で `getaddrinfo` して結果をキャッシュする。
//!   イベントループは解決を待つ間も他の接続を進める。

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

/// 解決結果を使い回す時間。上流の DNS レコードの変更に追従できる程度に短くする。
pub const CACHE_TTL: Duration = Duration::from_secs(30);

/// キャッシュの上限（超えたら古いものごと捨てる。上流の数は通常これより十分少ない）。
const CACHE_MAX: usize = 1024;

thread_local! {
    static CACHE: RefCell<HashMap<String, (SocketAddr, Instant)>> = RefCell::new(HashMap::new());
}

/// `"host:port"` を解決する。IP リテラルは即座に、ホスト名はキャッシュか offload で。
pub async fn resolve(addr: &str) -> io::Result<SocketAddr> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Ok(sa);
    }
    if let Some(sa) = CACHE.with(|c| {
        c.borrow()
            .get(addr)
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(sa, _)| *sa)
    }) {
        return Ok(sa);
    }
    let owned = addr.to_string();
    let resolved = crate::runtime::offload::offload(move || resolve_blocking(&owned)).await?;
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_MAX {
            c.clear();
        }
        c.insert(addr.to_string(), (resolved, Instant::now()));
    });
    Ok(resolved)
}

/// offload ワーカー内でのみ呼ぶ同期解決。
#[allow(clippy::disallowed_methods)] // offload ワーカー内: イベントループからは `resolve` 経由のみ
fn resolve_blocking(addr: &str) -> io::Result<SocketAddr> {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
        .next()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address resolved"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// IP リテラルはキャッシュにも offload にも行かずにそのまま返る（ランタイム不要）。
    #[test]
    fn ip_literals_parse_without_lookup() {
        let fut = resolve("127.0.0.1:8080");
        let mut fut = std::pin::pin!(fut);
        let waker = futures::task::noop_waker_ref();
        let mut cx = std::task::Context::from_waker(waker);
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Ready(Ok(sa)) => assert_eq!(sa, "127.0.0.1:8080".parse().unwrap()),
            other => panic!("expected immediate result, got {:?}", other.is_ready()),
        }
        let fut = resolve("[::1]:443");
        let mut fut = std::pin::pin!(fut);
        assert!(std::future::Future::poll(fut.as_mut(), &mut cx).is_ready());
        CACHE.with(|c| assert!(c.borrow().is_empty()));
    }
}
