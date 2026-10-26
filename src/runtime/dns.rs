//! # 上流ホスト名の非同期解決（B-107）
//!
//! `TcpStream::connect_str("host:port")` は従来、ホスト名を `ToSocketAddrs`（同期
//! `getaddrinfo`）でワーカースレッド上で解決していた。DNS が遅い・落ちているとイベントループが
//! 丸ごと止まる（ホットパス絶対規則違反）。
//!
//! - IP アドレスリテラル（`"10.0.0.1:80"` / `"[::1]:443"`）はパースするだけ（syscall なし）。
//! - ホスト名は、ワーカーごとのキャッシュ（`CACHE_TTL`）に当たればそれを、外れたら
//!   `runtime::offload`（専用スレッド）で `getaddrinfo` して結果をキャッシュする。
//!   イベントループは解決を待つ間も他の接続を進める。
//! - 接続は解決したアドレスを順に試す（`connect_each`。B-180）。

use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 解決結果を使い回す時間。上流の DNS レコードの変更に追従できる程度に短くする。
pub const CACHE_TTL: Duration = Duration::from_secs(30);

/// キャッシュの上限（超えたら古いものごと捨てる。上流の数は通常これより十分少ない）。
const CACHE_MAX: usize = 1024;

thread_local! {
    static CACHE: RefCell<HashMap<String, (Arc<[SocketAddr]>, Instant)>> = RefCell::new(HashMap::new());
}

/// 解決結果。IP リテラルは確保せずに 1 個、ホスト名はキャッシュと共有する一覧（`Arc` の複製だけ）。
enum Addrs {
    One(SocketAddr),
    Many(Arc<[SocketAddr]>),
}

impl Addrs {
    fn as_slice(&self) -> &[SocketAddr] {
        match self {
            Addrs::One(sa) => std::slice::from_ref(sa),
            Addrs::Many(v) => v,
        }
    }
}

async fn resolve_all(addr: &str) -> io::Result<Addrs> {
    if let Ok(sa) = addr.parse::<SocketAddr>() {
        return Ok(Addrs::One(sa));
    }
    if let Some(v) = CACHE.with(|c| {
        c.borrow()
            .get(addr)
            .filter(|(_, at)| at.elapsed() < CACHE_TTL)
            .map(|(v, _)| v.clone())
    }) {
        return Ok(Addrs::Many(v));
    }
    let owned = addr.to_string();
    let resolved = crate::runtime::offload::offload(move || resolve_blocking(&owned)).await?;
    CACHE.with(|c| {
        let mut c = c.borrow_mut();
        if c.len() >= CACHE_MAX {
            c.clear();
        }
        c.insert(addr.to_string(), (resolved.clone(), Instant::now()));
    });
    Ok(Addrs::Many(resolved))
}

/// `"host:port"` を解決する（先頭のアドレス）。IP リテラルは即座に、ホスト名はキャッシュか offload で。
pub async fn resolve(addr: &str) -> io::Result<SocketAddr> {
    Ok(resolve_all(addr).await?.as_slice()[0])
}

/// `"host:port"` を解決し、解決したアドレスへ順に `connect` して最初に成功したものを返す。
///
/// B-180: `localhost` が `::1` と `127.0.0.1` に解決され、上流が片方でしか待ち受けていないとき
/// （macOS の /etc/hosts は `::1` が先）、先頭だけに接続すると拒否されて終わっていた。先頭以外で
/// つながったら、そのアドレスをキャッシュの先頭に移して次からは最初に試す（並べ替えはその時だけ）。
/// 待ち時間の上限は呼び出し側の `timeout` が全体に掛かる（Happy Eyeballs はしない）。
pub async fn connect_each<T, F, Fut>(addr: &str, mut connect: F) -> io::Result<T>
where
    F: FnMut(SocketAddr) -> Fut,
    Fut: std::future::Future<Output = io::Result<T>>,
{
    let addrs = resolve_all(addr).await?;
    let list = addrs.as_slice();
    let mut last = None;
    for (i, sa) in list.iter().enumerate() {
        match connect(*sa).await {
            Ok(s) => {
                if i > 0 {
                    prefer(addr, *sa);
                }
                return Ok(s);
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address resolved")))
}

/// キャッシュの `addr` の一覧で `sa` を先頭へ移す。
fn prefer(addr: &str, sa: SocketAddr) {
    CACHE.with(|c| {
        if let Some((v, _)) = c.borrow_mut().get_mut(addr) {
            let mut list: Vec<SocketAddr> = Vec::with_capacity(v.len());
            list.push(sa);
            list.extend(v.iter().copied().filter(|x| *x != sa));
            *v = list.into();
        }
    });
}

/// offload ワーカー内でのみ呼ぶ同期解決。
#[allow(clippy::disallowed_methods)] // offload ワーカー内: イベントループからは `resolve` 経由のみ
fn resolve_blocking(addr: &str) -> io::Result<Arc<[SocketAddr]>> {
    use std::net::ToSocketAddrs;
    let mut list: Vec<SocketAddr> = Vec::new();
    for sa in addr
        .to_socket_addrs()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
    {
        if !list.contains(&sa) {
            list.push(sa);
        }
    }
    if list.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "no address resolved",
        ));
    }
    Ok(list.into())
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

    /// B-180: 先頭のアドレスで拒否されたら次を試し、つながったアドレスを次から先に試す。
    #[test]
    fn connect_each_falls_back_and_prefers_the_working_address() {
        let v6: SocketAddr = "[::1]:9".parse().unwrap();
        let v4: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let list: Arc<[SocketAddr]> = vec![v6, v4].into();
        CACHE.with(|c| {
            c.borrow_mut()
                .insert("b180.test:9".into(), (list, Instant::now()))
        });
        let waker = futures::task::noop_waker_ref();
        let mut cx = std::task::Context::from_waker(waker);
        let mut tried = Vec::new();
        {
            let fut = connect_each("b180.test:9", |sa| {
                tried.push(sa);
                std::future::ready(if sa.is_ipv4() {
                    Ok(sa)
                } else {
                    Err(io::Error::from(io::ErrorKind::ConnectionRefused))
                })
            });
            let mut fut = std::pin::pin!(fut);
            match std::future::Future::poll(fut.as_mut(), &mut cx) {
                std::task::Poll::Ready(Ok(sa)) => assert_eq!(sa, v4),
                _ => panic!("expected the IPv4 address"),
            }
        }
        assert_eq!(tried, vec![v6, v4]);
        CACHE.with(|c| assert_eq!(c.borrow()["b180.test:9"].0[..], [v4, v6]));
        // 全部だめなら最後のエラー
        let fut = connect_each("b180.test:9", |_| {
            std::future::ready(Err::<SocketAddr, _>(io::Error::from(
                io::ErrorKind::ConnectionRefused,
            )))
        });
        let mut fut = std::pin::pin!(fut);
        match std::future::Future::poll(fut.as_mut(), &mut cx) {
            std::task::Poll::Ready(Err(e)) => {
                assert_eq!(e.kind(), io::ErrorKind::ConnectionRefused)
            }
            _ => panic!("expected an error"),
        }
    }
}
