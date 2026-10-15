//! Linkerd の inbound（4143）と outbound（4140）の接続の処理（F-223）。
//!
//! - outbound: 元の宛先のポリシー（[`super::discovery::outbound`]）で HTTP なら HTTP の処理（ルート・
//!   バックエンドの選択・相手のプロキシへの mTLS・HTTP/2 への昇格）、それ以外は L4 で転送する。
//! - inbound: 自分の ID を SNI にした TLS は終端し（相手の ID は `l5d-client-id`）、ALPN が
//!   `transport.l5d.io/v1` なら最初の transport header で本来のポートを知る。ポートのポリシーの認可を
//!   見て、アプリ（元の宛先）へ送る。それ以外（平文・アプリ自身の TLS）はそのまま。

use super::discovery::{self, InProto, OutCfg};
use super::env::Config;
use super::proto as p;
use crate::mesh::detect::{self, Detected};
use crate::mesh::l7::ConnCtx;
use crate::mesh::model::*;
use crate::mesh::server::DownStream;
use crate::runtime::handle::AsRawFd;
use crate::runtime::io::AsyncReadRent;
use crate::runtime::tcp::TcpStream;
use once_cell::sync::OnceCell;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub static CONFIG: OnceCell<Arc<Config>> = OnceCell::new();

/// プロトコルの検出の待ち（本家の既定と同じ 10 秒。サーバが先に話すプロトコルは opaque に倒す）
const DETECT_TIMEOUT: Duration = Duration::from_secs(10);

/// ワーカーが受けた接続（`laddr` で inbound / outbound を分ける）。
pub async fn handle(stream: TcpStream, peer: SocketAddr, laddr: SocketAddr) {
    let _cx = crate::mesh::stats::CxGuard::new();
    let _ = stream.set_nodelay(true);
    let Some(cfg) = CONFIG.get().cloned() else {
        return;
    };
    let fd = stream.as_raw_fd();
    // 宛先は SO_ORIGINAL_DST（getsockopt 1 本）。取れないときだけ getsockname
    let dst = crate::mesh::net::original_dst(fd)
        .unwrap_or_else(|| crate::mesh::net::local_addr(fd).unwrap_or(laddr));
    if laddr.port() == cfg.outbound_addr.port() {
        outbound(&cfg, stream, peer, dst).await;
    } else if laddr.port() == cfg.admin_addr.port() {
        super::admin::serve(&cfg, stream, peer).await;
    } else if laddr.port() == cfg.control_addr.port() {
        super::tap::serve(&cfg, stream, peer).await;
    } else {
        inbound(&cfg, stream, peer, dst).await;
    }
}

fn ctx(
    hcm: Arc<HttpCm>,
    direction: Direction,
    dst: SocketAddr,
    peer: SocketAddr,
    client_id: Option<String>,
) -> Arc<ConnCtx> {
    Arc::new(ConnCtx {
        hcm,
        direction,
        orig_dst: Some(dst),
        peer,
        tls: client_id.is_some(),
        peer_principal: client_id.as_deref().map(Arc::from),
        peer_cert_hash: None,
        peer_dns: client_id.into_iter().collect(),
        sni: None,
        relookup: None,
        table_version: 0,
        // Linkerd は自分のメトリクス（linkerd::metrics）を数える
        stats: false,
    })
}

async fn serve_http(ds: DownStream, initial: Vec<u8>, h2: bool, c: Arc<ConnCtx>) {
    if h2 {
        crate::mesh::h2::serve(ds, initial, c).await;
    } else {
        crate::mesh::http1::serve(ds, initial, c).await;
    }
}

/// L4 で転送する（`dst` は元の宛先。ORIGINAL_DST のクラスタはそこへ、それ以外はバックエンドへ）。
async fn relay(mut ds: DownStream, tp: &TcpProxyCfg, dst: SocketAddr) {
    let Some(slot) = tp.pick().cloned() else {
        return;
    };
    let picked = match crate::mesh::upstream::pick(&slot, Some(dst), None, &[]) {
        Ok(p) => p,
        Err(_) => return,
    };
    picked.begin();
    let mut up =
        match crate::mesh::upstream::connect(&picked, crate::mesh::upstream::Alpn::Tcp).await {
            Ok(u) => u,
            Err(e) => {
                picked.end(false, true);
                ftlog::debug!("[linkerd] tcp connect to {} failed: {}", picked.addr, e);
                return;
            }
        };
    let _ = crate::mesh::tcp::relay(&mut ds, &mut up, Duration::from_secs(3600)).await;
    picked.end(false, false);
}

async fn outbound(cfg: &Config, stream: TcpStream, peer: SocketAddr, dst: SocketAddr) {
    // 自分自身への接続（ループ）は受けない
    if dst.port() == cfg.outbound_addr.port() && dst.ip().is_loopback() {
        return;
    }
    let oc = discovery::outbound(dst).await;
    let (det, initial) = match oc.as_deref() {
        Some(OutCfg::Http { .. }) => detect::sniff(&stream, Some(DETECT_TIMEOUT)).await,
        _ => (Detected::Other, Vec::new()),
    };
    if det == Detected::Closed {
        return;
    }
    let Ok(ds) = crate::mesh::server::plain(stream, initial).await else {
        return;
    };
    match oc.as_deref() {
        Some(OutCfg::Http {
            hcm,
            detect: _,
            opaque,
        }) => match det {
            Detected::Http1 | Detected::H2c => {
                let c = ctx(hcm.clone(), Direction::Outbound, dst, peer, None);
                serve_http(ds, Vec::new(), det == Detected::H2c, c).await;
            }
            _ => {
                let tp = opaque.clone().unwrap_or_else(|| discovery::app_tcp(cfg));
                relay(ds, &tp, dst).await;
            }
        },
        Some(OutCfg::Opaque(tp)) => relay(ds, tp, dst).await,
        // ポリシーが届かなかった: 元の宛先へそのまま
        None => relay(ds, &discovery::app_tcp(cfg), dst).await,
    }
}

/// TLS を終端した後の先頭を読んで分類する。
pub(super) async fn read_classify(ds: &mut DownStream, mut acc: Vec<u8>) -> (Detected, Vec<u8>) {
    loop {
        if let Some(d) = detect::classify(&acc) {
            return (d, acc);
        }
        if acc.len() > 64 * 1024 {
            return (Detected::Other, acc);
        }
        let buf: Vec<u8> = Vec::with_capacity(4096);
        let (r, buf) = match crate::runtime::timeout(DETECT_TIMEOUT, ds.read(buf)).await {
            Ok(r) => r,
            Err(_) => return (Detected::Other, acc),
        };
        match r {
            Ok(0) | Err(_) => {
                let d = if acc.is_empty() {
                    Detected::Closed
                } else {
                    Detected::Other
                };
                return (d, acc);
            }
            Ok(n) => acc.extend_from_slice(&buf[..n]),
        }
    }
}

/// opaque transport の transport header を読む（残りのバイトも返す）。
pub(super) async fn read_transport_header(
    ds: &mut DownStream,
) -> Option<(p::TransportHeader, Vec<u8>)> {
    let mut acc: Vec<u8> = Vec::new();
    loop {
        match p::TransportHeader::decode_prefaced(&acc) {
            Ok(Some((h, used))) => return Some((h, acc.split_off(used))),
            Ok(None) => {}
            Err(e) => {
                ftlog::debug!("[linkerd] transport header: {}", e);
                return None;
            }
        }
        let buf: Vec<u8> = Vec::with_capacity(1024);
        let (r, buf) = crate::runtime::timeout(Duration::from_secs(10), ds.read(buf))
            .await
            .ok()?;
        match r {
            Ok(0) | Err(_) => return None,
            Ok(n) => acc.extend_from_slice(&buf[..n]),
        }
    }
}

/// 自分の ID の TLS を終端する（相手の ID は DNS の SAN）。
pub(super) async fn accept_tls(
    stream: TcpStream,
    initial: Vec<u8>,
    peer: SocketAddr,
) -> Option<(DownStream, Option<String>)> {
    let accept = async {
        #[cfg(veil_ktls)]
        {
            crate::ktls_rustls::accept_with_peer_cert(
                stream,
                server_config(),
                false,
                true,
                false,
                Some(initial),
            )
            .await
        }
        #[cfg(not(veil_ktls))]
        {
            crate::simple_tls::accept_with_peer_cert(stream, server_config(), Some(initial)).await
        }
    };
    let (ds, cert) = match crate::runtime::timeout(Duration::from_secs(10), accept).await {
        Ok(Ok(x)) => x,
        Ok(Err(e)) => {
            ftlog::debug!("[linkerd] TLS from {} failed: {}", peer, e);
            return None;
        }
        Err(_) => return None,
    };
    let client_id = cert.as_deref().and_then(|c| {
        crate::mesh::identity::parse_sans(c)
            .into_iter()
            .find_map(|s| match s {
                crate::mesh::identity::San::Dns(d) => Some(d),
                _ => None,
            })
    });
    Some((ds, client_id))
}

fn server_config() -> Arc<rustls::ServerConfig> {
    static CFG: OnceCell<Arc<rustls::ServerConfig>> = OnceCell::new();
    CFG.get_or_init(|| {
        crate::mesh::identity::server_config(
            super::CERT,
            Some(super::ROOTS),
            Vec::new(),
            false,
            &[p::TRANSPORT_ALPN.to_vec()],
        )
        .expect("linkerd server TLS config")
    })
    .clone()
}

async fn inbound(cfg: &Config, stream: TcpStream, peer: SocketAddr, dst: SocketAddr) {
    let (det, initial) = detect::sniff(&stream, Some(DETECT_TIMEOUT)).await;
    match det {
        Detected::Closed => {}
        Detected::Tls { sni: Some(sni), .. } if sni == cfg.local_name => {
            let Some((mut ds, client_id)) = accept_tls(stream, initial, peer).await else {
                return;
            };
            if ds.alpn_protocol() == Some(p::TRANSPORT_ALPN) {
                let Some((h, rest)) = read_transport_header(&mut ds).await else {
                    return;
                };
                let target = SocketAddr::new(dst.ip(), h.port as u16);
                let session = h.session_protocol.and_then(|s| s.kind);
                serve_port(cfg, ds, rest, peer, target, client_id, session.is_some()).await;
            } else {
                serve_port(cfg, ds, Vec::new(), peer, dst, client_id, true).await;
            }
        }
        _ => {
            // 平文・アプリ自身の TLS（SNI が自分ではない）
            let Ok(ds) = crate::mesh::server::plain(stream, initial).await else {
                return;
            };
            serve_port(cfg, ds, Vec::new(), peer, dst, None, !det.is_tls()).await;
        }
    }
}

/// ポートのポリシーで HTTP / L4 を処理する（`http_ok` が false なら検出せず L4）。
async fn serve_port(
    cfg: &Config,
    mut ds: DownStream,
    initial: Vec<u8>,
    peer: SocketAddr,
    dst: SocketAddr,
    client_id: Option<String>,
    http_ok: bool,
) {
    let _tcp = super::metrics::TcpGuard::inbound(dst, client_id.as_deref());
    let ic = discovery::inbound(dst.port()).await;
    let Some(ic) = ic else {
        relay(ds, &discovery::app_tcp(cfg), dst).await;
        return;
    };
    let tcp_allowed = |client: Option<&str>| {
        let header = |_: &str| None;
        let md = |_: &str, _: &[String]| None;
        let input = crate::mesh::xds::rbac::RbacInput {
            header: &header,
            path: b"",
            dst: Some(dst),
            sni: None,
            principal: client,
            direct_remote: Some(peer.ip()),
            remote: None,
            metadata: &md,
            http: false,
        };
        crate::mesh::xds::rbac::allowed(&ic.tcp_rbac, &input)
    };
    let detect_http =
        http_ok && matches!(ic.proto, InProto::Detect | InProto::Http1 | InProto::Http2);
    if detect_http {
        let (det, acc) = read_classify(&mut ds, initial).await;
        match det {
            Detected::Closed => {}
            Detected::Http1 | Detected::H2c => {
                let c = ctx(ic.hcm.clone(), Direction::Inbound, dst, peer, client_id);
                serve_http(ds, acc, det == Detected::H2c, c).await;
            }
            _ => {
                if tcp_allowed(client_id.as_deref()) {
                    let mut ds = ds;
                    if !acc.is_empty() {
                        // 読んだ分を返せないので、アプリへ先に送る
                        relay_with_prefix(&mut ds, acc, &discovery::app_tcp(cfg), dst).await;
                    } else {
                        relay(ds, &discovery::app_tcp(cfg), dst).await;
                    }
                } else {
                    ftlog::debug!("[linkerd] inbound TCP from {} to {} denied", peer, dst);
                }
            }
        }
        return;
    }
    if !tcp_allowed(client_id.as_deref()) {
        ftlog::debug!("[linkerd] inbound TCP from {} to {} denied", peer, dst);
        return;
    }
    if initial.is_empty() {
        relay(ds, &discovery::app_tcp(cfg), dst).await;
    } else {
        relay_with_prefix(&mut ds, initial, &discovery::app_tcp(cfg), dst).await;
    }
}

/// 先に読んでしまった `prefix` をアプリへ送ってから L4 で転送する。
async fn relay_with_prefix(
    ds: &mut DownStream,
    prefix: Vec<u8>,
    tp: &TcpProxyCfg,
    dst: SocketAddr,
) {
    use crate::runtime::io::AsyncWriteRentExt;
    let Some(slot) = tp.pick().cloned() else {
        return;
    };
    let picked = match crate::mesh::upstream::pick(&slot, Some(dst), None, &[]) {
        Ok(p) => p,
        Err(_) => return,
    };
    picked.begin();
    let mut up =
        match crate::mesh::upstream::connect(&picked, crate::mesh::upstream::Alpn::Tcp).await {
            Ok(u) => u,
            Err(_) => {
                picked.end(false, true);
                return;
            }
        };
    if up.write_all(prefix).await.0.is_err() {
        picked.end(false, true);
        return;
    }
    let _ = crate::mesh::tcp::relay(ds, &mut up, Duration::from_secs(3600)).await;
    picked.end(false, false);
}
