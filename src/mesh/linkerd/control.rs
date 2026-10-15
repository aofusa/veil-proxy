//! Linkerd の制御プレーン（identity・destination・policy）の gRPC クライアント（F-223）。
//!
//! - 専用スレッド（`veil-l5d-ctl`）で veil のランタイムを 1 本動かし、gRPC は TLS の上の HTTP/2
//!   （`upstream_mux`）で話す（tonic は使わない。xDS の ADS と同じ作り）。
//! - 制御プレーンの Pod も Linkerd のプロキシを持つので、TLS の相手の ID は `*_SVC_NAME`（DNS の SAN）で確かめる。
//!   identity は自分の証明書が無いうちに呼ぶのでクライアント証明書なし、destination・policy は mTLS。
//! - データプレーン（ワーカー）からの購読の依頼（宛先・ポート）は [`request`] で積み、このスレッドが拾って
//!   購読のタスクを起こす。結果はメッシュのモデル（`ArcSwap`）に書き、ワーカーはそれを読むだけ。

use super::env::Config;
use crate::grpc::framing::GrpcFrameDecoder;
use crate::http2::upstream_mux::{H2Mux, UpResp, UpStream};
use crate::mesh::model::StringMatcher;
use crate::mesh::upstream::UpIo;
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use std::cell::RefCell;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 制御プレーンの相手。
#[derive(Clone, Debug)]
pub struct CtlTarget {
    /// `host:port`
    pub addr: String,
    /// TLS のサーバの ID（DNS の SAN）
    pub name: String,
    /// 自分の証明書を出すか（identity は出さない）
    pub client_cert: bool,
}

/// ワーカーからの購読の依頼。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Request {
    /// outbound の宛先（OutboundPolicies.Watch）
    Outbound(SocketAddr),
    /// inbound のポート（InboundServerPolicies.WatchPort）
    Inbound(u16),
}

static REQUESTS: Mutex<Vec<Request>> = Mutex::new(Vec::new());

/// 購読を依頼する（同じ依頼は制御のスレッドで 1 回にまとめる）。
pub fn request(r: Request) {
    REQUESTS.lock().unwrap_or_else(|e| e.into_inner()).push(r);
}

pub fn spawn(cfg: Arc<Config>) {
    let r = std::thread::Builder::new()
        .name("veil-l5d-ctl".into())
        .spawn(move || {
            crate::runtime::block_on(run(cfg));
            crate::runtime::shutdown_thread();
        });
    if let Err(e) = r {
        ftlog::error!("[linkerd] cannot spawn the control thread: {}", e);
    }
}

async fn run(cfg: Arc<Config>) {
    crate::runtime::spawn(super::identity::run(cfg.clone()));
    let mut seen = std::collections::HashSet::new();
    loop {
        if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            return;
        }
        let reqs: Vec<Request> =
            std::mem::take(&mut *REQUESTS.lock().unwrap_or_else(|e| e.into_inner()));
        for r in reqs {
            if !seen.insert(r.clone()) {
                continue;
            }
            match r {
                Request::Outbound(dst) => {
                    crate::runtime::spawn(super::discovery::watch_outbound(cfg.clone(), dst));
                }
                Request::Inbound(port) => {
                    crate::runtime::spawn(super::discovery::watch_inbound(cfg.clone(), port));
                }
            }
        }
        crate::runtime::time::sleep(Duration::from_millis(5)).await;
    }
}

// ---------------------------------------------------------------------------
// gRPC
// ---------------------------------------------------------------------------

thread_local! {
    /// 制御プレーンの相手ごとの HTTP/2 接続（購読は 1 本の接続に多重化する）
    static MUX: RefCell<HashMap<String, Rc<H2Mux>>> = RefCell::new(HashMap::new());
}

// 理由付き allow: 制御プレーン用の専用スレッド（データプレーンではない）で名前を引く
#[allow(clippy::disallowed_methods)]
fn resolve(addr: &str) -> Vec<SocketAddr> {
    use std::net::ToSocketAddrs;
    addr.to_socket_addrs()
        .map(|it| it.collect())
        .unwrap_or_default()
}

/// 相手への HTTP/2 接続（使える接続があれば再利用する）。
async fn mux(t: &CtlTarget) -> Result<Rc<H2Mux>, String> {
    let key = format!("{}|{}", t.addr, t.client_cert);
    if let Some(m) = MUX.with(|m| {
        m.borrow()
            .get(&key)
            .filter(|m| !m.is_dead() && m.has_capacity())
            .cloned()
    }) {
        return Ok(m);
    }
    let addrs = resolve(&t.addr);
    if addrs.is_empty() {
        return Err(format!("cannot resolve {}", t.addr));
    }
    let addr = addrs[crate::mesh::httpcore::rand_u32() as usize % addrs.len()];
    let tcp = crate::mesh::net::connect(addr, None, Duration::from_secs(3))
        .await
        .map_err(|e| format!("connect {} ({}): {}", t.addr, addr, e))?;
    let cfg = crate::mesh::identity::client_config(
        t.client_cert.then_some(super::CERT),
        Some(super::ROOTS),
        vec![StringMatcher::exact(&t.name)],
        &[],
    )?;
    let name = rustls::pki_types::ServerName::try_from(t.name.clone())
        .map_err(|e| format!("server name {}: {}", t.name, e))?;
    let hs = async {
        #[cfg(veil_ktls)]
        {
            crate::ktls_rustls::connect(tcp, cfg, name, false, true, false).await
        }
        #[cfg(not(veil_ktls))]
        {
            crate::simple_tls::connect(tcp, cfg, name).await
        }
    };
    let tls = crate::runtime::timeout(Duration::from_secs(5), hs)
        .await
        .map_err(|_| format!("TLS handshake with {} timed out", t.addr))?
        .map_err(|e| format!("TLS handshake with {}: {}", t.addr, e))?;
    let m = Rc::new(H2Mux::start(
        UpIo::Tls(Box::new(tls)),
        true,
        Duration::from_secs(3600),
    ));
    MUX.with(|mm| mm.borrow_mut().insert(key, m.clone()));
    Ok(m)
}

fn frame<M: Message>(m: &M) -> Bytes {
    let len = m.encoded_len();
    let mut b = BytesMut::with_capacity(5 + len);
    b.put_u8(0);
    b.put_u32(len as u32);
    let _ = m.encode(&mut b);
    b.freeze()
}

fn grpc_error(headers: &[(Bytes, Bytes)]) -> Option<String> {
    let status = headers
        .iter()
        .find(|(k, _)| k.as_ref() == b"grpc-status")
        .map(|(_, v)| v.clone())?;
    if status.as_ref() == b"0" {
        return None;
    }
    let msg = headers
        .iter()
        .find(|(k, _)| k.as_ref() == b"grpc-message")
        .map(|(_, v)| String::from_utf8_lossy(v).into_owned())
        .unwrap_or_default();
    Some(format!(
        "grpc-status {}: {}",
        String::from_utf8_lossy(&status),
        msg
    ))
}

/// 開いた gRPC の応答のストリーム。
pub struct Call {
    st: UpStream,
    dec: GrpcFrameDecoder,
}

/// 要求を 1 つ送って応答のストリームを開く（unary・server streaming 共通）。
pub async fn call<Q: Message>(t: &CtlTarget, path: &str, req: &Q) -> Result<Call, String> {
    let m = mux(t).await?;
    let headers: [(&[u8], &[u8]); 3] = [
        (b"content-type", b"application/grpc"),
        (b"te", b"trailers"),
        (b"user-agent", b"veil-linkerd-compat"),
    ];
    let mut st = m
        .open(
            b"POST",
            path.as_bytes(),
            t.addr.as_bytes(),
            headers.iter().copied(),
            false,
        )
        .ok_or_else(|| format!("cannot open a stream to {}", t.addr))?;
    if let Some(tx) = &st.req_tx {
        tx.send(frame(req))
            .await
            .map_err(|_| "request stream closed".to_string())?;
    }
    st.finish_request();
    Ok(Call {
        st,
        dec: GrpcFrameDecoder::with_max_size(64 * 1024 * 1024),
    })
}

impl Call {
    /// 次のメッセージ（ストリームが正常に終われば `None`）。`idle` を過ぎても何も来なければエラー。
    pub async fn next<R: Message + Default>(&mut self, idle: Duration) -> Result<Option<R>, String> {
        loop {
            match self.dec.decode_next() {
                Ok(Some(f)) => {
                    return R::decode(&f.data[..])
                        .map(Some)
                        .map_err(|e| format!("invalid message: {}", e));
                }
                Ok(None) => {}
                Err(e) => return Err(format!("bad gRPC frame: {}", e)),
            }
            let item = crate::runtime::time::timeout(idle, self.st.resp_rx.recv())
                .await
                .map_err(|_| "control stream idle timeout".to_string())?;
            match item {
                Some(UpResp::Head { status, headers }) => {
                    if status != 200 {
                        return Err(format!("HTTP {}", status));
                    }
                    if let Some(e) = grpc_error(&headers) {
                        return Err(e);
                    }
                }
                Some(UpResp::Data(chunk)) => self.dec.push(&chunk),
                Some(UpResp::Trailers(t)) => {
                    return match grpc_error(&t) {
                        Some(e) => Err(e),
                        None => Ok(None),
                    };
                }
                Some(UpResp::Reset { .. }) => return Err("stream reset".into()),
                None => return Ok(None),
            }
        }
    }
}

/// unary の呼び出し。
pub async fn unary<Q: Message, R: Message + Default>(
    t: &CtlTarget,
    path: &str,
    req: &Q,
    timeout: Duration,
) -> Result<R, String> {
    let mut c = call(t, path, req).await?;
    c.next::<R>(timeout)
        .await?
        .ok_or_else(|| "empty response".to_string())
}
