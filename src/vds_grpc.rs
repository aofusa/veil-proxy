//! VDS の gRPC 転送（F-199）: `veil.vds.v1.VeilDiscoveryService/StreamSnapshots` の双方向ストリーム。
//!
//! VDS の専用スレッドで veil のランタイムを 1 本動かし（`runtime::block_on`）、上流 HTTP/2 の多重化接続
//! （`http2::upstream_mux`、h2c / TLS + ALPN h2）でストリームを 1 本開く。要求（初回・ACK・NACK）は gRPC の
//! フレームにして要求本文へ、応答は `GrpcFrameDecoder` で切り出して prost でデコードする。tokio・tonic は使わない。
//!
//! メッセージは `proto/veil/vds/v1/vds.proto` と同じ定義を prost の derive で持つ（veil のビルドに protoc を
//! 要求しないため）。適用・ACK/NACK・状態は REST 版と共通（`vds::apply_snapshot` 等）。
//! 接続断・`grpc-status != 0`・ストリームの終わりは指数バックオフで張り直し、最後に適用した `version_info` から再開する。

use crate::config::SHUTDOWN_FLAG;
use crate::grpc::framing::GrpcFrameDecoder;
use crate::vds::{VdsConfig, TYPE_SNAPSHOT};
use bytes::{BufMut, Bytes, BytesMut};
use ftlog::{info, warn};
use prost::Message;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

/// gRPC のメソッドのパス。
pub const PATH: &str = "/veil.vds.v1.VeilDiscoveryService/StreamSnapshots";

/// `veil.vds.v1.Node`
#[derive(Clone, PartialEq, prost::Message)]
pub struct Node {
    #[prost(string, tag = "1")]
    pub id: String,
    #[prost(string, tag = "2")]
    pub cluster: String,
    #[prost(string, tag = "3")]
    pub user_agent_name: String,
    #[prost(string, tag = "4")]
    pub user_agent_version: String,
}

/// `veil.vds.v1.ErrorDetail`
#[derive(Clone, PartialEq, prost::Message)]
pub struct ErrorDetail {
    #[prost(int32, tag = "1")]
    pub code: i32,
    #[prost(string, tag = "2")]
    pub message: String,
}

/// `veil.vds.v1.DiscoveryRequest`
#[derive(Clone, PartialEq, prost::Message)]
pub struct DiscoveryRequest {
    #[prost(string, tag = "1")]
    pub version_info: String,
    #[prost(message, optional, tag = "2")]
    pub node: Option<Node>,
    #[prost(string, tag = "3")]
    pub type_url: String,
    #[prost(string, tag = "4")]
    pub response_nonce: String,
    #[prost(message, optional, tag = "5")]
    pub error_detail: Option<ErrorDetail>,
}

/// `veil.vds.v1.Resource`
#[derive(Clone, PartialEq, prost::Message)]
pub struct Resource {
    #[prost(string, tag = "1")]
    pub type_url: String,
    #[prost(string, tag = "2")]
    pub name: String,
    #[prost(string, tag = "3")]
    pub json: String,
}

/// `veil.vds.v1.DiscoveryResponse`
#[derive(Clone, PartialEq, prost::Message)]
pub struct DiscoveryResponse {
    #[prost(string, tag = "1")]
    pub version_info: String,
    #[prost(string, tag = "2")]
    pub nonce: String,
    #[prost(string, tag = "3")]
    pub type_url: String,
    #[prost(message, repeated, tag = "4")]
    pub resources: Vec<Resource>,
}

/// gRPC の `Resource` を REST 版の形（`@type`・`name` と本体のキーを持つオブジェクト）にする。
pub(crate) fn resources_to_json(resources: &[Resource]) -> Result<Vec<Value>, String> {
    let mut out = Vec::with_capacity(resources.len());
    for (i, r) in resources.iter().enumerate() {
        let body: Value = if r.json.trim().is_empty() {
            Value::Object(Default::default())
        } else {
            serde_json::from_str(&r.json).map_err(|e| format!("resource[{}].json: {}", i, e))?
        };
        let Value::Object(mut obj) = body else {
            return Err(format!("resource[{}].json must be a JSON object", i));
        };
        obj.insert("@type".into(), Value::String(r.type_url.clone()));
        if !r.name.is_empty() {
            obj.insert("name".into(), Value::String(r.name.clone()));
        }
        out.push(Value::Object(obj));
    }
    Ok(out)
}

fn frame<M: Message>(m: &M) -> Bytes {
    let len = m.encoded_len();
    let mut b = BytesMut::with_capacity(5 + len);
    b.put_u8(0);
    b.put_u32(len as u32);
    let _ = m.encode(&mut b);
    b.freeze()
}

/// 接続をまたいで引き継ぐ状態（最後に適用した版と、次の要求で送る nonce・NACK）。
#[derive(Default)]
struct State {
    version_info: String,
    nonce: String,
    error_detail: Option<String>,
}

impl State {
    fn request(&mut self, node: &Node) -> DiscoveryRequest {
        DiscoveryRequest {
            version_info: self.version_info.clone(),
            node: Some(node.clone()),
            type_url: TYPE_SNAPSHOT.into(),
            response_nonce: self.nonce.clone(),
            // NACK は 1 回だけ伝える（REST 版と同じ）
            error_detail: self.error_detail.take().map(|m| ErrorDetail {
                code: 3,
                message: m,
            }),
        }
    }
}

/// TLS のコネクタ（ALPN は h2 だけ）。
#[cfg(veil_ktls)]
type Connector = crate::ktls_rustls::RustlsConnector;
#[cfg(not(veil_ktls))]
type Connector = crate::simple_tls::SimpleTlsConnector;

fn connector(cfg: &VdsConfig) -> Result<Connector, String> {
    let opts = crate::ctrl_http::TlsOptions {
        ca_cert: Some(cfg.ca_cert.clone()),
        client_cert: Some(cfg.client_cert.clone()),
        client_key: Some(cfg.client_key.clone()),
    };
    let mut c = (*crate::ctrl_http::client_config(&opts)?).clone();
    c.alpn_protocols = vec![b"h2".to_vec()];
    #[cfg(veil_ktls)]
    return Ok(crate::ktls_rustls::RustlsConnector::new(Arc::new(c)).with_ktls(false));
    #[cfg(not(veil_ktls))]
    return Ok(crate::simple_tls::SimpleTlsConnector::new(Arc::new(c)));
}

/// VDS スレッドの本体（`vds::spawn_if_enabled` から）。
pub fn run(cfg: VdsConfig, config_path: PathBuf) {
    crate::runtime::block_on(async move { run_async(cfg, config_path).await });
    crate::runtime::shutdown_thread();
    info!("[VDS] stopped");
}

async fn run_async(cfg: VdsConfig, config_path: PathBuf) {
    let servers = cfg.server_list();
    let mut urls = Vec::with_capacity(servers.len());
    for s in &servers {
        match crate::ctrl_http::parse_url(s) {
            Ok(u) => urls.push(u),
            Err(e) => {
                warn!("[VDS] vds.server '{}': {}", s, e);
                return;
            }
        }
    }
    if urls.is_empty() {
        return;
    }
    let tls = if urls.iter().any(|u| u.tls) {
        match connector(&cfg) {
            Ok(c) => Some(c),
            Err(e) => {
                warn!("[VDS] TLS setup failed: {}", e);
                return;
            }
        }
    } else {
        None
    };
    let node = Node {
        id: crate::vds::node_id(&cfg),
        cluster: cfg.cluster.clone(),
        user_agent_name: "veil".into(),
        user_agent_version: env!("CARGO_PKG_VERSION").into(),
    };
    info!(
        "[VDS] subscribing to {} (gRPC) as node '{}'",
        servers.join(", "),
        node.id
    );
    let mut state = State::default();
    let initial = cfg.retry_initial_ms.max(50);
    let mut backoff = initial;
    let mut rot = crate::vds::ServerRotation::new(urls.len());
    while !SHUTDOWN_FLAG.load(Ordering::Relaxed) {
        let mut got_response = false;
        let i = rot.current();
        // 失敗して次の制御プレーンへ移った（1 周していない）ならバックオフせずにすぐ試す
        let mut next_now = false;
        match session(
            &cfg,
            &servers[i],
            &urls[i],
            tls.as_ref(),
            &node,
            &mut state,
            &config_path,
            &mut got_response,
        )
        .await
        {
            Ok(()) => info!("[VDS] stream to {} ended; reconnecting", servers[i]),
            Err(e) => {
                warn!("[VDS] stream to {} failed: {}", servers[i], e);
                crate::vds::record_connection(false, Some(e));
                // 次の制御プレーンへ（全部を 1 周したらバックオフ）。nonce は制御プレーンごとのもの
                state.nonce.clear();
                next_now = !rot.failed();
            }
        }
        if got_response {
            backoff = initial;
            rot.succeeded();
        }
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            break;
        }
        if next_now {
            continue;
        }
        let mut left = backoff;
        while left > 0 && !SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            let step = left.min(200);
            crate::runtime::time::sleep(Duration::from_millis(step)).await;
            left -= step;
        }
        backoff = (backoff * 2).min(cfg.retry_max_ms.max(100));
    }
}

/// 1 本のストリーム（接続 → 初回要求 → 応答ごとに適用して ACK/NACK）。
#[allow(clippy::too_many_arguments)]
async fn session(
    cfg: &VdsConfig,
    server: &str,
    url: &crate::ctrl_http::Url,
    tls: Option<&Connector>,
    node: &Node,
    state: &mut State,
    config_path: &Path,
    got_response: &mut bool,
) -> Result<(), String> {
    use crate::http2::upstream_mux::{H2Mux, UpResp};
    const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

    let addr = if url.host.contains(':') {
        format!("[{}]:{}", url.host, url.port)
    } else {
        format!("{}:{}", url.host, url.port)
    };
    let tcp = crate::runtime::time::timeout(
        CONNECT_TIMEOUT,
        crate::runtime::tcp::TcpStream::connect_str(&addr),
    )
    .await
    .map_err(|_| format!("connect to {} timed out", addr))?
    .map_err(|e| format!("connect to {}: {}", addr, e))?;
    let _ = tcp.set_nodelay(true);
    set_keepalive(&tcp, cfg.keepalive_secs);

    // ストリームが終われば接続も閉じる（idle_timeout = 0）
    let mux = match tls {
        Some(c) => {
            let s = crate::runtime::time::timeout(CONNECT_TIMEOUT, c.connect(tcp, &url.host))
                .await
                .map_err(|_| "TLS handshake timed out".to_string())?
                .map_err(|e| format!("TLS handshake: {}", e))?;
            if !s.negotiated_h2() {
                return Err("the control plane did not negotiate h2 (ALPN)".into());
            }
            H2Mux::start(s, true, Duration::ZERO)
        }
        None => H2Mux::start(tcp, false, Duration::ZERO),
    };

    let token = if cfg.token_file.is_empty() {
        None
    } else {
        match crate::ctrl_http::read_file(&cfg.token_file) {
            Ok(t) => Some(format!("Bearer {}", String::from_utf8_lossy(&t).trim())),
            Err(e) => {
                warn!("[VDS] cannot read token_file {}: {}", cfg.token_file, e);
                None
            }
        }
    };
    let path = format!("{}{}", url.path.trim_end_matches('/'), PATH);
    let ua = concat!("veil/", env!("CARGO_PKG_VERSION"));
    let mut headers: Vec<(&[u8], &[u8])> = vec![
        (b"content-type", b"application/grpc"),
        (b"te", b"trailers"),
        (b"user-agent", ua.as_bytes()),
    ];
    if let Some(t) = &token {
        headers.push((b"authorization", t.as_bytes()));
    }
    let mut stream = mux
        .open(
            b"POST",
            path.as_bytes(),
            addr.as_bytes(),
            headers.iter().copied(),
            false,
        )
        .ok_or("cannot open an HTTP/2 stream")?;
    send_request(&stream, state.request(node)).await?;

    let mut decoder = GrpcFrameDecoder::with_max_size(cfg.max_response_bytes);
    loop {
        // 停止を見逃さないよう短い間隔で起きる（受信の待ちは取り消しても失われない）
        let item =
            match crate::runtime::time::timeout(Duration::from_millis(500), stream.resp_rx.recv())
                .await
            {
                Err(_) => {
                    if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
                        stream.finish_request();
                        return Ok(());
                    }
                    continue;
                }
                Ok(item) => item,
            };
        match item {
            Some(UpResp::Head { status, headers }) => {
                if status != 200 {
                    return Err(format!("control plane returned HTTP {}", status));
                }
                // Trailers-Only（すぐ grpc-status が来た）
                if let Some(e) = grpc_error(&headers) {
                    return Err(e);
                }
                crate::vds::record_connection_to(server);
            }
            Some(UpResp::Data(chunk)) => {
                decoder.push(&chunk);
                loop {
                    let f = match decoder.decode_next() {
                        Ok(Some(f)) => f,
                        Ok(None) => break,
                        Err(e) => return Err(format!("bad gRPC frame: {}", e)),
                    };
                    if f.compressed {
                        return Err("compressed messages are not supported".into());
                    }
                    let resp = DiscoveryResponse::decode(&f.data[..])
                        .map_err(|e| format!("invalid DiscoveryResponse: {}", e))?;
                    *got_response = true;
                    handle(resp, cfg, config_path, state);
                    send_request(&stream, state.request(node)).await?;
                }
            }
            Some(UpResp::Trailers(t)) => {
                return match grpc_error(&t) {
                    Some(e) => Err(e),
                    None => Ok(()),
                };
            }
            Some(UpResp::Reset { .. }) => return Err("stream reset by the control plane".into()),
            None => return Ok(()),
        }
    }
}

/// 要求メッセージを 1 つ送る（要求本文のチャネルへ gRPC のフレームで）。
async fn send_request(
    stream: &crate::http2::upstream_mux::UpStream,
    req: DiscoveryRequest,
) -> Result<(), String> {
    match &stream.req_tx {
        Some(tx) => tx
            .send(frame(&req))
            .await
            .map_err(|_| "request stream closed".to_string()),
        None => Err("request stream closed".to_string()),
    }
}

/// 応答 1 つを適用し、次に送る要求（ACK/NACK）の状態を作る。
fn handle(resp: DiscoveryResponse, cfg: &VdsConfig, config_path: &Path, state: &mut State) {
    let nonce = resp.nonce.clone();
    let result = resources_to_json(&resp.resources)
        .map_err(|e| (nonce, e))
        .and_then(|res| {
            crate::vds::apply_snapshot(
                resp.version_info,
                resp.nonce,
                &resp.type_url,
                &res,
                cfg,
                config_path,
            )
        });
    match result {
        Ok((v, n)) => {
            crate::vds::record_applied(&v);
            state.version_info = v;
            state.nonce = n;
            state.error_detail = None;
        }
        Err((n, e)) => {
            crate::vds::record_rejected(&state.version_info, &e);
            state.nonce = n;
            state.error_detail = Some(e);
        }
    }
}

/// `grpc-status` が 0 以外ならその説明。
fn grpc_error(fields: &[(Bytes, Bytes)]) -> Option<String> {
    let get = |n: &[u8]| {
        fields
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(n))
            .map(|(_, v)| v)
    };
    let status = get(b"grpc-status")?;
    if status.as_ref() == b"0" {
        return None;
    }
    let msg = get(b"grpc-message")
        .and_then(|m| crate::grpc::status::GrpcStatus::decode_message(m))
        .unwrap_or_default();
    Some(format!(
        "grpc-status {}: {}",
        String::from_utf8_lossy(status),
        msg
    ))
}

/// TCP キープアライブ（応答が長く来ないストリームで相手の消失を検知する）。
#[cfg(unix)]
fn set_keepalive(tcp: &crate::runtime::tcp::TcpStream, secs: u64) {
    use crate::runtime::handle::AsRawFd;
    if secs == 0 {
        return;
    }
    let fd = tcp.as_raw_fd();
    let set = |level: libc::c_int, opt: libc::c_int, v: libc::c_int| unsafe {
        // SAFETY: 有効なソケット fd に int のオプションを設定するだけ
        libc::setsockopt(
            fd,
            level,
            opt,
            &v as *const _ as *const libc::c_void,
            std::mem::size_of::<libc::c_int>() as libc::socklen_t,
        )
    };
    set(libc::SOL_SOCKET, libc::SO_KEEPALIVE, 1);
    let secs = secs.min(i32::MAX as u64) as libc::c_int;
    #[cfg(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd"
    ))]
    {
        set(libc::IPPROTO_TCP, libc::TCP_KEEPIDLE, secs);
        set(libc::IPPROTO_TCP, libc::TCP_KEEPINTVL, (secs / 3).max(1));
        set(libc::IPPROTO_TCP, libc::TCP_KEEPCNT, 3);
    }
    #[cfg(target_os = "macos")]
    set(libc::IPPROTO_TCP, libc::TCP_KEEPALIVE, secs);
    #[cfg(not(any(
        target_os = "linux",
        target_os = "android",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "macos"
    )))]
    let _ = secs;
}

#[cfg(not(unix))]
fn set_keepalive(_tcp: &crate::runtime::tcp::TcpStream, _secs: u64) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn grpc_resources_become_rest_shape() {
        let res = vec![
            Resource {
                type_url: crate::vds::TYPE_ROUTE.into(),
                name: "r1".into(),
                json: r#"{"conditions":{"path":"/a"},"action":{"type":"Proxy","upstream":"pool"}}"#
                    .into(),
            },
            Resource {
                type_url: crate::vds::TYPE_UPSTREAM.into(),
                name: "pool".into(),
                json: r#"{"servers":["127.0.0.1:1"]}"#.into(),
            },
        ];
        let json = resources_to_json(&res).unwrap();
        let ov = crate::vds::resources_to_overlay(&json, 10).unwrap();
        assert_eq!(ov["route"][0]["conditions"]["path"].as_str(), Some("/a"));
        assert_eq!(
            ov["upstreams"]["pool"]["servers"][0].as_str(),
            Some("127.0.0.1:1")
        );
        assert!(resources_to_json(&[Resource {
            json: "[1]".into(),
            ..Default::default()
        }])
        .unwrap_err()
        .contains("JSON object"));
        assert!(resources_to_json(&[Resource {
            json: "{".into(),
            ..Default::default()
        }])
        .is_err());
    }

    #[test]
    fn ack_and_nack_requests() {
        let node = Node {
            id: "n".into(),
            ..Default::default()
        };
        let mut st = State::default();
        let r = st.request(&node);
        assert_eq!(r.version_info, "");
        assert_eq!(r.type_url, TYPE_SNAPSHOT);
        st.version_info = "v1".into();
        st.nonce = "n2".into();
        st.error_detail = Some("bad".into());
        let r = st.request(&node);
        assert_eq!(r.version_info, "v1");
        assert_eq!(r.response_nonce, "n2");
        assert_eq!(r.error_detail.as_ref().unwrap().message, "bad");
        // NACK は 1 回だけ
        assert!(st.request(&node).error_detail.is_none());
        // ワイヤ形式の往復
        let b = frame(&r);
        assert_eq!(b[0], 0);
        assert_eq!(
            DiscoveryRequest::decode(&b[5..]).unwrap().response_nonce,
            "n2"
        );
    }

    #[test]
    fn grpc_status_errors() {
        let ok = vec![(Bytes::from_static(b"grpc-status"), Bytes::from_static(b"0"))];
        assert!(grpc_error(&ok).is_none());
        let bad = vec![
            (
                Bytes::from_static(b"grpc-status"),
                Bytes::from_static(b"16"),
            ),
            (
                Bytes::from_static(b"grpc-message"),
                Bytes::from_static(b"no%20token"),
            ),
        ];
        assert_eq!(grpc_error(&bad).unwrap(), "grpc-status 16: no token");
        assert!(grpc_error(&[]).is_none());
    }
}
