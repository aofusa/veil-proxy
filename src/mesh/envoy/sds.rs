//! SDS のクライアント（F-217・F-222）: pilot-agent の SDS（UDS）から証明書と信頼アンカーを受け取る。
//!
//! 秘密の名前（Istio のサイドカーは `default` と `ROOTCA`、ゲートウェイは `credentialName`）ごとに
//! 1 本の `StreamSecrets`（SotW）を張り、届いたら [`crate::mesh::identity`] へ差し替える（鍵はメモリのみ）。
//! 欲しい名前は変換（xDS）が `want` で足していく。

use crate::grpc::framing::GrpcFrameDecoder;
use crate::mesh::xds::proto::{self as p, decode_any};
use bytes::{BufMut, Bytes, BytesMut};
use ftlog::{debug, info, warn};
use once_cell::sync::Lazy;
use prost::Message;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::time::Duration;

const PATH: &str = "/envoy.service.secret.v3.SecretDiscoveryService/StreamSecrets";

static WANTED: Lazy<Mutex<BTreeSet<String>>> = Lazy::new(|| {
    let mut s = BTreeSet::new();
    s.insert("default".to_string());
    s.insert("ROOTCA".to_string());
    Mutex::new(s)
});

/// 秘密の名前を購読に足す。
pub fn want(name: &str) {
    if let Ok(mut w) = WANTED.lock() {
        w.insert(name.to_string());
    }
}

/// ADS で配られる秘密か（Istio のゲートウェイの `credentialName` は `kubernetes://<name>`。
/// サイドカーの `default`・`ROOTCA` は pilot-agent の SDS の UDS で配られる）。
pub fn via_ads(name: &str) -> bool {
    name.starts_with("kubernetes://") || name.starts_with("kubernetes-gateway://")
}

/// ADS で購読する秘密の名前（今欲しいもの）。
pub fn wanted_ads() -> BTreeSet<String> {
    WANTED
        .lock()
        .map(|w| w.iter().filter(|n| via_ads(n)).cloned().collect())
        .unwrap_or_default()
}

fn frame<M: Message>(m: &M) -> Bytes {
    let len = m.encoded_len();
    let mut b = BytesMut::with_capacity(5 + len);
    b.put_u8(0);
    b.put_u32(len as u32);
    let _ = m.encode(&mut b);
    b.freeze()
}

pub fn spawn(uds: PathBuf, node: p::Node) {
    let r = std::thread::Builder::new()
        .name("veil-sds".into())
        .spawn(move || {
            crate::runtime::block_on(async move {
                let mut started: BTreeSet<String> = BTreeSet::new();
                loop {
                    if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        return;
                    }
                    let wanted: Vec<String> = WANTED
                        .lock()
                        .map(|w| w.iter().cloned().collect())
                        .unwrap_or_default();
                    for name in wanted {
                        if started.insert(name.clone()) {
                            crate::runtime::spawn(stream_loop(uds.clone(), node.clone(), name));
                        }
                    }
                    crate::runtime::time::sleep(Duration::from_millis(200)).await;
                }
            });
            crate::runtime::shutdown_thread();
        });
    if let Err(e) = r {
        warn!("[sds] cannot spawn the SDS thread: {}", e);
    }
}

async fn stream_loop(uds: PathBuf, node: p::Node, name: String) {
    let mut backoff = 200u64;
    loop {
        if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            return;
        }
        let mut got = false;
        match session(&uds, &node, &name, &mut got).await {
            Ok(()) => debug!("[sds] stream for {} ended", name),
            Err(e) => warn!("[sds] stream for {}: {}", name, e),
        }
        if got {
            backoff = 200;
        }
        crate::runtime::time::sleep(Duration::from_millis(backoff)).await;
        backoff = (backoff * 2).min(10_000);
    }
}

async fn session(
    uds: &std::path::Path,
    node: &p::Node,
    name: &str,
    got: &mut bool,
) -> Result<(), String> {
    use crate::http2::upstream_mux::{H2Mux, UpResp};
    let io = crate::runtime::time::timeout(
        Duration::from_secs(10),
        crate::runtime::tcp::TcpStream::connect_unix(uds),
    )
    .await
    .map_err(|_| "connect timeout".to_string())?
    .map_err(|e| format!("connect {}: {}", uds.display(), e))?;
    let mux = H2Mux::start(io, false, Duration::ZERO);
    let headers: [(&[u8], &[u8]); 2] =
        [(b"content-type", b"application/grpc"), (b"te", b"trailers")];
    let mut st = mux
        .open(
            b"POST",
            PATH.as_bytes(),
            b"sds-grpc",
            headers.iter().copied(),
            false,
        )
        .ok_or("cannot open an HTTP/2 stream")?;
    let first = frame(&p::DiscoveryRequest {
        node: Some(node.clone()),
        type_url: p::TYPE_SECRET.into(),
        resource_names: vec![name.to_string()],
        ..Default::default()
    });
    if let Some(tx) = &st.req_tx {
        tx.send(first).await.map_err(|_| "request stream closed")?;
    }
    let mut decoder = GrpcFrameDecoder::with_max_size(16 * 1024 * 1024);
    loop {
        let item = match crate::runtime::time::timeout(
            Duration::from_millis(500),
            st.resp_rx.recv(),
        )
        .await
        {
            Err(_) => {
                if crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                    st.finish_request();
                    return Ok(());
                }
                continue;
            }
            Ok(i) => i,
        };
        match item {
            Some(UpResp::Head { status, .. }) => {
                if status != 200 {
                    return Err(format!("SDS returned HTTP {}", status));
                }
            }
            Some(UpResp::Data(chunk)) => {
                decoder.push(&chunk);
                while let Ok(Some(f)) = decoder.decode_next() {
                    let resp = p::DiscoveryResponse::decode(&f.data[..])
                        .map_err(|e| format!("invalid DiscoveryResponse: {}", e))?;
                    *got = true;
                    let mut err = None;
                    for any in &resp.resources {
                        match decode_any::<p::Secret>(any, p::TYPE_SECRET) {
                            Some(sec) => {
                                if let Err(e) = apply(&sec) {
                                    err = Some(e);
                                }
                            }
                            None => err = Some(format!("cannot decode {}", any.type_url)),
                        }
                    }
                    let ack = p::DiscoveryRequest {
                        version_info: resp.version_info.clone(),
                        type_url: p::TYPE_SECRET.into(),
                        resource_names: vec![name.to_string()],
                        response_nonce: resp.nonce.clone(),
                        error_detail: err.map(|m| p::Status {
                            code: 3,
                            message: m,
                        }),
                        ..Default::default()
                    };
                    let f = frame(&ack);
                    if let Some(tx) = &st.req_tx {
                        tx.send(f).await.map_err(|_| "request stream closed")?;
                    }
                }
            }
            Some(UpResp::Trailers(_)) | None => return Ok(()),
            Some(UpResp::Reset { .. }) => return Err("stream reset".into()),
        }
    }
}

pub fn apply(sec: &p::Secret) -> Result<(), String> {
    if let Some(tc) = &sec.tls_certificate {
        let chain = tc
            .certificate_chain
            .as_ref()
            .and_then(|d| d.inline())
            .ok_or("tls_certificate without inline chain")?;
        let key = tc
            .private_key
            .as_ref()
            .and_then(|d| d.inline())
            .ok_or("tls_certificate without inline key")?;
        crate::mesh::identity::set_cert_pem(&sec.name, chain, key)?;
        info!("[sds] certificate '{}' updated", sec.name);
    }
    if let Some(vc) = &sec.validation_context {
        let ca = vc
            .trusted_ca
            .as_ref()
            .and_then(|d| d.inline())
            .ok_or("validation_context without inline trusted_ca")?;
        crate::mesh::identity::set_roots_pem(&sec.name, ca)?;
        info!("[sds] trust anchors '{}' updated", sec.name);
    }
    Ok(())
}
