//! ambient の HBONE と Envoy の内部リスナー（F-224 の waypoint）。
//!
//! istiod が waypoint に配る設定は Envoy の内部リスナー（ポートを持たず、名前で接続を渡す）で組まれている:
//!
//! 1. `connect_terminate`（15008、mTLS + HTTP/2）: ztunnel からの CONNECT（`:authority` = 宛先のサービスの
//!    VIP:ポート）を終端し、中身の TCP を内部リスナー `main_internal` へ渡す。
//! 2. `main_internal`: `filter_chain_matcher`（宛先の IP・ポート）でサービスごとのフィルタチェーンを選び、
//!    L7 の処理（ルート・認可・観測）をして、内部アドレスのエンドポイント（`connect_originate`、メタデータに
//!    宛先の Pod の IP:ポート）へ送る。
//! 3. `connect_originate`: `tcp_proxy` + `tunneling_config` で宛先の Pod の ztunnel（Pod の IP:15008）へ
//!    HTTP/2 CONNECT（`:authority` = Pod の IP:ポート）を張る。
//!
//! veil は内部リスナーへの受け渡しを `socketpair(2)` で行う（片端を通常の接続と同じ経路
//! 〈[`super::server::handle_internal`]〉で処理し、もう片端を CONNECT のストリームと相互に転送する）。
//! HTTP/1.1・L4・TLS の処理はファイル記述子の可読を待つ作りなので、そのまま使える。HBONE の送出は宛先の
//! ztunnel ごとに 1 本の HTTP/2 接続（ワーカーごとのプール）に CONNECT のストリームを多重化する。

use super::server::{self, Inherited};
use super::state::state;
use crate::http2::upstream_mux::{H2Mux, UpResp, UpStream};
use crate::runtime::handle::AsRawFd;
use crate::runtime::io::AsyncWriteRentExt;
use crate::runtime::tcp::TcpStream;
use bytes::Bytes;
use std::cell::RefCell;
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

const BUF: usize = 64 * 1024;

/// 非ブロッキングの `socketpair(AF_UNIX, SOCK_STREAM)`。
pub fn socketpair() -> io::Result<(TcpStream, TcpStream)> {
    let mut fds = [0i32; 2];
    // SAFETY: 2 要素の配列に fd を受け取る
    let r = unsafe {
        libc::socketpair(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
            0,
            fds.as_mut_ptr(),
        )
    };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 作ったばかりの非ブロッキングのソケット fd の所有権を移す
    Ok(unsafe { (TcpStream::from_raw_fd(fds[0]), TcpStream::from_raw_fd(fds[1])) })
}

/// 同じソケットの別の fd（読みと書きを別のタスクで行う）。
fn dup(s: &TcpStream) -> io::Result<TcpStream> {
    // SAFETY: 有効な fd を複製する
    let fd = unsafe { libc::fcntl(s.as_raw_fd() as i32, libc::F_DUPFD_CLOEXEC, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 複製した fd の所有権を移す
    Ok(unsafe { TcpStream::from_raw_fd(fd) })
}

fn shutdown(s: &TcpStream, how: i32) {
    // SAFETY: 有効なソケット fd の送信側（または両方）を閉じるだけ
    unsafe {
        libc::shutdown(s.as_raw_fd() as i32, how);
    }
}

/// 内部リスナー `name` への接続（Envoy の `internal_upstream`）。`dst` は元の宛先（エンドポイントの
/// `envoy.filters.listener.original_dst.local`）。戻り値の端を上流の接続として使う。
pub async fn connect_internal(name: &str, dst: SocketAddr) -> io::Result<TcpStream> {
    let lst = state()
        .listeners
        .load()
        .internal
        .get(name)
        .cloned()
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                format!("no internal listener {}", name),
            )
        })?;
    // HBONE の送出（`tcp_proxy` + `tunneling_config` だけのリスナー）は直接 CONNECT を張る
    let tunnel = lst
        .chains
        .first()
        .or(lst.default_chain.as_ref())
        .and_then(|c| match &c.action {
            super::model::ChainAction::Tcp(tp) if tp.tunnel.is_some() => Some(tp.clone()),
            _ => None,
        });
    if let Some(tp) = tunnel {
        return originate(&tp, dst).await;
    }
    let (mine, theirs) = socketpair()?;
    let peer = SocketAddr::from(([0, 0, 0, 0], 0));
    crate::runtime::spawn(server::handle_internal(
        theirs,
        lst,
        peer,
        dst,
        Inherited::default(),
    ));
    Ok(mine)
}

thread_local! {
    /// 宛先の ztunnel（IP:15008）ごとの HTTP/2 接続
    static HBONE_POOL: RefCell<HashMap<SocketAddr, Rc<H2Mux>>> = RefCell::new(HashMap::new());
}

/// HBONE の CONNECT を張り、中身を転送する端を返す。
async fn originate(tp: &super::model::TcpProxyCfg, dst: SocketAddr) -> io::Result<TcpStream> {
    let err = |m: String| io::Error::new(io::ErrorKind::ConnectionRefused, m);
    let slot = tp.pick().cloned().ok_or_else(|| err("tunnel without a cluster".into()))?;
    let cfg = slot
        .cfg
        .load()
        .as_ref()
        .clone()
        .ok_or_else(|| err(format!("cluster {} not ready", slot.name)))?;
    let target = SocketAddr::new(dst.ip(), cfg.port_override.unwrap_or(dst.port()));
    let authority = authority_of(tp.tunnel.as_deref().unwrap_or(""), dst);
    let mux = match HBONE_POOL.with(|p| {
        p.borrow()
            .get(&target)
            .filter(|m| !m.is_dead() && m.has_capacity())
            .cloned()
    }) {
        Some(m) => m,
        None => {
            let picked = super::upstream::Picked {
                tls: cfg.tls_default.clone(),
                eps: slot.endpoints.load_full(),
                idx: usize::MAX,
                addr: target,
                cfg: cfg.clone(),
            };
            let io = super::upstream::connect(&picked, super::upstream::Alpn::H2).await?;
            let m = Rc::new(H2Mux::start(io, true, cfg.idle_timeout));
            HBONE_POOL.with(|p| {
                let mut p = p.borrow_mut();
                p.retain(|_, m| !m.is_dead());
                p.insert(target, m.clone());
            });
            m
        }
    };
    let mut st = mux
        .open(b"CONNECT", b"", authority.as_bytes(), std::iter::empty(), false)
        .ok_or_else(|| err(format!("cannot open a CONNECT stream to {}", target)))?;
    let head = crate::runtime::time::timeout(
        cfg.connect_timeout.max(Duration::from_secs(1)),
        st.resp_rx.recv(),
    )
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "HBONE CONNECT timeout"))?;
    match head {
        Some(UpResp::Head { status, .. }) if (200..300).contains(&status) => {}
        Some(UpResp::Head { status, .. }) => {
            return Err(err(format!("HBONE CONNECT to {} returned {}", authority, status)));
        }
        _ => return Err(err(format!("HBONE CONNECT to {} failed", authority))),
    }
    ftlog::debug!("[mesh] HBONE CONNECT {} via {}", authority, target);
    let (mine, theirs) = socketpair()?;
    crate::runtime::spawn(pump_upstream(st.req_tx.take(), st, theirs));
    Ok(mine)
}

/// `tunneling_config.hostname` の書式（Istio は `%DOWNSTREAM_LOCAL_ADDRESS%`）。
fn authority_of(template: &str, dst: SocketAddr) -> String {
    if template.is_empty() || template.contains('%') {
        dst.to_string()
    } else {
        template.to_string()
    }
}

/// 送出した CONNECT のストリーム ⇄ socketpair の端。
async fn pump_upstream(
    req_tx: Option<crate::stream_channel::Sender<Bytes>>,
    st: UpStream,
    sock: TcpStream,
) {
    let wr = match dup(&sock) {
        Ok(w) => w,
        Err(_) => return,
    };
    // 送信側（socket → ストリーム）が終わるまでストリームを捨てない（先に捨てると RST_STREAM になる）
    let (done_tx, done_rx) = crate::stream_channel::channel::<()>(1);
    crate::runtime::spawn(async move {
        let _done = done_tx;
        let sock = sock;
        let Some(tx) = req_tx else { return };
        loop {
            let buf: Vec<u8> = Vec::with_capacity(BUF);
            let (r, mut buf) = sock.read(buf).await;
            match r {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    buf.truncate(n);
                    if tx.send(Bytes::from(buf)).await.is_err() {
                        break;
                    }
                }
            }
        }
        // `tx` の drop で END_STREAM
    });
    let mut wr = wr;
    loop {
        match st.resp_rx.recv().await {
            Some(UpResp::Data(b)) => {
                if wr.write_all(b).await.0.is_err() {
                    shutdown(&wr, libc::SHUT_RDWR);
                    break;
                }
            }
            Some(UpResp::Head { .. }) => {}
            Some(UpResp::Trailers(_)) | None => {
                shutdown(&wr, libc::SHUT_WR);
                break;
            }
            Some(UpResp::Reset { .. }) => {
                shutdown(&wr, libc::SHUT_RDWR);
                break;
            }
        }
    }
    let _ = done_rx.recv().await;
    drop(st);
}

/// 終端した CONNECT（下流の HTTP/2 のストリーム）の中身を内部リスナーへ渡す。
/// `req_rx` が要求の本文（下流 → 中身）、戻るまで `send_body` で応答の本文（中身 → 下流）を送る。
pub async fn terminate<F, Fut>(
    lst: Arc<super::model::MeshListener>,
    peer: SocketAddr,
    dst: SocketAddr,
    inherited: Inherited,
    req_rx: Option<crate::stream_channel::Receiver<Bytes>>,
    mut send_body: F,
) where
    F: FnMut(Bytes) -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let (mine, theirs) = match socketpair() {
        Ok(p) => p,
        Err(_) => return,
    };
    crate::runtime::spawn(server::handle_internal(theirs, lst, peer, dst, inherited));
    let wr = match dup(&mine) {
        Ok(w) => w,
        Err(_) => return,
    };
    // 下流 → 中身
    crate::runtime::spawn(async move {
        let mut wr = wr;
        if let Some(rx) = req_rx {
            while let Some(b) = rx.recv().await {
                if wr.write_all(b).await.0.is_err() {
                    break;
                }
            }
        }
        shutdown(&wr, libc::SHUT_WR);
    });
    // 中身 → 下流
    let rd = mine;
    loop {
        let buf: Vec<u8> = Vec::with_capacity(BUF);
        let (r, mut buf) = rd.read(buf).await;
        match r {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                buf.truncate(n);
                if !send_body(Bytes::from(buf)).await {
                    shutdown(&rd, libc::SHUT_RDWR);
                    break;
                }
            }
        }
    }
}

/// CONNECT の `:authority`（`IP:ポート`）。
pub fn parse_authority(a: &[u8]) -> Option<SocketAddr> {
    std::str::from_utf8(a).ok()?.parse().ok()
}
