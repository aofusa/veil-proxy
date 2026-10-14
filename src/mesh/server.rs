//! メッシュのワーカー・リスナー・接続の振り分け（F-216）。
//!
//! - ワーカー（スレッド）ごとに veil のランタイムを 1 本動かす。ポートを開くリスナー（Envoy の
//!   `bind_to_port`。virtualOutbound・virtualInbound・ブートストラップの静的リスナー）は LDS の更新に
//!   合わせて各ワーカーが `SO_REUSEPORT` で開き直す（`server::create_listener`）。
//! - 接続ごとに元の宛先（`SO_ORIGINAL_DST`）→ リスナー → フィルタチェーン（Envoy と同じ選択順）→
//!   TLS の終端 → HTTP / L4 の処理。接続の受理ごとにリスナーの表を 1 回 `load` するだけ。

use super::detect::{self, Detected};
use super::l7::ConnCtx;
use super::model::*;
use super::state::state;
use crate::runtime::handle::AsRawFd;
use crate::runtime::tcp::TcpStream;
use once_cell::sync::OnceCell;
use std::cell::Cell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

/// 下流のストリーム（平文も TLS も同じ型。`accept_plain` は先読みした分を持つ）。
#[cfg(veil_ktls)]
pub type DownStream = crate::ktls_rustls::KtlsServerStream;
#[cfg(not(veil_ktls))]
pub type DownStream = crate::simple_tls::SimpleTlsServerStream;

/// ブートストラップの静的リスナー（15021・15090 等）。
pub static STATIC_LISTENERS: OnceCell<Vec<Arc<MeshListener>>> = OnceCell::new();

static WORKERS: OnceCell<usize> = OnceCell::new();

/// ワーカーを起動し、停止まで戻らない。
pub fn run(workers: usize) {
    let workers = workers.max(1);
    let _ = WORKERS.set(workers);
    let mut handles = Vec::new();
    for id in 0..workers {
        let h = std::thread::Builder::new()
            .name(format!("veil-mesh-{}", id))
            .spawn(move || {
                crate::runtime::block_on(worker(id, workers));
                crate::runtime::shutdown_thread();
            });
        match h {
            Ok(h) => handles.push(h),
            Err(e) => ftlog::error!("[mesh] cannot spawn worker {}: {}", id, e),
        }
    }
    for h in handles {
        let _ = h.join();
    }
}

struct Active {
    stop: Rc<Cell<bool>>,
}

/// 開くべきリスナーのアドレス（静的 + LDS の bind_to_port）。
fn desired_listeners() -> Vec<SocketAddr> {
    let mut v: Vec<SocketAddr> = STATIC_LISTENERS
        .get()
        .map(|l| l.iter().map(|x| x.addr).collect())
        .unwrap_or_default();
    for l in &state().listeners.load().bound {
        v.push(l.addr);
    }
    v.sort();
    v.dedup();
    v
}

/// アドレスに対応する現在のリスナー。
fn listener_for(addr: SocketAddr) -> Option<Arc<MeshListener>> {
    if let Some(l) = STATIC_LISTENERS
        .get()
        .and_then(|v| v.iter().find(|l| l.addr == addr))
    {
        return Some(l.clone());
    }
    state()
        .listeners
        .load()
        .bound
        .iter()
        .find(|l| l.addr == addr)
        .cloned()
}

async fn worker(id: usize, workers: usize) {
    let mut active: HashMap<SocketAddr, Active> = HashMap::new();
    let mut announced = false;
    loop {
        if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            for a in active.values() {
                a.stop.set(true);
            }
            break;
        }
        let want = desired_listeners();
        for addr in &want {
            if active.contains_key(addr) {
                continue;
            }
            match crate::server::create_listener(
                *addr,
                crate::config::ReuseportBalancing::Kernel,
                workers,
                id,
            ) {
                Ok(l) => {
                    let stop = Rc::new(Cell::new(false));
                    crate::runtime::spawn(accept_loop(l, *addr, stop.clone()));
                    active.insert(*addr, Active { stop });
                    ftlog::debug!("[mesh] worker {} listening on {}", id, addr);
                }
                Err(e) => {
                    ftlog::warn!("[mesh] worker {}: cannot listen on {}: {}", id, addr, e);
                }
            }
        }
        active.retain(|addr, a| {
            if want.contains(addr) {
                true
            } else {
                a.stop.set(true);
                false
            }
        });
        if !announced && !want.is_empty() {
            super::stats::WORKERS_STARTED.fetch_add(1, Ordering::Relaxed);
            announced = true;
        }
        crate::runtime::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn accept_loop(l: crate::runtime::TcpListener, addr: SocketAddr, stop: Rc<Cell<bool>>) {
    let pool = crate::runtime::TaskPool::new();
    loop {
        if stop.get() || crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            return;
        }
        match crate::runtime::timeout(Duration::from_secs(1), l.accept()).await {
            Ok(Ok((stream, peer))) => {
                pool.spawn(handle(stream, peer, addr));
            }
            Ok(Err(e)) => {
                ftlog::debug!("[mesh] accept on {}: {}", addr, e);
            }
            Err(_) => crate::alloc_tune::idle_tick(),
        }
    }
}

/// Envoy のフィルタチェーンの選択（宛先ポート → 宛先 IP → SNI → トランスポート → ALPN → 送信元）。
pub fn select_chain<'a>(l: &'a MeshListener, f: &ConnFacts<'_>) -> Option<&'a Arc<FilterChain>> {
    let mut cands: Vec<&Arc<FilterChain>> = l.chains.iter().collect();
    // 宛先ポート
    let port = f.dst.map(|d| d.port());
    let exact: Vec<_> = cands
        .iter()
        .copied()
        .filter(|c| c.m.dst_port.is_some() && c.m.dst_port == port)
        .collect();
    cands = if exact.is_empty() {
        cands
            .into_iter()
            .filter(|c| c.m.dst_port.is_none())
            .collect()
    } else {
        exact
    };
    // 宛先 IP（最長一致）
    if let Some(dst) = f.dst {
        let best = cands
            .iter()
            .filter_map(|c| {
                c.m.prefix_ranges
                    .iter()
                    .filter(|r| r.contains(dst.ip()))
                    .map(|r| r.len)
                    .max()
            })
            .max();
        cands = match best {
            Some(len) => cands
                .into_iter()
                .filter(|c| {
                    c.m.prefix_ranges
                        .iter()
                        .any(|r| r.len == len && r.contains(dst.ip()))
                })
                .collect(),
            None => cands
                .into_iter()
                .filter(|c| c.m.prefix_ranges.is_empty())
                .collect(),
        };
    } else {
        cands.retain(|c| c.m.prefix_ranges.is_empty());
    }
    // SNI（完全一致 → ワイルドカード → 指定なし）
    if let Some(sni) = f.sni {
        let exact: Vec<_> = cands
            .iter()
            .copied()
            .filter(|c| c.m.server_names.iter().any(|n| n == sni))
            .collect();
        cands = if !exact.is_empty() {
            exact
        } else {
            let wild: Vec<_> = cands
                .iter()
                .copied()
                .filter(|c| {
                    c.m.server_names
                        .iter()
                        .any(|n| n.starts_with("*.") && sni.ends_with(&n[1..]))
                })
                .collect();
            if !wild.is_empty() {
                wild
            } else {
                cands
                    .into_iter()
                    .filter(|c| c.m.server_names.is_empty())
                    .collect()
            }
        };
    } else {
        cands.retain(|c| c.m.server_names.is_empty());
    }
    // トランスポート
    let exact: Vec<_> = cands
        .iter()
        .copied()
        .filter(|c| c.m.transport.is_some() && c.m.transport == f.transport)
        .collect();
    cands = if exact.is_empty() {
        cands
            .into_iter()
            .filter(|c| c.m.transport.is_none())
            .collect()
    } else {
        exact
    };
    // ALPN（検出したプロトコル）
    let exact: Vec<_> = cands
        .iter()
        .copied()
        .filter(|c| {
            !c.m.application_protocols.is_empty()
                && c.m
                    .application_protocols
                    .iter()
                    .any(|p| f.alpn.iter().any(|a| a == p))
        })
        .collect();
    cands = if exact.is_empty() {
        cands
            .into_iter()
            .filter(|c| c.m.application_protocols.is_empty())
            .collect()
    } else {
        exact
    };
    // 送信元
    if let Some(src) = f.src {
        let best = cands
            .iter()
            .filter_map(|c| {
                c.m.source_prefix_ranges
                    .iter()
                    .filter(|r| r.contains(src.ip()))
                    .map(|r| r.len)
                    .max()
            })
            .max();
        cands = match best {
            Some(len) => cands
                .into_iter()
                .filter(|c| {
                    c.m.source_prefix_ranges
                        .iter()
                        .any(|r| r.len == len && r.contains(src.ip()))
                })
                .collect(),
            None => cands
                .into_iter()
                .filter(|c| c.m.source_prefix_ranges.is_empty())
                .collect(),
        };
        let exact: Vec<_> = cands
            .iter()
            .copied()
            .filter(|c| c.m.source_ports.contains(&src.port()))
            .collect();
        cands = if exact.is_empty() {
            cands
                .into_iter()
                .filter(|c| c.m.source_ports.is_empty())
                .collect()
        } else {
            exact
        };
    }
    cands.first().copied().or(l.default_chain.as_ref())
}

async fn tls_accept(
    stream: TcpStream,
    tls: &DownstreamTls,
    initial: Vec<u8>,
    client_alpn: &[String],
) -> std::io::Result<(DownStream, Option<Vec<u8>>)> {
    let server_config = tls.config_for(client_alpn);
    let fut = async {
        #[cfg(veil_ktls)]
        {
            let c = crate::config::CURRENT_CONFIG.load();
            crate::ktls_rustls::accept_with_peer_cert(
                stream,
                server_config,
                c.ktls_config.enabled,
                c.ktls_config.fallback_enabled,
                c.ktls_config.tcp_cork_enabled,
                Some(initial),
            )
            .await
        }
        #[cfg(not(veil_ktls))]
        {
            crate::simple_tls::accept_with_peer_cert(
                stream,
                tls.server_config.clone(),
                Some(initial),
            )
            .await
        }
    };
    match crate::runtime::timeout(Duration::from_secs(15), fut).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "TLS handshake timeout",
        )),
    }
}

async fn plain(stream: TcpStream, initial: Vec<u8>) -> std::io::Result<DownStream> {
    #[cfg(veil_ktls)]
    {
        crate::ktls_rustls::accept_plain(stream, Some(initial)).await
    }
    #[cfg(not(veil_ktls))]
    {
        crate::simple_tls::accept_plain(stream, Some(initial)).await
    }
}

/// 接続 1 本の処理。
async fn handle(stream: TcpStream, peer: SocketAddr, laddr: SocketAddr) {
    let _cx = super::stats::CxGuard::new();
    let _ = stream.set_nodelay(true);
    let Some(lst) = listener_for(laddr) else {
        return;
    };
    if state().is_draining(lst.direction) {
        return;
    }
    let fd = stream.as_raw_fd() as i32;
    let local = super::net::local_addr(fd).unwrap_or(laddr);
    let dst = if lst.use_original_dst || lst.direction == Direction::Inbound {
        super::net::original_dst(fd).unwrap_or(local)
    } else {
        local
    };
    // virtualOutbound は元の宛先に対応するリスナーへ渡す
    let table = state().listeners.load();
    let target: Arc<MeshListener> = if lst.use_original_dst {
        table.find_original_dst(dst).cloned().unwrap_or(lst)
    } else {
        lst
    };
    drop(table);
    let (det, initial) = if target.needs_sniff {
        detect::sniff(&stream, None).await
    } else {
        (Detected::Other, Vec::new())
    };
    if det == Detected::Closed {
        return;
    }
    let (transport, sni, alpn): (TransportProto, Option<String>, Vec<String>) = match &det {
        Detected::Tls { sni, alpn } => (TransportProto::Tls, sni.clone(), alpn.clone()),
        Detected::Http1 if target.http_inspector => {
            (TransportProto::RawBuffer, None, vec!["http/1.1".into()])
        }
        Detected::H2c if target.http_inspector => {
            (TransportProto::RawBuffer, None, vec!["h2c".into()])
        }
        _ => (TransportProto::RawBuffer, None, Vec::new()),
    };
    let alpn_refs: Vec<&str> = alpn.iter().map(|s| s.as_str()).collect();
    let facts = ConnFacts {
        dst: Some(dst),
        src: Some(peer),
        sni: sni.as_deref(),
        transport: Some(transport),
        alpn: &alpn_refs,
    };
    let Some(chain) = select_chain(&target, &facts).cloned() else {
        ftlog::debug!("[mesh] no filter chain for {} on {}", dst, target.name);
        return;
    };
    match &chain.action {
        ChainAction::Close => {}
        ChainAction::Http(hcm) => {
            let hcm = hcm.clone();
            serve_http(
                stream,
                initial,
                det,
                &chain,
                hcm,
                peer,
                dst,
                target.direction,
                sni,
                &alpn,
            )
            .await;
        }
        ChainAction::Tcp(tp) => {
            let tp = tp.clone();
            serve_tcp(
                stream,
                initial,
                &chain,
                tp,
                peer,
                dst,
                target.direction,
                sni,
                &alpn,
            )
            .await;
        }
    }
}

fn principal_of(cert: &Option<Vec<u8>>) -> (Option<Arc<str>>, Option<Arc<str>>, Vec<String>) {
    let Some(c) = cert else {
        return (None, None, Vec::new());
    };
    let sans = super::identity::parse_sans(c);
    let uri = sans.iter().find_map(|s| match s {
        super::identity::San::Uri(u) => Some(Arc::<str>::from(u.as_str())),
        _ => None,
    });
    let dns = sans
        .iter()
        .filter_map(|s| match s {
            super::identity::San::Dns(d) => Some(d.clone()),
            _ => None,
        })
        .collect();
    (
        uri,
        Some(Arc::from(super::identity::cert_sha256_hex(c).as_str())),
        dns,
    )
}

#[allow(clippy::too_many_arguments)]
async fn serve_http(
    stream: TcpStream,
    initial: Vec<u8>,
    det: Detected,
    chain: &FilterChain,
    hcm: Arc<HttpCm>,
    peer: SocketAddr,
    dst: SocketAddr,
    direction: Direction,
    sni: Option<String>,
    client_alpn: &[String],
) {
    let direction = if hcm.direction == Direction::Unspecified {
        direction
    } else {
        hcm.direction
    };
    let (ds, peer_cert, h2) = match &chain.tls {
        Some(tls) => match tls_accept(stream, tls, initial, client_alpn).await {
            Ok((s, pc)) => {
                let h2 = s.alpn_protocol() == Some(b"h2");
                (s, pc, h2)
            }
            Err(e) => {
                ftlog::debug!("[mesh] TLS handshake from {} failed: {}", peer, e);
                return;
            }
        },
        None => match plain(stream, initial).await {
            Ok(s) => (s, None, det == Detected::H2c),
            Err(_) => return,
        },
    };
    let h2 = match hcm.codec {
        Codec::Http2 => true,
        Codec::Http1 => false,
        Codec::Auto => h2,
    };
    let (principal, hash, dns) = principal_of(&peer_cert);
    let ctx = Arc::new(ConnCtx {
        hcm,
        direction,
        orig_dst: Some(dst),
        peer,
        tls: chain.tls.is_some(),
        peer_principal: principal,
        peer_cert_hash: hash,
        peer_dns: dns,
        sni,
    });
    if h2 {
        super::h2::serve(ds, Vec::new(), ctx).await;
    } else {
        super::http1::serve(ds, Vec::new(), ctx).await;
    }
}

#[allow(clippy::too_many_arguments)]
async fn serve_tcp(
    stream: TcpStream,
    initial: Vec<u8>,
    chain: &FilterChain,
    tp: Arc<TcpProxyCfg>,
    peer: SocketAddr,
    dst: SocketAddr,
    direction: Direction,
    sni: Option<String>,
    client_alpn: &[String],
) {
    let (mut ds, peer_cert) = match &chain.tls {
        Some(tls) => match tls_accept(stream, tls, initial, client_alpn).await {
            Ok(x) => x,
            Err(e) => {
                ftlog::debug!("[mesh] TLS handshake from {} failed: {}", peer, e);
                return;
            }
        },
        None => match plain(stream, initial).await {
            Ok(s) => (s, None),
            Err(_) => return,
        },
    };
    let (principal, _, _) = principal_of(&peer_cert);
    if let Some(r) = &tp.rbac {
        let header = |_: &str| None;
        let md = |_: &str, _: &[String]| None;
        let input = super::xds::rbac::RbacInput {
            header: &header,
            path: b"",
            dst: Some(dst),
            sni: sni.as_deref(),
            principal: principal.as_deref(),
            direct_remote: Some(peer.ip()),
            remote: None,
            metadata: &md,
            http: false,
        };
        if !super::xds::rbac::allowed(r, &input) {
            ftlog::debug!("[mesh] TCP RBAC denied {} -> {}", peer, dst);
            return;
        }
    }
    let Some(cluster) = tp.pick().cloned() else {
        return;
    };
    let picked = match super::upstream::pick(&cluster, Some(dst), None, &[]) {
        Ok(p) => p,
        Err(_) => return,
    };
    let labels = super::stats::ReqLabels {
        reporter: if direction == Direction::Inbound {
            super::stats::Reporter::Destination
        } else {
            super::stats::Reporter::Source
        },
        peer: picked
            .endpoint()
            .and_then(|e| super::peer::cached_endpoint(&e.workload)),
        peer_principal: principal.clone(),
        service: picked.cfg.svc_labels.clone(),
        proto: super::stats::Proto::Tcp,
        mtls: chain.tls.is_some() && principal.is_some(),
    };
    let stats = chain.stats;
    if stats {
        super::stats::record_tcp(
            &labels,
            true,
            false,
            0,
            0,
            super::httpcore::ResponseFlag::None,
        );
    }
    picked.begin();
    let mut up = match super::upstream::connect(&picked, super::upstream::Alpn::Tcp).await {
        Ok(u) => u,
        Err(e) => {
            picked.end(false, true);
            ftlog::debug!("[mesh] tcp connect to {} failed: {}", picked.addr, e);
            if stats {
                super::stats::record_tcp(
                    &labels,
                    false,
                    true,
                    0,
                    0,
                    super::httpcore::ResponseFlag::UpstreamConnectFailure,
                );
            }
            return;
        }
    };
    let idle = tp.idle_timeout.unwrap_or(Duration::from_secs(3600));
    let (sent, received) = super::tcp::relay(&mut ds, &mut up, idle).await;
    picked.end(false, false);
    if stats {
        super::stats::record_tcp(
            &labels,
            false,
            true,
            received,
            sent,
            super::httpcore::ResponseFlag::None,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chain(name: &str, m: ChainMatch) -> Arc<FilterChain> {
        Arc::new(FilterChain {
            name: name.into(),
            m,
            tls: None,
            action: ChainAction::Close,
            stats: false,
        })
    }

    #[test]
    fn selects_like_envoy_virtual_inbound() {
        let l = MeshListener {
            name: "virtualInbound".into(),
            addr: "0.0.0.0:15006".parse().unwrap(),
            use_original_dst: false,
            bind_to_port: true,
            direction: Direction::Inbound,
            chains: vec![
                chain(
                    "blackhole",
                    ChainMatch {
                        dst_port: Some(15006),
                        ..Default::default()
                    },
                ),
                chain(
                    "catchall-http-tls",
                    ChainMatch {
                        transport: Some(TransportProto::Tls),
                        application_protocols: vec!["istio-http/1.1".into(), "istio-h2".into()],
                        ..Default::default()
                    },
                ),
                chain(
                    "catchall-http-plain",
                    ChainMatch {
                        transport: Some(TransportProto::RawBuffer),
                        application_protocols: vec!["http/1.1".into(), "h2c".into()],
                        ..Default::default()
                    },
                ),
                chain(
                    "tcp-tls",
                    ChainMatch {
                        transport: Some(TransportProto::Tls),
                        ..Default::default()
                    },
                ),
                chain(
                    "tcp-plain",
                    ChainMatch {
                        transport: Some(TransportProto::RawBuffer),
                        ..Default::default()
                    },
                ),
                chain(
                    "8080-tls",
                    ChainMatch {
                        dst_port: Some(8080),
                        transport: Some(TransportProto::Tls),
                        application_protocols: vec!["istio".into(), "istio-http/1.1".into()],
                        ..Default::default()
                    },
                ),
                chain(
                    "8080-plain",
                    ChainMatch {
                        dst_port: Some(8080),
                        transport: Some(TransportProto::RawBuffer),
                        ..Default::default()
                    },
                ),
            ],
            default_chain: None,
            tls_inspector: true,
            http_inspector: true,
            needs_sniff: true,
        };
        let f = |port: u16, t: TransportProto, alpn: &'static [&'static str]| ConnFacts {
            dst: Some(SocketAddr::new("10.0.0.5".parse().unwrap(), port)),
            src: Some("10.0.0.9:1234".parse().unwrap()),
            sni: None,
            transport: Some(t),
            alpn,
        };
        assert_eq!(
            select_chain(
                &l,
                &f(8080, TransportProto::Tls, &["istio-http/1.1", "istio"])
            )
            .unwrap()
            .name,
            "8080-tls"
        );
        assert_eq!(
            select_chain(&l, &f(8080, TransportProto::RawBuffer, &["http/1.1"]))
                .unwrap()
                .name,
            "8080-plain"
        );
        assert_eq!(
            select_chain(&l, &f(9000, TransportProto::Tls, &["istio-h2"]))
                .unwrap()
                .name,
            "catchall-http-tls"
        );
        assert_eq!(
            select_chain(&l, &f(9000, TransportProto::Tls, &["istio"]))
                .unwrap()
                .name,
            "tcp-tls"
        );
        assert_eq!(
            select_chain(&l, &f(9000, TransportProto::RawBuffer, &[]))
                .unwrap()
                .name,
            "tcp-plain"
        );
        assert_eq!(
            select_chain(&l, &f(15006, TransportProto::RawBuffer, &[]))
                .unwrap()
                .name,
            "blackhole"
        );
    }
}
