//! メッシュの HTTP/2（F-219）。下流のフレーム処理は `proxy.rs` の HTTP/2 の駆動部（F-116 の
//! アクターモデル）をそのまま使い、per-stream の処理だけをメッシュのもの（[`MeshH2Service`]）に差し替える。
//!
//! - 要求本文は常にストリーミング（gRPC の双方向ストリームを全二重で中継する）。
//! - 上流が HTTP/2 なら多重化接続のストリームで、HTTP/1.1 なら要求を HTTP/1.1 に直して送る。

use super::httpcore::ResponseFlag;
use super::l7::{self, ConnCtx, Decision, Forward, LocalReply, ReqView};
use super::upstream::{self, Alpn};
use crate::http2::upstream_mux::UpResp;
use crate::proxy::{H2RequestCtx, H2RespMsg, H2Service};
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};
use crate::stream_channel::{Notify, Receiver, Sender};
use bytes::{BufMut, Bytes, BytesMut};
use std::future::{Future, poll_fn};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::task::Poll;
use std::time::{Duration, Instant};

pub struct MeshH2Service {
    pub ctx: std::cell::RefCell<Arc<ConnCtx>>,
}

impl H2Service for MeshH2Service {
    fn streaming_plan<S>(
        &self,
        _conn: &crate::http2::Http2Connection<S>,
        _stream_id: u32,
        _client_ip: &str,
        _client_socket_addr: std::net::SocketAddr,
    ) -> Option<u64>
    where
        S: AsyncReadRent + AsyncWriteRentExt + Unpin,
    {
        Some(0)
    }

    fn spawn(
        &self,
        ctx: H2RequestCtx,
        req_rx: Option<Receiver<Bytes>>,
        resp_tx: Sender<H2RespMsg>,
        notify: Notify,
    ) {
        // LDS の更新で設定が変わっていれば選び直す（HTTP でなくなったら古い設定のまま。接続は GOAWAY されない）
        let refreshed = self.ctx.borrow().refreshed();
        if let Some(r) = refreshed {
            *self.ctx.borrow_mut() = r;
        }
        let c = self.ctx.borrow().clone();
        crate::runtime::spawn(async move {
            task(c, ctx, req_rx, &resp_tx, &notify).await;
            drop(resp_tx);
            notify.notify();
        });
    }
}

/// 下流の HTTP/2 接続（h2c・TLS の h2）を処理する。
pub async fn serve<S>(io: S, initial: Vec<u8>, ctx: Arc<ConnCtx>)
where
    S: AsyncReadRent
        + AsyncWriteRentExt
        + crate::runtime::handle::AsRawFd
        + crate::runtime::io::BufferedReadState
        + Unpin
        + 'static,
{
    let settings = crate::config::CURRENT_CONFIG
        .load()
        .http2_config
        .to_http2_settings();
    let mut conn = crate::http2::Http2Connection::new_with_initial_buffer(io, settings, initial);
    if let Err(e) = conn.handshake().await {
        ftlog::debug!("[mesh] h2 handshake: {}", e);
        return;
    }
    let mut metric = crate::metrics::ActiveConnectionMetric::new(false);
    let svc = MeshH2Service {
        ctx: std::cell::RefCell::new(ctx.clone()),
    };
    let ip = ctx.peer.ip().to_string();
    if let Err(e) =
        crate::proxy::handle_http2_requests_with(&mut conn, &ip, &mut metric, false, &svc).await
    {
        ftlog::debug!("[mesh] h2 connection: {}", e);
    }
}

async fn send(tx: &Sender<H2RespMsg>, notify: &Notify, msg: H2RespMsg) -> bool {
    let r = tx.send(msg).await.is_ok();
    notify.notify();
    r
}

/// CONNECT を終端して中身を内部リスナーへ渡す（ambient の waypoint の HBONE。F-224）。
async fn connect_terminate(
    c: &Arc<ConnCtx>,
    f: &Forward,
    authority: &[u8],
    req_rx: Option<Receiver<Bytes>>,
    tx: &Sender<H2RespMsg>,
    notify: &Notify,
) {
    let reply = |status: u16| H2RespMsg::Head {
        status,
        headers: Vec::new(),
        end_stream: true,
    };
    let Some(dst) = super::hbone::parse_authority(authority) else {
        send(tx, notify, reply(400)).await;
        return;
    };
    // 終端先のクラスタのエンドポイントは内部リスナー（`main_internal`）
    let lst = f
        .cluster
        .endpoints
        .load()
        .endpoints
        .iter()
        .find_map(|e| e.internal.clone())
        .and_then(|name| super::state::state().listeners.load().internal.get(&*name).cloned());
    let Some(lst) = lst else {
        ftlog::debug!("[mesh] CONNECT {}: no internal listener behind {}", dst, f.cluster.name);
        send(tx, notify, reply(503)).await;
        return;
    };
    if !send(
        tx,
        notify,
        H2RespMsg::Head {
            status: 200,
            headers: Vec::new(),
            end_stream: false,
        },
    )
    .await
    {
        return;
    }
    let inherited = super::server::Inherited {
        principal: c.peer_principal.clone(),
        cert_hash: c.peer_cert_hash.clone(),
        dns: c.peer_dns.clone(),
    };
    super::hbone::terminate(lst, c.peer, dst, inherited, req_rx, notify.clone(), |b| {
        send(tx, notify, H2RespMsg::Body(b))
    })
    .await;
}

async fn local(
    tx: &Sender<H2RespMsg>,
    notify: &Notify,
    lr: &LocalReply,
    is_grpc: bool,
    hcm: &super::model::HttpCm,
) {
    let mut headers = lr.headers.clone();
    if hcm.linkerd {
        headers.push((
            Bytes::from_static(b"l5d-proxy-error"),
            Bytes::from_static(super::httpcore::reason(lr.status).as_bytes()),
        ));
    } else {
        headers.push((Bytes::from_static(b"server"), hcm.server_name.clone()));
    }
    if is_grpc && lr.grpc_status.is_some() {
        headers.push((
            Bytes::from_static(b"content-type"),
            Bytes::from_static(b"application/grpc"),
        ));
        let mut it = itoa::Buffer::new();
        headers.push((
            Bytes::from_static(b"grpc-status"),
            Bytes::copy_from_slice(it.format(lr.grpc_status.unwrap_or(2)).as_bytes()),
        ));
        if !lr.body.is_empty() {
            headers.push((Bytes::from_static(b"grpc-message"), lr.body.clone()));
        }
        // Trailers-Only（HEADERS に END_STREAM）
        send(
            tx,
            notify,
            H2RespMsg::Head {
                status: 200,
                headers,
                end_stream: true,
            },
        )
        .await;
        return;
    }
    if !lr.body.is_empty() {
        headers.push((
            Bytes::from_static(b"content-type"),
            Bytes::from_static(b"text/plain"),
        ));
    }
    let mut it = itoa::Buffer::new();
    headers.push((
        Bytes::from_static(b"content-length"),
        Bytes::copy_from_slice(it.format(lr.body.len()).as_bytes()),
    ));
    let empty = lr.body.is_empty();
    if send(
        tx,
        notify,
        H2RespMsg::Head {
            status: lr.status,
            headers,
            end_stream: empty,
        },
    )
    .await
        && !empty
    {
        send(tx, notify, H2RespMsg::Body(lr.body.clone())).await;
    }
}

async fn task(
    c: Arc<ConnCtx>,
    mut rq: H2RequestCtx,
    req_rx: Option<Receiver<Bytes>>,
    tx: &Sender<H2RespMsg>,
    notify: &Notify,
) {
    super::stats::DOWNSTREAM_RQ_TOTAL.fetch_add(1, Ordering::Relaxed);
    let start = Instant::now();
    let start_sys = if c.hcm.access_logs.is_empty() {
        std::time::UNIX_EPOCH
    } else {
        std::time::SystemTime::now()
    };
    let mut scheme = Bytes::from_static(b"http");
    let mut headers: Vec<(Bytes, Bytes)> = Vec::with_capacity(rq.headers.len() + 8);
    for h in &rq.headers {
        if h.name.as_ref() == b":scheme" {
            scheme = h.value.clone();
            continue;
        }
        if h.name.starts_with(b":") {
            continue;
        }
        headers.push((h.name.clone(), h.value.clone()));
    }
    let filtered = l7::request_filters(
        &c,
        &mut l7::ReqParts {
            method: &mut rq.method,
            path: &mut rq.path,
            authority: &mut rq.authority,
            headers: &mut headers,
            scheme: &scheme,
            eos: req_rx.is_none() && rq.body.is_empty(),
        },
    )
    .await;
    let view = ReqView {
        method: &rq.method,
        path: &rq.path,
        authority: &rq.authority,
        headers: &headers,
        scheme: &scheme,
    };
    let is_grpc = view.is_grpc();
    let req_len = headers
        .iter()
        .find(|(k, _)| k.as_ref() == b"content-length")
        .and_then(|(_, v)| std::str::from_utf8(v).ok()?.parse().ok())
        .unwrap_or(rq.body.len() as u64);
    let decision = match filtered {
        Ok((jwt, ran)) => l7::decide(&c, &view, jwt, ran),
        Err(lr) => Decision::Local(lr),
    };
    match decision {
        Decision::Local(lr) => {
            local(tx, notify, &lr, is_grpc, &c.hcm).await;
            let labels = c.labels(None, None, is_grpc);
            super::stats::record_request(
                &labels,
                lr.status,
                lr.grpc_status,
                lr.flag,
                start.elapsed().as_secs_f64() * 1000.0,
                req_len,
                lr.body.len() as u64,
            );
            l7::access_log(
                &c,
                &view,
                start_sys,
                "HTTP/2",
                lr.status,
                lr.flag,
                req_len,
                lr.body.len() as u64,
                start.elapsed().as_millis() as u64,
                None,
                None,
                "",
                None,
            );
        }
        Decision::Forward(f) if f.action.connect_terminate && rq.method.as_ref() == b"CONNECT" => {
            connect_terminate(&c, &f, &rq.authority, req_rx, tx, notify).await;
        }
        Decision::Forward(f) => {
            if let Some(d) = f.delay {
                crate::runtime::time::sleep(d).await;
            }
            let origin = view.header("origin");
            let o = forward(
                &c,
                &f,
                &rq,
                &headers,
                rq.body.clone(),
                req_rx,
                tx,
                notify,
                is_grpc,
                origin.as_ref(),
            )
            .await;
            let labels = c.labels(o.peer.clone(), o.svc.clone(), is_grpc);
            super::stats::record_request(
                &labels,
                o.status,
                o.grpc,
                o.flag,
                start.elapsed().as_secs_f64() * 1000.0,
                o.req_bytes.max(req_len),
                o.resp_bytes,
            );
            l7::access_log(
                &c,
                &view,
                start_sys,
                "HTTP/2",
                o.status,
                o.flag,
                o.req_bytes.max(req_len),
                o.resp_bytes,
                start.elapsed().as_millis() as u64,
                o.upstream,
                o.cluster.as_deref(),
                &f.route().name,
                o.upstream_ms,
            );
            l7::finish_span(&c, &f, &view, o.status, o.cluster.as_deref());
        }
    }
}

pub(crate) struct Outcome {
    pub status: u16,
    pub grpc: Option<u32>,
    pub flag: ResponseFlag,
    pub req_bytes: u64,
    pub resp_bytes: u64,
    pub peer: Option<Arc<super::peer::Workload>>,
    pub svc: Option<Arc<super::stats::ServiceLabels>>,
    /// アクセスログ用（上流のアドレス・クラスタ・上流の所要時間）
    pub upstream: Option<std::net::SocketAddr>,
    pub cluster: Option<Arc<super::model::ClusterCfg>>,
    pub upstream_ms: Option<u64>,
}

fn grpc_status_of(h: &[(Bytes, Bytes)]) -> Option<u32> {
    h.iter()
        .find(|(k, _)| k.as_ref() == b"grpc-status")
        .and_then(|(_, v)| std::str::from_utf8(v).ok()?.parse().ok())
}

#[allow(clippy::too_many_arguments)]
async fn forward(
    c: &ConnCtx,
    f: &Forward,
    rq: &H2RequestCtx,
    headers: &[(Bytes, Bytes)],
    first_body: Bytes,
    mut req_rx: Option<Receiver<Bytes>>,
    tx: &Sender<H2RespMsg>,
    notify: &Notify,
    is_grpc: bool,
    origin: Option<&Bytes>,
) -> Outcome {
    let svc = f
        .cluster
        .cfg
        .load()
        .as_ref()
        .as_ref()
        .and_then(|cc| cc.svc_labels.clone());
    let mut out = Outcome {
        status: 0,
        grpc: None,
        flag: ResponseFlag::None,
        req_bytes: first_body.len() as u64,
        resp_bytes: 0,
        peer: None,
        svc,
        upstream: None,
        cluster: None,
        upstream_ms: None,
    };
    let retry = f.action.retry.clone().or_else(|| f.vhost().retry.clone());
    let max_attempts = 1 + retry.as_ref().map_or(0, |r| r.num_retries);
    let new_path = l7::rewritten_path(f, &rq.path);
    let path: &[u8] = new_path.as_deref().unwrap_or(&rq.path);
    let authority: Bytes = f
        .action
        .host_rewrite
        .clone()
        .unwrap_or_else(|| rq.authority.clone());
    let deadline = f.action.timeout.map(|t| Instant::now() + t);
    let mut tried = Vec::new();
    let mut attempt = 0;
    loop {
        attempt += 1;
        // 本文がまだ届いている（チャネル）なら、上流へ流し始めた後はリトライできない
        let replayable = req_rx.is_none();
        let picked = match upstream::pick(&f.cluster, c.orig_dst, f.hash, &tried) {
            Ok(p) => p,
            Err(flag) => {
                out.status = 503;
                out.flag = flag;
                let mut lr = LocalReply::new(503, flag);
                if is_grpc {
                    lr.grpc_status = Some(14);
                }
                local(tx, notify, &lr, is_grpc, &c.hcm).await;
                return out;
            }
        };
        out.upstream = Some(picked.addr);
        out.cluster = Some(picked.cfg.clone());
        let (req_headers, peer_req) = l7::upstream_request_headers(c, f, headers, attempt, false, false);
        // Linkerd の inbound: 相手のプロキシが HTTP/1.1 から昇格させた要求（`l5d-orig-proto`）はアプリへ HTTP/1.1 で
        let up_h2 = l7::upstream_is_h2(&picked.cfg, true)
            && !(c.hcm.linkerd
                && headers
                    .iter()
                    .any(|(k, _)| k.eq_ignore_ascii_case(b"l5d-orig-proto")));
        let _admission = match upstream::admit(&f.cluster, &picked.cfg, up_h2) {
            Ok(a) => a,
            Err(flag) => {
                out.status = 503;
                out.flag = flag;
                let mut lr = LocalReply::new(503, flag);
                if is_grpc {
                    lr.grpc_status = Some(14);
                }
                local(tx, notify, &lr, is_grpc, &c.hcm).await;
                return out;
            }
        };
        if attempt == 1 && !f.action.mirrors.is_empty() && req_rx.is_none() {
            super::mirror::spawn_h1(f, c, &rq.method, path, &authority, headers, first_body.clone(), false);
        }
        if !up_h2 {
            return to_h1(
                c,
                f,
                &picked,
                &rq.method,
                path,
                &authority,
                req_headers,
                peer_req,
                first_body,
                req_rx,
                tx,
                notify,
                is_grpc,
                out,
                origin,
            )
            .await;
        }
        picked.begin();
        let started = Instant::now();
        let mux = match upstream::h2_mux(&picked).await {
            Ok(m) => m,
            Err(e) => {
                picked.end(false, true);
                tried.push(picked.addr);
                ftlog::debug!("[mesh] h2 connect to {} failed: {}", picked.addr, e);
                if attempt < max_attempts
                    && retry
                        .as_ref()
                        .is_some_and(|r| r.on_connect_failure || r.on_reset_before_request)
                {
                    continue;
                }
                out.status = 503;
                out.flag = ResponseFlag::UpstreamConnectFailure;
                out.peer = l7::peer_workload(c, peer_req, None, picked.endpoint());
                let mut lr = LocalReply::new(503, out.flag);
                if is_grpc {
                    lr.grpc_status = Some(14);
                }
                local(tx, notify, &lr, is_grpc, &c.hcm).await;
                return out;
            }
        };
        let no_body = req_rx.is_none() && first_body.is_empty();
        let Some(mut st) = mux.open(
            &rq.method,
            path,
            &authority,
            req_headers.iter().map(|(k, v)| (k.as_ref(), v.as_ref())),
            no_body,
        ) else {
            picked.end(false, true);
            tried.push(picked.addr);
            if attempt < max_attempts {
                continue;
            }
            out.status = 503;
            out.flag = ResponseFlag::UpstreamConnectFailure;
            local(
                tx,
                notify,
                &LocalReply::new(503, out.flag),
                is_grpc,
                &c.hcm,
            )
            .await;
            return out;
        };
        if !first_body.is_empty() {
            if let Some(t) = &st.req_tx {
                let _ = t.send(first_body.clone()).await;
            }
        }
        if req_rx.is_none() {
            st.finish_request();
        }
        // 要求本文（チャネル）と応答を同時に流す
        let mut head_sent = false;
        let mut retry_now = false;
        let mut done = false;
        while !done {
            let wait_left = deadline.map(|d| d.saturating_duration_since(Instant::now()));
            if wait_left == Some(Duration::ZERO) && !head_sent {
                out.status = 504;
                out.flag = ResponseFlag::UpstreamTimeout;
                let mut lr = LocalReply::new(504, out.flag);
                if is_grpc {
                    lr.grpc_status = Some(4);
                }
                local(tx, notify, &lr, is_grpc, &c.hcm).await;
                picked.end(false, true);
                return out;
            }
            enum Ev {
                Req(Option<Bytes>),
                Resp(Option<UpResp>),
                Timeout,
            }
            let ev = {
                let resp = st.resp_rx.recv();
                let mut resp = std::pin::pin!(resp);
                let reqf = async {
                    match &req_rx {
                        Some(r) => r.recv().await,
                        None => std::future::pending().await,
                    }
                };
                let mut reqf = std::pin::pin!(reqf);
                let sleep =
                    crate::runtime::time::sleep(wait_left.unwrap_or(Duration::from_secs(3600)));
                let mut sleep = std::pin::pin!(sleep);
                poll_fn(|cx| {
                    if let Poll::Ready(v) = resp.as_mut().poll(cx) {
                        return Poll::Ready(Ev::Resp(v));
                    }
                    if req_rx.is_some() {
                        if let Poll::Ready(v) = reqf.as_mut().poll(cx) {
                            return Poll::Ready(Ev::Req(v));
                        }
                    }
                    if wait_left.is_some() && sleep.as_mut().poll(cx).is_ready() {
                        return Poll::Ready(Ev::Timeout);
                    }
                    Poll::Pending
                })
                .await
            };
            match ev {
                Ev::Timeout => continue,
                Ev::Req(Some(chunk)) => {
                    // 消費したら駆動部を起こして受信ウィンドウを補充させる（B-123 と同じ。起こさないと
                    // 64KB を超える本文が WINDOW_UPDATE を待って止まる）
                    notify.notify();
                    out.req_bytes += chunk.len() as u64;
                    if let Some(t) = &st.req_tx {
                        let _ = t.send(chunk).await;
                    }
                }
                Ev::Req(None) => {
                    req_rx = None;
                    st.finish_request();
                }
                Ev::Resp(Some(UpResp::Head {
                    status,
                    mut headers,
                })) => {
                    let grpc = grpc_status_of(&headers);
                    if !head_sent
                        && attempt < max_attempts
                        && replayable
                        && retry
                            .as_ref()
                            .is_some_and(|r| l7::retry_on_status(r, status, grpc))
                    {
                        picked.end(status >= 500, matches!(status, 502..=504));
                        tried.push(picked.addr);
                        retry_now = true;
                        break;
                    }
                    let ums = started.elapsed().as_millis() as u64;
                    out.upstream_ms = Some(ums);
                    f.response_filters(status, &mut headers).await;
                    let peer_resp = l7::downstream_response_headers(
                        c,
                        Some(f),
                        &mut headers,
                        Some(ums),
                        origin,
                    );
                    out.peer = l7::peer_workload(c, peer_req.clone(), peer_resp, picked.endpoint());
                    out.status = status;
                    if grpc.is_some() {
                        out.grpc = grpc;
                    }
                    // Trailers-Only（grpc-status が HEADERS にある）なら END_STREAM
                    let end = grpc.is_some() && is_grpc;
                    if !send(
                        tx,
                        notify,
                        H2RespMsg::Head {
                            status,
                            headers,
                            end_stream: end,
                        },
                    )
                    .await
                    {
                        out.flag = ResponseFlag::DownstreamConnectionTermination;
                        done = true;
                    }
                    head_sent = true;
                    if end {
                        done = true;
                    }
                }
                Ev::Resp(Some(UpResp::Data(d))) => {
                    out.resp_bytes += d.len() as u64;
                    if !send(tx, notify, H2RespMsg::Body(d)).await {
                        out.flag = ResponseFlag::DownstreamConnectionTermination;
                        done = true;
                    }
                }
                Ev::Resp(Some(UpResp::Trailers(t))) => {
                    out.grpc = grpc_status_of(&t).or(out.grpc);
                    send(tx, notify, H2RespMsg::Trailers(t)).await;
                    done = true;
                }
                Ev::Resp(Some(UpResp::Reset { retryable })) => {
                    if !head_sent && retryable && attempt < max_attempts && replayable {
                        picked.end(false, true);
                        tried.push(picked.addr);
                        retry_now = true;
                        break;
                    }
                    if !head_sent {
                        out.status = 503;
                        out.flag = ResponseFlag::UpstreamConnectionTermination;
                        let mut lr = LocalReply::new(503, out.flag);
                        if is_grpc {
                            lr.grpc_status = Some(14);
                        }
                        local(tx, notify, &lr, is_grpc, &c.hcm).await;
                    } else {
                        out.flag = ResponseFlag::UpstreamConnectionTermination;
                        send(tx, notify, H2RespMsg::Reset(0x2)).await;
                    }
                    done = true;
                }
                Ev::Resp(None) => {
                    if !head_sent {
                        out.status = 503;
                        out.flag = ResponseFlag::UpstreamConnectionTermination;
                        local(
                            tx,
                            notify,
                            &LocalReply::new(503, out.flag),
                            is_grpc,
                            &c.hcm,
                        )
                        .await;
                    }
                    done = true;
                }
            }
        }
        if retry_now {
            continue;
        }
        picked.end(out.status >= 500, matches!(out.status, 502..=504));
        return out;
    }
}

/// HTTP/2 の下流 → HTTP/1.1 の上流。
#[allow(clippy::too_many_arguments)]
async fn to_h1(
    c: &ConnCtx,
    f: &Forward,
    picked: &upstream::Picked,
    method: &Bytes,
    path: &[u8],
    authority: &Bytes,
    req_headers: Vec<(Bytes, Bytes)>,
    peer_req: Option<Arc<super::peer::Workload>>,
    first_body: Bytes,
    req_rx: Option<Receiver<Bytes>>,
    tx: &Sender<H2RespMsg>,
    notify: &Notify,
    is_grpc: bool,
    mut out: Outcome,
    origin: Option<&Bytes>,
) -> Outcome {
    picked.begin();
    let started = Instant::now();
    let (mut up, uses) = match upstream::h1_take(picked) {
        Some(x) => x,
        None => match upstream::connect(picked, Alpn::H1).await {
            Ok(u) => (u, 0),
            Err(_) => {
                picked.end(false, true);
                out.status = 503;
                out.flag = ResponseFlag::UpstreamConnectFailure;
                local(
                    tx,
                    notify,
                    &LocalReply::new(503, out.flag),
                    is_grpc,
                    &c.hcm,
                )
                .await;
                return out;
            }
        },
    };
    let has_len = req_headers
        .iter()
        .any(|(k, _)| k.as_ref() == b"content-length");
    let streaming = req_rx.is_some() && !has_len;
    let mut head = BytesMut::with_capacity(512);
    head.put_slice(method);
    head.put_u8(b' ');
    head.put_slice(path);
    head.put_slice(b" HTTP/1.1\r\nhost: ");
    head.put_slice(authority);
    head.put_slice(b"\r\n");
    for (k, v) in &req_headers {
        if k.as_ref() == b"host" || k.as_ref() == b"te" {
            continue;
        }
        head.put_slice(k);
        head.put_slice(b": ");
        head.put_slice(v);
        head.put_slice(b"\r\n");
    }
    if streaming {
        head.put_slice(b"transfer-encoding: chunked\r\n");
    } else if !has_len && req_rx.is_none() && !first_body.is_empty() {
        let mut it = itoa::Buffer::new();
        head.put_slice(b"content-length: ");
        head.put_slice(it.format(first_body.len()).as_bytes());
        head.put_slice(b"\r\n");
    }
    head.put_slice(b"\r\n");
    let write_chunk = |b: Bytes| -> Bytes {
        if streaming {
            let mut c = BytesMut::with_capacity(b.len() + 16);
            c.put_slice(format!("{:x}\r\n", b.len()).as_bytes());
            c.put_slice(&b);
            c.put_slice(b"\r\n");
            c.freeze()
        } else {
            b
        }
    };
    let ok = async {
        up.write_all(head.freeze()).await.0?;
        if !first_body.is_empty() {
            up.write_all(write_chunk(first_body.clone())).await.0?;
        }
        if let Some(rx) = &req_rx {
            while let Some(b) = rx.recv().await {
                notify.notify();
                out.req_bytes += b.len() as u64;
                if !b.is_empty() {
                    up.write_all(write_chunk(b)).await.0?;
                }
            }
            if streaming {
                up.write_all(Bytes::from_static(b"0\r\n\r\n")).await.0?;
            }
        }
        Ok::<(), std::io::Error>(())
    }
    .await;
    if ok.is_err() {
        picked.end(false, true);
        out.status = 503;
        out.flag = ResponseFlag::UpstreamConnectionTermination;
        local(
            tx,
            notify,
            &LocalReply::new(503, out.flag),
            is_grpc,
            &c.hcm,
        )
        .await;
        return out;
    }
    // 応答
    let mut rbuf = BytesMut::with_capacity(16 * 1024);
    let head = loop {
        if let Some(i) = memchr::memmem::find(&rbuf, b"\r\n\r\n") {
            break rbuf.split_to(i + 4).freeze();
        }
        let b: Vec<u8> = Vec::with_capacity(16 * 1024);
        let (r, b) = up.read(b).await;
        match r {
            Ok(n) if n > 0 => rbuf.extend_from_slice(&b[..n]),
            _ => {
                picked.end(false, true);
                out.status = 503;
                out.flag = ResponseFlag::UpstreamConnectionTermination;
                local(
                    tx,
                    notify,
                    &LocalReply::new(503, out.flag),
                    is_grpc,
                    &c.hcm,
                )
                .await;
                return out;
            }
        }
    };
    let mut hb = [httparse::EMPTY_HEADER; 100];
    let mut resp = httparse::Response::new(&mut hb);
    if !matches!(resp.parse(&head), Ok(httparse::Status::Complete(_))) {
        picked.end(false, true);
        out.status = 502;
        local(
            tx,
            notify,
            &LocalReply::new(502, ResponseFlag::None),
            is_grpc,
            &c.hcm,
        )
        .await;
        return out;
    }
    let status = resp.code.unwrap_or(502);
    let mut headers = Vec::with_capacity(resp.headers.len());
    let mut len: Option<u64> = None;
    let mut chunked = false;
    let mut keep = resp.version != Some(0);
    for h in resp.headers.iter() {
        let name = h.name.to_ascii_lowercase();
        match name.as_str() {
            "content-length" => {
                len = std::str::from_utf8(h.value)
                    .ok()
                    .and_then(|s| s.trim().parse().ok())
            }
            "transfer-encoding" => {
                chunked = h
                    .value
                    .windows(7)
                    .any(|w| w.eq_ignore_ascii_case(b"chunked"))
            }
            "connection" => {
                if h.value.windows(5).any(|w| w.eq_ignore_ascii_case(b"close")) {
                    keep = false;
                }
                continue;
            }
            _ => {}
        }
        if super::httpcore::is_hop_by_hop(name.as_bytes()) {
            continue;
        }
        headers.push((Bytes::from(name), Bytes::copy_from_slice(h.value)));
    }
    let ums = started.elapsed().as_millis() as u64;
    out.upstream_ms = Some(ums);
    f.response_filters(status, &mut headers).await;
    let peer_resp = l7::downstream_response_headers(c, Some(f), &mut headers, Some(ums), origin);
    // Linkerd の inbound で HTTP/1.1 に戻した要求の応答には元の版を付ける（相手のプロキシが外す）
    if c.hcm.linkerd && c.direction == super::model::Direction::Inbound {
        headers.push((
            Bytes::from_static(b"l5d-orig-proto"),
            Bytes::from_static(b"HTTP/1.1"),
        ));
    }
    out.peer = l7::peer_workload(c, peer_req, peer_resp, picked.endpoint());
    out.status = status;
    let no_body = method.as_ref() == b"HEAD" || status == 204 || status == 304 || len == Some(0);
    if !send(
        tx,
        notify,
        H2RespMsg::Head {
            status,
            headers,
            end_stream: no_body,
        },
    )
    .await
    {
        picked.end(false, false);
        return out;
    }
    if !no_body {
        // 本文（Content-Length / chunked / 接続が閉じるまで）
        let mut remaining = len;
        let mut sc_buf = BytesMut::new();
        loop {
            if rbuf.is_empty() {
                if remaining == Some(0) {
                    break;
                }
                let b: Vec<u8> = Vec::with_capacity(16 * 1024);
                let (r, b) = up.read(b).await;
                match r {
                    Ok(n) if n > 0 => rbuf.extend_from_slice(&b[..n]),
                    _ => {
                        keep = false;
                        break;
                    }
                }
            }
            if chunked {
                sc_buf.extend_from_slice(&rbuf.split());
                // chunked をほどいて中身だけ送る
                loop {
                    let Some(i) = memchr::memmem::find(&sc_buf, b"\r\n") else {
                        break;
                    };
                    let size = usize::from_str_radix(
                        std::str::from_utf8(&sc_buf[..i])
                            .unwrap_or("")
                            .split(';')
                            .next()
                            .unwrap_or("")
                            .trim(),
                        16,
                    )
                    .unwrap_or(0);
                    if size == 0 {
                        remaining = Some(0);
                        rbuf.clear();
                        sc_buf.clear();
                        break;
                    }
                    if sc_buf.len() < i + 2 + size + 2 {
                        break;
                    }
                    let _ = sc_buf.split_to(i + 2);
                    let data = sc_buf.split_to(size).freeze();
                    let _ = sc_buf.split_to(2);
                    out.resp_bytes += data.len() as u64;
                    if !send(tx, notify, H2RespMsg::Body(data)).await {
                        keep = false;
                        remaining = Some(0);
                        break;
                    }
                }
                if remaining == Some(0) {
                    break;
                }
                continue;
            }
            let take = match remaining {
                Some(r) => (r as usize).min(rbuf.len()),
                None => rbuf.len(),
            };
            let data = rbuf.split_to(take).freeze();
            if let Some(r) = remaining.as_mut() {
                *r -= take as u64;
            }
            out.resp_bytes += data.len() as u64;
            if !send(tx, notify, H2RespMsg::Body(data)).await {
                keep = false;
                break;
            }
            if remaining == Some(0) {
                break;
            }
        }
    }
    picked.end(status >= 500, matches!(status, 502..=504));
    if keep && rbuf.is_empty() && (len.is_some() || chunked) {
        upstream::h1_put(picked, up, uses + 1);
    }
    out
}
