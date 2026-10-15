//! Linkerd の tap（制御ポート 4190。`linkerd viz tap / top`。F-223）。
//!
//! - viz の tap サービス（ID は `LINKERD2_PROXY_TAP_SVC_NAME`）からの mTLS だけを受け、
//!   `io.linkerd.proxy.tap.Tap/Observe` に答える（HTTP/2 の gRPC、サーバストリーミング）。
//! - 購読があるときだけ、HTTP の要求の終わりに 1 本につき 3 つのイベント（要求の開始・応答の開始・応答の終わり）を
//!   条件（宛先・送信元・HTTP の属性・宛先のラベル）に合う購読者の待ち行列へ積む。購読が無ければ原子的な数を
//!   1 回読むだけ。

use super::proto as p;
use crate::mesh::detect::{self, Detected};
use crate::proxy::{H2RequestCtx, H2RespMsg, H2Service};
use crate::runtime::io::{AsyncReadRent, AsyncWriteRentExt};
use crate::runtime::tcp::TcpStream;
use crate::stream_channel::{Notify, Receiver, Sender};
use bytes::{BufMut, Bytes, BytesMut};
use prost::Message;
use std::collections::{HashMap, VecDeque};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OBSERVE: &[u8] = b"/io.linkerd.proxy.tap.Tap/Observe";

/// 購読の数（0 なら要求ごとの処理を何もしない）
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static TAPS: Mutex<Vec<Arc<Tap>>> = Mutex::new(Vec::new());
static NEXT_STREAM: AtomicU64 = AtomicU64::new(1);

struct Tap {
    m: Option<p::TapMatch>,
    queue: Mutex<VecDeque<Bytes>>,
}

/// 要求 1 本の tap 用の属性。
pub struct TapReq<'a> {
    pub inbound: bool,
    pub source: SocketAddr,
    pub destination: SocketAddr,
    pub dst_labels: Option<&'a str>,
    pub client_id: Option<&'a str>,
    pub method: &'a [u8],
    pub scheme: &'a [u8],
    pub authority: &'a [u8],
    pub path: &'a [u8],
    pub status: u16,
    pub grpc: Option<u32>,
    pub latency: Duration,
    pub resp_bytes: u64,
}

pub fn active() -> bool {
    ACTIVE.load(Ordering::Relaxed) > 0
}

fn labels_map(s: Option<&str>) -> HashMap<String, String> {
    let mut m = HashMap::new();
    let Some(s) = s else { return m };
    for kv in s.split(',') {
        if let Some((k, v)) = kv.split_once('=') {
            let k = k.trim_start_matches("dst_");
            m.insert(k.to_string(), v.trim_matches('"').to_string());
        }
    }
    m
}

fn net_match(t: &p::TapTcp, a: SocketAddr) -> bool {
    match &t.r#match {
        Some(p::tap_tcp::Match::Ports(r)) => {
            let port = a.port() as u32;
            port >= r.min && (r.max == 0 || port <= r.max)
        }
        Some(p::tap_tcp::Match::Netmask(n)) => match n.ip.as_ref().and_then(|i| i.to_ip()) {
            Some(ip) => crate::mesh::model::Cidr {
                addr: ip,
                len: n.mask as u8,
            }
            .contains(a.ip()),
            None => true,
        },
        None => true,
    }
}

fn str_match(m: &p::TapStringMatch, v: &[u8]) -> bool {
    match &m.r#match {
        Some(p::tap_string_match::Match::Exact(e)) => v == e.as_bytes(),
        Some(p::tap_string_match::Match::Prefix(pf)) => v.starts_with(pf.as_bytes()),
        None => true,
    }
}

fn matches(m: &p::TapMatch, r: &TapReq<'_>, dst: &HashMap<String, String>) -> bool {
    use p::tap_match::Match as M;
    match &m.r#match {
        Some(M::All(s)) => s.matches.iter().all(|x| matches(x, r, dst)),
        Some(M::Any(s)) => s.matches.is_empty() || s.matches.iter().any(|x| matches(x, r, dst)),
        Some(M::Not(x)) => !matches(x, r, dst),
        Some(M::Source(t)) => net_match(t, r.source),
        Some(M::Destination(t)) => net_match(t, r.destination),
        Some(M::DestinationLabel(l)) => dst.get(&l.key).is_some_and(|v| *v == l.value),
        Some(M::RouteLabel(_)) => true,
        Some(M::Http(h)) => match &h.r#match {
            Some(p::tap_http::Match::Method(mm)) => mm
                .as_str()
                .is_some_and(|x| x.as_bytes().eq_ignore_ascii_case(r.method)),
            Some(p::tap_http::Match::Authority(s)) => str_match(s, r.authority),
            Some(p::tap_http::Match::Path(s)) => str_match(s, r.path),
            Some(p::tap_http::Match::Scheme(_)) | None => true,
        },
        None => true,
    }
}

fn frame(m: &p::TapEvent) -> Bytes {
    let len = m.encoded_len();
    let mut b = BytesMut::with_capacity(5 + len);
    b.put_u8(0);
    b.put_u32(len as u32);
    let _ = m.encode(&mut b);
    b.freeze()
}

fn method(m: &[u8]) -> p::HttpMethod {
    const R: [&[u8]; 9] = [
        b"GET", b"POST", b"PUT", b"DELETE", b"PATCH", b"OPTIONS", b"CONNECT", b"HEAD", b"TRACE",
    ];
    let t = match R.iter().position(|x| x.eq_ignore_ascii_case(m)) {
        Some(i) => p::http_method::Type::Registered(i as i32),
        None => p::http_method::Type::Unregistered(String::from_utf8_lossy(m).into_owned()),
    };
    p::HttpMethod { r#type: Some(t) }
}

fn dur(d: Duration) -> p::Duration {
    p::Duration {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}

/// 要求の終わりに呼ぶ（購読者が居るときだけ）。
pub fn emit(r: &TapReq<'_>) {
    let taps: Vec<Arc<Tap>> = TAPS.lock().map(|t| t.clone()).unwrap_or_default();
    if taps.is_empty() {
        return;
    }
    let dst = labels_map(r.dst_labels);
    let mut src_meta = HashMap::new();
    if let Some(c) = r.client_id {
        src_meta.insert("tls".to_string(), "true".to_string());
        src_meta.insert("client_id".to_string(), c.to_string());
    }
    let id = p::TapStreamId {
        base: 0,
        stream: NEXT_STREAM.fetch_add(1, Ordering::Relaxed),
    };
    let base = |http: p::tap_http_event::Event| p::TapEvent {
        source: Some(p::TcpAddress {
            ip: Some(p::IpAddress::from_ip(r.source.ip())),
            port: r.source.port() as u32,
        }),
        source_meta: Some(p::EndpointMeta {
            labels: src_meta.clone(),
        }),
        route_meta: None,
        destination: Some(p::TcpAddress {
            ip: Some(p::IpAddress::from_ip(r.destination.ip())),
            port: r.destination.port() as u32,
        }),
        destination_meta: Some(p::EndpointMeta {
            labels: dst.clone(),
        }),
        proxy_direction: if r.inbound { 1 } else { 2 },
        http: Some(p::TapHttpEvent { event: Some(http) }),
    };
    let events = [
        frame(&base(p::tap_http_event::Event::RequestInit(
            p::TapRequestInit {
                id: Some(id.clone()),
                method: Some(method(r.method)),
                scheme: Some(p::Scheme {
                    r#type: Some(p::scheme::Type::Registered(
                        if r.scheme.eq_ignore_ascii_case(b"https") {
                            1
                        } else {
                            0
                        },
                    )),
                }),
                authority: String::from_utf8_lossy(r.authority).into_owned(),
                path: String::from_utf8_lossy(r.path).into_owned(),
            },
        ))),
        frame(&base(p::tap_http_event::Event::ResponseInit(
            p::TapResponseInit {
                id: Some(id.clone()),
                since_request_init: Some(dur(r.latency)),
                http_status: r.status as u32,
            },
        ))),
        frame(&base(p::tap_http_event::Event::ResponseEnd(
            p::TapResponseEnd {
                id: Some(id),
                since_request_init: Some(dur(r.latency)),
                since_response_init: Some(dur(Duration::ZERO)),
                response_bytes: r.resp_bytes,
                eos: r.grpc.map(|g| p::TapEos {
                    end: Some(p::tap_eos::End::GrpcStatusCode(g)),
                }),
            },
        ))),
    ];
    for t in taps {
        if t.m.as_ref().is_none_or(|m| matches(m, r, &dst)) {
            if let Ok(mut q) = t.queue.lock() {
                if q.len() < 3 * 4096 {
                    q.extend(events.iter().cloned());
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// 制御ポート
// ---------------------------------------------------------------------------

pub async fn serve(cfg: &super::env::Config, stream: TcpStream, peer: SocketAddr) {
    let Some(tap_name) = cfg.tap_name.as_deref() else {
        return;
    };
    let (det, initial) = detect::sniff(&stream, Some(Duration::from_secs(10))).await;
    let Detected::Tls { sni: Some(sni), .. } = det else {
        return;
    };
    if sni != cfg.local_name {
        return;
    }
    let Some((mut ds, client)) = super::conn::accept_tls(stream, initial, peer).await else {
        return;
    };
    // tap の制御プレーンの ID だけを受ける（本家と同じ）
    if client.as_deref() != Some(tap_name) {
        ftlog::debug!("[linkerd] tap from {:?} refused", client);
        return;
    }
    let rest = if ds.alpn_protocol() == Some(p::TRANSPORT_ALPN) {
        match super::conn::read_transport_header(&mut ds).await {
            Some((_, rest)) => rest,
            None => return,
        }
    } else {
        Vec::new()
    };
    let (det, acc) = super::conn::read_classify(&mut ds, rest).await;
    if det != Detected::H2c {
        return;
    }
    let settings = crate::config::CURRENT_CONFIG
        .load()
        .http2_config
        .to_http2_settings();
    let mut conn = crate::http2::Http2Connection::new_with_initial_buffer(ds, settings, acc);
    if conn.handshake().await.is_err() {
        return;
    }
    let mut metric = crate::metrics::ActiveConnectionMetric::new(false);
    let ip = peer.ip().to_string();
    let _ =
        crate::proxy::handle_http2_requests_with(&mut conn, &ip, &mut metric, false, &TapH2).await;
}

struct TapH2;

impl H2Service for TapH2 {
    fn streaming_plan<S>(
        &self,
        _conn: &crate::http2::Http2Connection<S>,
        _stream_id: u32,
        _client_ip: &str,
        _client_socket_addr: SocketAddr,
    ) -> Option<u64>
    where
        S: AsyncReadRent + AsyncWriteRentExt + Unpin,
    {
        None
    }

    fn spawn(
        &self,
        ctx: H2RequestCtx,
        _req_rx: Option<Receiver<Bytes>>,
        resp_tx: Sender<H2RespMsg>,
        notify: Notify,
    ) {
        crate::runtime::spawn(async move {
            observe(ctx, &resp_tx, &notify).await;
            drop(resp_tx);
            notify.notify();
        });
    }
}

async fn send(tx: &Sender<H2RespMsg>, notify: &Notify, m: H2RespMsg) -> bool {
    let r = tx.send(m).await.is_ok();
    notify.notify();
    r
}

fn grpc_trailers(code: u32) -> H2RespMsg {
    H2RespMsg::Trailers(vec![(
        Bytes::from_static(b"grpc-status"),
        Bytes::from(code.to_string()),
    )])
}

async fn observe(ctx: H2RequestCtx, tx: &Sender<H2RespMsg>, notify: &Notify) {
    let head = H2RespMsg::Head {
        status: 200,
        headers: vec![(
            Bytes::from_static(b"content-type"),
            Bytes::from_static(b"application/grpc"),
        )],
        end_stream: false,
    };
    if ctx.path.as_ref() != OBSERVE {
        send(tx, notify, head).await;
        send(tx, notify, grpc_trailers(12)).await;
        return;
    }
    let req = if ctx.body.len() >= 5 {
        p::ObserveRequest::decode(&ctx.body[5..]).unwrap_or_default()
    } else {
        p::ObserveRequest::default()
    };
    let tap = Arc::new(Tap {
        m: req.r#match.clone(),
        queue: Mutex::new(VecDeque::new()),
    });
    if let Ok(mut t) = TAPS.lock() {
        t.push(tap.clone());
    }
    ACTIVE.fetch_add(1, Ordering::Relaxed);
    let limit = if req.limit == 0 {
        u64::MAX
    } else {
        req.limit as u64
    };
    let mut sent_reqs = 0u64;
    let mut ok = send(tx, notify, head).await;
    while ok && sent_reqs < limit && !crate::config::SHUTDOWN_FLAG.load(Ordering::Relaxed) {
        let batch: Vec<Bytes> = tap
            .queue
            .lock()
            .map(|mut q| q.drain(..).collect())
            .unwrap_or_default();
        if batch.is_empty() {
            if tx.is_closed() {
                break;
            }
            crate::runtime::time::sleep(Duration::from_millis(20)).await;
            continue;
        }
        for (i, ev) in batch.into_iter().enumerate() {
            if !send(tx, notify, H2RespMsg::Body(ev)).await {
                ok = false;
                break;
            }
            // 要求の数で数える（1 本につき 3 つのイベント）
            if i % 3 == 2 {
                sent_reqs += 1;
            }
        }
    }
    ACTIVE.fetch_sub(1, Ordering::Relaxed);
    if let Ok(mut t) = TAPS.lock() {
        t.retain(|x| !Arc::ptr_eq(x, &tap));
    }
    if ok {
        send(tx, notify, grpc_trailers(0)).await;
    }
}
