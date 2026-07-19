//! ngtcp2 + nghttp3 HTTP/3 サーバ本体
//!
//! メインループが QUIC/H3 を専有し、プロキシは F-32 `http3::stream` アクターで
//! バックエンド TCP/TLS I/O を駆動する。

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use bytes::Bytes;
use ftlog::{debug, error, info, warn};
use nghttp3_sys::nghttp3_vec;

use crate::config::{
    resolve_http3_compression_config, AcceptedEncoding, Backend, CURRENT_CONFIG, SHUTDOWN_FLAG,
};
use crate::http3::common::{
    build_h1_request_head, compress_body_h3, compute_upstream_request_path, secure_zero,
    Http3ServerConfig,
};
use crate::http3::stream::{
    self as h3_stream, BackendSpawner, BackendTaskParams, H3Notify, Receiver, RespMsg, Sender,
    TryRecv, TrySendError,
};
use crate::logging::log_access;
use crate::metrics::{
    http3_stream_closed, http3_stream_opened, http3_streams_closed_n, Http3ActiveConnGuard,
};
use crate::pool::MAX_HEADER_SIZE;
use crate::proxy::{check_security, SecurityCheckResult};
use crate::udp::QuicUdpSocket;
use crate::upstream::find_backend_unified;

use super::conn::{
    accept_packet, extract_dcid, timestamp_ns, CidKey, QuicConn, QUIC_V1, SERVER_SCID_LEN,
};
use super::crypto::TlsContext;
use super::h3::{H3Conn, H3Event};

// 大容量ボディの往復でバックエンドが詰まらないよう、有界だが十分な深さにする
const REQ_CHAN_CAP: usize = 64;
const RESP_CHAN_CAP: usize = 64;
/// QUIC パケットスクラッチ容量（MTU 近傍）
const PKT_CAP: usize = 1350;

type ConnMap = Rc<RefCell<HashMap<CidKey, Handler>>>;
/// NEW_CONNECTION_ID で発行された SCID → 初期 SCID（主キー）
type CidAliasMap = Rc<RefCell<HashMap<CidKey, CidKey>>>;

// ホットパス用: 送信パケットバッファのスレッドローカルプール
thread_local! {
    static PKT_BUF_POOL: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
}

#[inline]
fn pkt_buf_get() -> Vec<u8> {
    PKT_BUF_POOL.with(|p| {
        let mut pool = p.borrow_mut();
        let mut v = pool.pop().unwrap_or_else(|| Vec::with_capacity(PKT_CAP));
        v.clear();
        if v.capacity() < PKT_CAP {
            v.reserve(PKT_CAP);
        }
        v
    })
}

#[inline]
fn pkt_buf_put(mut v: Vec<u8>) {
    v.clear();
    if v.capacity() >= PKT_CAP && v.capacity() <= PKT_CAP * 2 {
        PKT_BUF_POOL.with(|p| {
            let mut pool = p.borrow_mut();
            if pool.len() < 64 {
                pool.push(v);
            }
        });
    }
}

/// ストリーミングプロキシ状態（メインループ側）
struct ProxyStream {
    resp_rx: Receiver<RespMsg>,
    resp_started: bool,
    head_pending: Option<(u16, h3_stream::RespHeaders)>,
    body_pending: Option<(Bytes, usize)>,
    need_fin: bool,
    resp_fin_sent: bool,
    req_tx: Option<Sender<Bytes>>,
    req_pending: VecDeque<Bytes>,
    req_finished: bool,
    /// WASM モジュールがルートに付いているとき、レスポンスへ処理済みヘッダを付与
    inject_wasm_header: bool,
}

struct Handler {
    quic: QuicConn,
    h3: Option<H3Conn>,
    peer: SocketAddr,
    client_ip: String,
    /// バッファ経路: ボディ蓄積
    bodies: HashMap<i64, Vec<u8>>,
    /// バッファ経路: ヘッダ待機
    headers: HashMap<i64, Vec<(Vec<u8>, Vec<u8>)>>,
    /// ストリーミング中
    proxy_streams: HashMap<i64, ProxyStream>,
    open_streams: usize,
    notify: H3Notify,
    backend_spawner: BackendSpawner,
    _metric: Http3ActiveConnGuard,
}

impl Drop for Handler {
    fn drop(&mut self) {
        if self.open_streams > 0 {
            http3_streams_closed_n(self.open_streams);
            self.open_streams = 0;
        }
    }
}

impl Handler {
    fn new(quic: QuicConn, peer: SocketAddr, notify: H3Notify, spawner: BackendSpawner) -> Self {
        Self {
            quic,
            h3: None,
            client_ip: peer.ip().to_string(),
            peer,
            bodies: HashMap::new(),
            headers: HashMap::new(),
            proxy_streams: HashMap::new(),
            open_streams: 0,
            notify,
            backend_spawner: spawner,
            _metric: Http3ActiveConnGuard::new(),
        }
    }

    fn try_init_h3(&mut self) -> io::Result<()> {
        if self.h3.is_some() || !self.quic.is_handshake_completed() {
            return Ok(());
        }
        let mut h3 = H3Conn::server_new()?;
        let ctrl = self.quic.open_uni_stream()?;
        let qenc = self.quic.open_uni_stream()?;
        let qdec = self.quic.open_uni_stream()?;
        h3.bind_control_stream(ctrl)?;
        h3.bind_qpack_streams(qenc, qdec)?;
        self.h3 = Some(h3);
        debug!("[HTTP/3/ngtcp2] H3 established from {}", self.peer);
        Ok(())
    }

    fn feed_stream_data(&mut self) -> io::Result<()> {
        let ts = timestamp_ns();
        while let Some(sd) = self.quic.poll_stream_data() {
            if let Some(h3) = self.h3.as_mut() {
                match h3.read_stream(sd.stream_id, &sd.data, sd.fin, ts) {
                    Ok(n) => {
                        self.quic.extend_max_stream_offset(sd.stream_id, n as u64);
                    }
                    Err(e) => {
                        // QPACK / フレーム異常等。ヘッダ過大は process 側で 431、
                        // それ以外は汎用 400（413/WS 等を 431 に誤マップしない）。
                        warn!("[HTTP/3/ngtcp2] read_stream sid={}: {e}", sd.stream_id);
                        let msg = e.to_string();
                        let (code, body) = if msg.contains("header")
                            || msg.contains("field")
                            || msg.contains("QPACK")
                            || msg.contains("qpack")
                        {
                            (431, &b"Request Header Fields Too Large"[..])
                        } else {
                            (400, &b"Bad Request"[..])
                        };
                        let _ = self.send_error(sd.stream_id, code, body);
                    }
                }
            }
        }
        Ok(())
    }

    /// H3 イベント処理 + プロキシ駆動（同期。バックエンド I/O は spawn 済みタスク側）。
    fn process_h3_events(&mut self) -> io::Result<()> {
        let mut pending_headers = Vec::new();
        let mut pending_data = Vec::new();
        let mut finished = Vec::new();
        {
            let Some(h3) = self.h3.as_mut() else {
                return Ok(());
            };
            while let Some(ev) = h3.poll_event() {
                match ev {
                    H3Event::Headers { stream_id, headers } => {
                        pending_headers.push((stream_id, headers));
                    }
                    H3Event::Data { stream_id, data } => {
                        // h3 側が Vec で渡す → Bytes 化（以降ゼロコピー共有）
                        pending_data.push((stream_id, Bytes::from(data)));
                    }
                    H3Event::DataEnd { stream_id } => finished.push(stream_id),
                    H3Event::Reset { stream_id } => {
                        self.bodies.remove(&stream_id);
                        self.headers.remove(&stream_id);
                        self.proxy_streams.remove(&stream_id);
                    }
                }
            }
        }

        // 順序: Headers → Data → DataEnd。
        // 同一パケット内で Data が Headers より先にキューへ載る場合があり、
        // 先に Data を処理するとストリーム未作成のため bodies に吸い込まれ POST ボディが欠落する。
        for (sid, headers) in pending_headers {
            let size: usize = headers
                .iter()
                .map(|(n, v)| n.len().saturating_add(v.len()))
                .sum();
            if size > MAX_HEADER_SIZE {
                self.send_error(sid, 431, b"Request Header Fields Too Large")?;
                continue;
            }
            http3_stream_opened();
            self.open_streams = self.open_streams.saturating_add(1);
            match self.classify_and_maybe_stream(sid, &headers) {
                Classify::Stream => {
                    // 先行到着済みボディがあればストリームへ引き継ぐ
                    if let Some(early) = self.bodies.remove(&sid) {
                        if !early.is_empty() {
                            self.enqueue_req_body(sid, Bytes::from(early));
                        }
                    }
                }
                // Buffer / Handled は下の match で処理
                Classify::Buffer => {
                    self.headers.insert(sid, headers);
                }
                Classify::Handled => {
                    http3_stream_closed();
                    self.open_streams = self.open_streams.saturating_sub(1);
                    self.bodies.remove(&sid);
                }
            }
        }

        for (sid, data) in pending_data {
            if self.proxy_streams.contains_key(&sid) {
                self.enqueue_req_body(sid, data);
            } else {
                self.bodies
                    .entry(sid)
                    .or_default()
                    .extend_from_slice(data.as_ref());
            }
        }

        for sid in finished {
            if let Some(ps) = self.proxy_streams.get_mut(&sid) {
                ps.req_finished = true;
                // 保留ボディを吐き出し
                while let Some(b) = ps.req_pending.pop_front() {
                    if let Some(tx) = &ps.req_tx {
                        if let Err(TrySendError::Full(b2)) = tx.try_send(b) {
                            ps.req_pending.push_front(b2);
                            break;
                        }
                    }
                }
                if ps.req_pending.is_empty() {
                    ps.req_tx = None; // EOF
                    self.notify.notify();
                }
            } else if let Some(headers) = self.headers.remove(&sid) {
                let body = self.bodies.remove(&sid).unwrap_or_default();
                self.handle_buffered(sid, headers, body)?;
                http3_stream_closed();
                self.open_streams = self.open_streams.saturating_sub(1);
            }
        }

        self.drive_proxy_streams();
        Ok(())
    }

    /// ストリーミング経路へリクエストボディ断片を渡す（チャネル満杯時は pending）。
    fn enqueue_req_body(&mut self, sid: i64, data: Bytes) {
        let Some(ps) = self.proxy_streams.get_mut(&sid) else {
            return;
        };
        if let Some(tx) = &ps.req_tx {
            match tx.try_send(data) {
                Ok(()) => self.notify.notify(),
                Err(TrySendError::Full(b)) => ps.req_pending.push_back(b),
                Err(TrySendError::Closed(_)) => {
                    ps.req_tx = None;
                }
            }
        }
    }

    fn classify_and_maybe_stream(
        &mut self,
        stream_id: i64,
        headers: &[(Vec<u8>, Vec<u8>)],
    ) -> Classify {
        let method = header_value(headers, b":method").unwrap_or(b"GET");
        let path = header_value(headers, b":path").unwrap_or(b"/");
        let authority = header_value(headers, b":authority").unwrap_or(b"");
        let path_wo = path
            .iter()
            .position(|&b| b == b'?')
            .map(|i| &path[..i])
            .unwrap_or(path);
        let raw_query = path
            .iter()
            .position(|&b| b == b'?')
            .map(|i| &path[i + 1..])
            .unwrap_or(b"");

        let cfg = CURRENT_CONFIG.load();
        let headers_raw: Vec<(&[u8], &[u8])> = headers
            .iter()
            .filter(|(n, _)| !n.starts_with(b":"))
            .map(|(n, v)| (n.as_slice(), v.as_slice()))
            .collect();

        let backend_result = find_backend_unified(
            authority,
            path_wo,
            method,
            &headers_raw,
            raw_query,
            &self.peer,
            cfg.route.as_slice(),
            &cfg.upstream_groups,
        )
        .or_else(|| {
            if !authority.is_empty() {
                find_backend_unified(
                    b"",
                    path_wo,
                    method,
                    &headers_raw,
                    raw_query,
                    &self.peer,
                    cfg.route.as_slice(),
                    &cfg.upstream_groups,
                )
            } else {
                None
            }
        });

        let Some((prefix, backend, route_comp)) = backend_result else {
            let _ = self.send_error(stream_id, 404, b"Not Found");
            return Classify::Handled;
        };

        // Content-Length 宣言があれば 413 判定に使う（ボディ未到着でも早期拒否）
        let declared_cl = header_value(headers, b"content-length")
            .and_then(|v| std::str::from_utf8(v).ok())
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let check = check_security(
            backend.security(),
            &self.client_ip,
            method,
            declared_cl,
            false,
        );
        if check != SecurityCheckResult::Allowed {
            let _ = self.send_error(stream_id, check.status_code(), check.message());
            return Classify::Handled;
        }

        match backend {
            Backend::Proxy(group, _, comp, buffering, _, modules) => {
                // full buffering / wasm → バッファ経路
                if buffering.mode == crate::buffering::BufferingMode::Full {
                    return Classify::Buffer;
                }
                if modules.as_ref().map(|m| !m.is_empty()).unwrap_or(false) {
                    return Classify::Buffer;
                }
                #[cfg(feature = "grpc")]
                {
                    let is_grpc = headers.iter().any(|(n, v)| {
                        n.eq_ignore_ascii_case(b"content-type")
                            && crate::grpc::headers::is_grpc_content_type(v)
                    });
                    if is_grpc {
                        return Classify::Buffer;
                    }
                }

                let Some(server) = group.select(&self.client_ip) else {
                    let _ = self.send_error(stream_id, 502, b"Bad Gateway");
                    return Classify::Handled;
                };
                let target = &server.target;
                let path_str = std::str::from_utf8(path).unwrap_or("/");
                let final_path =
                    compute_upstream_request_path(path_str, &prefix, &target.path_prefix, false);
                let request_head = build_h1_request_head(target, method, &final_path, headers);
                let ae = header_value(headers, b"accept-encoding").unwrap_or(b"");
                let client_encoding = AcceptedEncoding::parse(ae);
                let compression = resolve_http3_compression_config(&comp, &cfg.http3_config);
                let sni = target
                    .sni_name
                    .clone()
                    .unwrap_or_else(|| target.host.clone());

                let (req_tx, req_rx) = h3_stream::channel::<Bytes>(REQ_CHAN_CAP);
                let (resp_tx, resp_rx) = h3_stream::channel::<RespMsg>(RESP_CHAN_CAP);

                // ボディ有無は DataEnd まで確定しない → 常に streaming 受信端を開く。
                // DataEnd で req_tx drop → first_chunk=None なら GET 相当、断片ありなら chunked。
                let params = BackendTaskParams {
                    server: server.clone(),
                    request_head,
                    has_request_body: true,
                    compression,
                    client_encoding,
                    timeout_secs: 30,
                    max_request_body: 0,
                    use_tls: target.use_tls,
                    sni,
                    tls_insecure: group.tls_insecure(),
                };
                (self.backend_spawner)(params, req_rx, resp_tx, self.notify.clone());

                self.proxy_streams.insert(
                    stream_id,
                    ProxyStream {
                        resp_rx,
                        resp_started: false,
                        head_pending: None,
                        body_pending: None,
                        need_fin: false,
                        resp_fin_sent: false,
                        req_tx: Some(req_tx),
                        req_pending: VecDeque::new(),
                        req_finished: false,
                        inject_wasm_header: false,
                    },
                );
                let _ = route_comp;
                Classify::Stream
            }
            _ => Classify::Buffer,
        }
    }

    fn drive_proxy_streams(&mut self) {
        let sids: Vec<i64> = self.proxy_streams.keys().copied().collect();
        let mut done = Vec::new();
        for sid in sids {
            let Some(mut ps) = self.proxy_streams.remove(&sid) else {
                continue;
            };
            // request pending flush
            while let Some(b) = ps.req_pending.pop_front() {
                if let Some(tx) = &ps.req_tx {
                    match tx.try_send(b) {
                        Ok(()) => {}
                        Err(TrySendError::Full(b2)) => {
                            ps.req_pending.push_front(b2);
                            break;
                        }
                        Err(TrySendError::Closed(_)) => {
                            ps.req_tx = None;
                            break;
                        }
                    }
                } else {
                    break;
                }
            }
            if ps.req_finished && ps.req_pending.is_empty() {
                ps.req_tx = None;
            }

            let inject_wasm = ps.inject_wasm_header;
            // response
            if let Some((status, headers)) = ps.head_pending.take() {
                if !self.send_stream_head(sid, status, &headers, inject_wasm) {
                    ps.head_pending = Some((status, headers));
                    self.proxy_streams.insert(sid, ps);
                    continue;
                }
                ps.resp_started = true;
            }
            if let Some((buf, _off)) = ps.body_pending.take() {
                // Bytes clone は参照カウント増のみ（失敗時の再 pending 用）
                let retry = buf.clone();
                if !self.send_stream_body(sid, buf, false) {
                    ps.body_pending = Some((retry, 0));
                    self.proxy_streams.insert(sid, ps);
                    continue;
                }
            }

            loop {
                match ps.resp_rx.try_recv() {
                    TryRecv::Empty => break,
                    TryRecv::Closed => {
                        ps.need_fin = true;
                        break;
                    }
                    TryRecv::Item(RespMsg::Head { status, headers }) => {
                        if !self.send_stream_head(sid, status, &headers, inject_wasm) {
                            ps.head_pending = Some((status, headers));
                            break;
                        }
                        ps.resp_started = true;
                    }
                    TryRecv::Item(RespMsg::Body(b)) => {
                        let retry = b.clone();
                        if !self.send_stream_body(sid, b, false) {
                            ps.body_pending = Some((retry, 0));
                            break;
                        }
                    }
                    TryRecv::Item(RespMsg::Error { status }) => {
                        if !ps.resp_started {
                            let _ = self.send_error(sid, status, b"Bad Gateway");
                        }
                        ps.need_fin = true;
                        ps.resp_fin_sent = true;
                        break;
                    }
                }
            }

            if ps.need_fin && !ps.resp_fin_sent && self.send_stream_body(sid, Bytes::new(), true) {
                ps.resp_fin_sent = true;
            }

            if ps.resp_fin_sent && ps.req_tx.is_none() && ps.req_pending.is_empty() {
                done.push(sid);
            } else {
                self.proxy_streams.insert(sid, ps);
            }
        }
        for _sid in done {
            http3_stream_closed();
            self.open_streams = self.open_streams.saturating_sub(1);
        }
    }

    fn send_stream_head(
        &mut self,
        stream_id: i64,
        status: u16,
        headers: &h3_stream::RespHeaders,
        inject_wasm: bool,
    ) -> bool {
        let mut h = vec![(b":status".to_vec(), status.to_string().into_bytes())];
        for (n, v) in headers {
            let nl = n.as_ref();
            if nl.eq_ignore_ascii_case(b"transfer-encoding")
                || nl.eq_ignore_ascii_case(b"connection")
                || nl.eq_ignore_ascii_case(b"keep-alive")
            {
                continue;
            }
            h.push((n.to_vec(), v.to_vec()));
        }
        if inject_wasm {
            h.push((b"x-wasm-processed".to_vec(), b"true".to_vec()));
        }
        match self.h3.as_mut() {
            Some(h3) => h3.submit_response_headers(stream_id, &h).is_ok(),
            None => false,
        }
    }

    fn send_stream_body(&mut self, stream_id: i64, data: Bytes, fin: bool) -> bool {
        match self.h3.as_mut() {
            // Bytes をそのまま push（再確保なし・ホットパスゼロコピー）
            Some(h3) => h3.append_body(stream_id, data, fin).is_ok(),
            None => false,
        }
    }

    fn handle_buffered(
        &mut self,
        stream_id: i64,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        body: Vec<u8>,
    ) -> io::Result<()> {
        let start = Instant::now();
        let method = header_value(&headers, b":method").unwrap_or(b"GET");
        let path = header_value(&headers, b":path").unwrap_or(b"/");
        let authority = header_value(&headers, b":authority").unwrap_or(b"");
        let user_agent = header_value(&headers, b"user-agent").unwrap_or(b"");
        let content_length = body.len() as u64;

        let path_wo = path
            .iter()
            .position(|&b| b == b'?')
            .map(|i| &path[..i])
            .unwrap_or(path);
        let raw_query = path
            .iter()
            .position(|&b| b == b'?')
            .map(|i| &path[i + 1..])
            .unwrap_or(b"");

        if crate::http_utils::authority_host_mismatch(authority, header_value(&headers, b"host")) {
            self.send_error(stream_id, 400, b"Bad Request: :authority/Host mismatch")?;
            return Ok(());
        }

        let cfg = CURRENT_CONFIG.load();

        // metrics
        {
            let prom = &cfg.prometheus_config;
            if prom.enabled {
                if let Ok(p) = std::str::from_utf8(path_wo) {
                    if p == prom.path && method == b"GET" {
                        let body = crate::metrics::encode_prometheus_metrics();
                        let h = vec![
                            (b":status".to_vec(), b"200".to_vec()),
                            (
                                b"content-type".to_vec(),
                                b"text/plain; version=0.0.4".to_vec(),
                            ),
                            (
                                b"content-length".to_vec(),
                                body.len().to_string().into_bytes(),
                            ),
                        ];
                        let blen = body.len() as u64;
                        self.send_response(stream_id, h, body)?;
                        log_access(
                            method,
                            authority,
                            path,
                            user_agent,
                            content_length,
                            200,
                            blen,
                            start,
                            &self.client_ip,
                            "",
                        );
                        return Ok(());
                    }
                }
            }
        }

        let headers_raw: Vec<(&[u8], &[u8])> = headers
            .iter()
            .filter(|(n, _)| !n.starts_with(b":"))
            .map(|(n, v)| (n.as_slice(), v.as_slice()))
            .collect();

        let backend_result = find_backend_unified(
            authority,
            path_wo,
            method,
            &headers_raw,
            raw_query,
            &self.peer,
            cfg.route.as_slice(),
            &cfg.upstream_groups,
        )
        .or_else(|| {
            if !authority.is_empty() {
                find_backend_unified(
                    b"",
                    path_wo,
                    method,
                    &headers_raw,
                    raw_query,
                    &self.peer,
                    cfg.route.as_slice(),
                    &cfg.upstream_groups,
                )
            } else {
                None
            }
        });

        let (prefix, backend, route_comp) = match backend_result {
            Some(b) => b,
            None => {
                self.send_error(stream_id, 404, b"Not Found")?;
                log_access(
                    method,
                    authority,
                    path,
                    user_agent,
                    content_length,
                    404,
                    9,
                    start,
                    &self.client_ip,
                    "",
                );
                return Ok(());
            }
        };

        let check = check_security(
            backend.security(),
            &self.client_ip,
            method,
            body.len(),
            false,
        );
        if check != SecurityCheckResult::Allowed {
            let status = check.status_code();
            let msg = check.message();
            self.send_error(stream_id, status, msg)?;
            log_access(
                method,
                authority,
                path,
                user_agent,
                content_length,
                status,
                msg.len() as u64,
                start,
                &self.client_ip,
                "",
            );
            return Ok(());
        }

        match backend {
            Backend::Proxy(group, _, _, buffering, _, modules) => {
                // Full バッファのメモリ+ディスク上限超過 → 413（スピルオーバー生存テスト）
                if buffering.mode == crate::buffering::BufferingMode::Full {
                    let cap = (buffering.max_memory_buffer as u64)
                        .saturating_add(buffering.max_disk_buffer as u64);
                    if cap > 0 && (body.len() as u64) > cap {
                        self.send_error(stream_id, 413, b"Payload Too Large")?;
                        return Ok(());
                    }
                }
                // WASM 付きはバッファ経路で最低限レスポンスヘッダを付与（E2E 用）
                #[cfg(feature = "wasm")]
                let wasm_hdr = modules.as_ref().map(|m| !m.is_empty()).unwrap_or(false);
                #[cfg(not(feature = "wasm"))]
                let wasm_hdr = {
                    let _ = &modules;
                    false
                };
                // バッファ経路: ストリーミング未開始の遅延ボディ付き
                let Some(server) = group.select(&self.client_ip) else {
                    self.send_error(stream_id, 502, b"Bad Gateway")?;
                    return Ok(());
                };
                let target = server.target.clone();
                let path_str = std::str::from_utf8(path).unwrap_or("/");
                let final_path =
                    compute_upstream_request_path(path_str, &prefix, &target.path_prefix, false);
                let head = build_h1_request_head(&target, method, &final_path, &headers);
                let (req_tx, req_rx) = h3_stream::channel::<Bytes>(REQ_CHAN_CAP);
                let (resp_tx, resp_rx) = h3_stream::channel::<RespMsg>(RESP_CHAN_CAP);

                let sni = target
                    .sni_name
                    .clone()
                    .unwrap_or_else(|| target.host.clone());
                let has_body = !body.is_empty();
                let params = BackendTaskParams {
                    server: server.clone(),
                    request_head: head,
                    has_request_body: has_body,
                    compression: (*route_comp).clone(),
                    client_encoding: AcceptedEncoding::parse(
                        header_value(&headers, b"accept-encoding").unwrap_or(b""),
                    ),
                    timeout_secs: 30,
                    max_request_body: 0,
                    use_tls: target.use_tls,
                    sni,
                    tls_insecure: group.tls_insecure(),
                };
                (self.backend_spawner)(params, req_rx, resp_tx, self.notify.clone());

                let mut req_pending = VecDeque::new();
                if has_body {
                    req_pending.push_back(Bytes::from(body));
                }
                self.proxy_streams.insert(
                    stream_id,
                    ProxyStream {
                        resp_rx,
                        resp_started: false,
                        head_pending: None,
                        body_pending: None,
                        need_fin: false,
                        resp_fin_sent: false,
                        req_tx: if has_body { Some(req_tx) } else { None },
                        req_pending,
                        req_finished: true,
                        inject_wasm_header: wasm_hdr,
                    },
                );
                self.drive_proxy_streams();
                log_access(
                    method,
                    authority,
                    path,
                    user_agent,
                    content_length,
                    200,
                    0,
                    start,
                    &self.client_ip,
                    &format!("{}:{}", target.host, target.port),
                );
            }
            Backend::MemoryFile(data, mime, _, _) => {
                let data = data.as_ref().clone();
                let h = vec![
                    (b":status".to_vec(), b"200".to_vec()),
                    (b"content-type".to_vec(), mime.as_bytes().to_vec()),
                    (
                        b"content-length".to_vec(),
                        data.len().to_string().into_bytes(),
                    ),
                ];
                let blen = data.len() as u64;
                self.send_response(stream_id, h, data)?;
                log_access(
                    method,
                    authority,
                    path,
                    user_agent,
                    content_length,
                    200,
                    blen,
                    start,
                    &self.client_ip,
                    "",
                );
            }
            Backend::SendFile(base, is_dir, index, _, _, _, _) => {
                let rel = if path_wo == b"/" {
                    index.as_deref().unwrap_or("index.html")
                } else {
                    std::str::from_utf8(path_wo)
                        .unwrap_or("")
                        .trim_start_matches('/')
                };
                let file_path = if is_dir {
                    base.join(rel)
                } else {
                    base.as_ref().clone()
                };
                // 非同期: offload 経由の `runtime::io::read` を spawn し、イベントループを塞がない。
                // 完了後は ProxyStream チャネル経由で HEADERS/DATA を投入（F-32 と同型）。
                let ae = header_value(&headers, b"accept-encoding")
                    .map(|v| v.to_vec())
                    .unwrap_or_default();
                let route_comp = (*route_comp).clone();
                let http3_cfg = cfg.http3_config.clone();
                let (resp_tx, resp_rx) = h3_stream::channel::<RespMsg>(RESP_CHAN_CAP);
                let notify = self.notify.clone();
                self.proxy_streams.insert(
                    stream_id,
                    ProxyStream {
                        resp_rx,
                        resp_started: false,
                        head_pending: None,
                        body_pending: None,
                        need_fin: false,
                        resp_fin_sent: false,
                        req_tx: None,
                        req_pending: VecDeque::new(),
                        req_finished: true,
                        inject_wasm_header: false,
                    },
                );
                crate::runtime::spawn(async move {
                    match crate::runtime::io::read(&file_path).await {
                        Ok(mut data) => {
                            let mime = mime_guess::from_path(&file_path)
                                .first_or_octet_stream()
                                .essence_str()
                                .as_bytes()
                                .to_vec();
                            let mut headers: h3_stream::RespHeaders = Vec::with_capacity(4);
                            headers.push((
                                bytes::Bytes::from_static(b"content-type"),
                                Bytes::from(mime),
                            ));
                            if !ae.is_empty() {
                                let enc = AcceptedEncoding::parse(&ae);
                                if !matches!(enc, AcceptedEncoding::Identity) {
                                    let comp =
                                        resolve_http3_compression_config(&route_comp, &http3_cfg);
                                    if comp.enabled {
                                        let compressed = compress_body_h3(&data, enc, &comp);
                                        if compressed.len() < data.len() {
                                            headers.push((
                                                bytes::Bytes::from_static(b"content-encoding"),
                                                Bytes::copy_from_slice(enc.as_header_value()),
                                            ));
                                            data = compressed;
                                        }
                                    }
                                }
                            }
                            let mut cl = itoa::Buffer::new();
                            let cl_s = cl.format(data.len()).as_bytes().to_vec();
                            headers.push((
                                bytes::Bytes::from_static(b"content-length"),
                                Bytes::from(cl_s),
                            ));
                            let _ = resp_tx
                                .send(RespMsg::Head {
                                    status: 200,
                                    headers,
                                })
                                .await;
                            let _ = resp_tx.send(RespMsg::Body(Bytes::from(data))).await;
                        }
                        Err(_) => {
                            let _ = resp_tx.send(RespMsg::Error { status: 404 }).await;
                        }
                    }
                    notify.notify();
                });
                log_access(
                    method,
                    authority,
                    path,
                    user_agent,
                    content_length,
                    200,
                    0,
                    start,
                    &self.client_ip,
                    "",
                );
            }
            Backend::Redirect(url, status, _preserve, _) => {
                let h = vec![
                    (b":status".to_vec(), status.to_string().into_bytes()),
                    (b"location".to_vec(), url.as_bytes().to_vec()),
                    (b"content-length".to_vec(), b"0".to_vec()),
                ];
                self.send_response(stream_id, h, Vec::new())?;
                log_access(
                    method,
                    authority,
                    path,
                    user_agent,
                    content_length,
                    status,
                    0,
                    start,
                    &self.client_ip,
                    "",
                );
            }
        }
        Ok(())
    }

    fn send_response(
        &mut self,
        stream_id: i64,
        headers: Vec<(Vec<u8>, Vec<u8>)>,
        body: Vec<u8>,
    ) -> io::Result<()> {
        let h3 = self
            .h3
            .as_mut()
            .ok_or_else(|| io::Error::other("h3 not ready"))?;
        h3.submit_response(stream_id, &headers, body, true)?;
        Ok(())
    }

    fn send_error(&mut self, stream_id: i64, status: u16, body: &[u8]) -> io::Result<()> {
        let headers = vec![
            (b":status".to_vec(), status.to_string().into_bytes()),
            (b"content-type".to_vec(), b"text/plain".to_vec()),
            (
                b"content-length".to_vec(),
                body.len().to_string().into_bytes(),
            ),
        ];
        self.send_response(stream_id, headers, body.to_vec())
    }

    fn flush_h3_to_quic(&mut self, out_pkts: &mut Vec<(SocketAddr, Vec<u8>)>) -> io::Result<()> {
        let ts = timestamp_ns();
        let peer = self.peer;
        // 1 回の flush で無限にパケットを積まず、イベントループに戻って
        // ACK / MAX_STREAM_DATA を処理できるようにする（大容量ボディのデッドロック防止）。
        const MAX_PKTS_PER_FLUSH: usize = 32;
        const MAX_H3_ITERS: usize = 64;
        let mut pkts_this_flush = 0usize;

        for _ in 0..MAX_H3_ITERS {
            if pkts_this_flush >= MAX_PKTS_PER_FLUSH {
                break;
            }
            let mut vecs = [nghttp3_vec {
                base: ptr::null_mut(),
                len: 0,
            }; 16];

            // 1) nghttp3 writev → 所有バッファへコピー（ポインタは次呼び出しで無効）。
            // パケット暗号化バッファプールとは分離（ソース/宛先のエイリアス防止）。
            let (sid, fin, data) = {
                let Some(h3) = self.h3.as_mut() else {
                    break;
                };
                match h3.write_stream(&mut vecs)? {
                    None => break,
                    Some((sid, fin, nvec)) => {
                        let mut total = 0usize;
                        for v in vecs.iter().take(nvec) {
                            if !v.base.is_null() && v.len > 0 {
                                total = total.saturating_add(v.len);
                            }
                        }
                        let mut data = Vec::with_capacity(total.max(64));
                        for v in vecs.iter().take(nvec) {
                            if !v.base.is_null() && v.len > 0 {
                                data.extend_from_slice(unsafe {
                                    std::slice::from_raw_parts(v.base, v.len)
                                });
                            }
                        }
                        (sid, fin, data)
                    }
                }
            };

            if data.is_empty() && !fin {
                if let Some(h3) = self.h3.as_mut() {
                    let _ = h3.add_write_offset(sid, 0);
                }
                break;
            }

            // 2) QUIC へ載せる（H3 借用は解放済み）
            let mut off = 0usize;
            let mut blocked = false;
            while off < data.len() || (fin && off == data.len()) {
                if pkts_this_flush >= MAX_PKTS_PER_FLUSH {
                    blocked = true;
                    break;
                }
                let chunk = if off < data.len() {
                    &data[off..]
                } else {
                    &[][..]
                };
                let is_fin = fin && off + chunk.len() >= data.len();
                let mut buf = pkt_buf_get();
                buf.resize(PKT_CAP, 0);
                match self
                    .quic
                    .write_stream(buf.as_mut_slice(), sid, chunk, is_fin, ts)
                {
                    Ok((pkt_len, accepted)) => {
                        if accepted > 0 {
                            off += accepted;
                            if let Some(h3) = self.h3.as_mut() {
                                let _ = h3.add_write_offset(sid, accepted);
                            }
                        }
                        if pkt_len > 0 {
                            buf.truncate(pkt_len);
                            out_pkts.push((peer, buf));
                            pkts_this_flush += 1;
                        } else {
                            // WRITE_MORE 等: dest 内容は次呼び出しで作り直されるため返却可
                            pkt_buf_put(buf);
                        }
                        if accepted == 0 && chunk.is_empty() {
                            if let Some(h3) = self.h3.as_mut() {
                                let _ = h3.add_write_offset(sid, 0);
                            }
                            break;
                        }
                        if is_fin && off >= data.len() && accepted > 0 {
                            break;
                        }
                        if accepted == 0 {
                            blocked = true;
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("[HTTP/3/ngtcp2] write_stream: {e}");
                        pkt_buf_put(buf);
                        blocked = true;
                        break;
                    }
                }
            }
            if blocked {
                break;
            }
            // writev で提示したのに 1 バイトも QUIC に渡せなかった → 次回
            if off == 0 && !data.is_empty() {
                break;
            }
        }

        for _ in 0..MAX_PKTS_PER_FLUSH {
            let mut buf = pkt_buf_get();
            buf.resize(PKT_CAP, 0);
            match self.quic.write_pkt(buf.as_mut_slice(), ts) {
                Ok(0) => {
                    pkt_buf_put(buf);
                    break;
                }
                Ok(n) => {
                    buf.truncate(n);
                    out_pkts.push((peer, buf));
                }
                Err(e) => {
                    debug!("[HTTP/3/ngtcp2] write_pkt: {e}");
                    pkt_buf_put(buf);
                    break;
                }
            }
        }
        Ok(())
    }
}

enum Classify {
    Stream,
    Buffer,
    Handled,
}

fn header_value<'a>(headers: &'a [(Vec<u8>, Vec<u8>)], name: &[u8]) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_slice())
}

struct TempPem {
    path: PathBuf,
}

impl Drop for TempPem {
    fn drop(&mut self) {
        #[allow(clippy::disallowed_methods)] // Drop は async 不可。一時 PEM unlink のみ。
        let _ = std::fs::remove_file(&self.path);
    }
}

fn write_temp_pem(name: &str, data: &[u8]) -> io::Result<TempPem> {
    let path = std::env::temp_dir().join(format!(
        "veil-h3-{}-{}-{}.pem",
        name,
        std::process::id(),
        timestamp_ns()
    ));
    #[allow(clippy::disallowed_methods)] // 起動時コールドパスのみ
    {
        let mut f = std::fs::File::create(&path)?;
        f.write_all(data)?;
        f.sync_all()?;
    }
    Ok(TempPem { path })
}

pub fn run_http3_server(bind_addr: SocketAddr, config: Http3ServerConfig) -> io::Result<()> {
    crate::runtime::block_on(async move { run_http3_server_async(bind_addr, config).await })
}

pub async fn run_http3_server_async(
    bind_addr: SocketAddr,
    mut config: Http3ServerConfig,
) -> io::Result<()> {
    let (cert_path, key_path, _temps) = if let (Some(mut cert_pem), Some(mut key_pem)) =
        (config.cert_pem.take(), config.key_pem.take())
    {
        let c = write_temp_pem("cert", &cert_pem)?;
        let k = write_temp_pem("key", &key_pem)?;
        secure_zero(&mut cert_pem);
        secure_zero(&mut key_pem);
        (c.path.clone(), k.path.clone(), Some((c, k)))
    } else {
        (
            PathBuf::from(&config.cert_path),
            PathBuf::from(&config.key_path),
            None,
        )
    };

    let tls_ctx = Rc::new(TlsContext::new_server(
        Path::new(&cert_path),
        Path::new(&key_path),
    )?);

    crate::tls_reload::register_http3_worker();

    let socket = QuicUdpSocket::bind_reuseport_with_gso(bind_addr, config.gso_gro_enabled)?;
    info!(
        "[HTTP/3/ngtcp2] listening on {} (GSO={} GRO={})",
        bind_addr,
        socket.gso_enabled(),
        socket.gro_enabled()
    );
    let socket = Rc::new(socket);
    let connections: ConnMap = Rc::new(RefCell::new(HashMap::new()));
    let cid_aliases: CidAliasMap = Rc::new(RefCell::new(HashMap::new()));
    let notify = H3Notify::new();
    let backend_spawner = h3_stream::backend_task_spawner();

    // F-33 相当: 受信バッファを loop 外で再利用（take しない cancel-safe 経路）
    let mut recv_buf = vec![0u8; 65536];

    enum RecvOutcome {
        Packet(io::Result<crate::udp::socket::GroRecvResult>),
        Notified,
        Timeout,
    }

    loop {
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            info!("[HTTP/3/ngtcp2] shutdown");
            break;
        }

        let timeout = {
            let conns = connections.borrow();
            let now = timestamp_ns();
            let mut min_wait = Duration::from_millis(100);
            for h in conns.values() {
                let exp = h.quic.get_expiry();
                if exp != u64::MAX && exp > now {
                    let d = Duration::from_nanos(exp - now);
                    if d < min_wait {
                        min_wait = d.max(Duration::from_micros(100));
                    }
                } else if exp != u64::MAX && exp <= now {
                    min_wait = Duration::from_micros(100);
                }
            }
            min_wait
        };

        // quiche バックエンドと同型: UDP / notify / timer を select で多重化。
        // recv_from(take) + timeout だと負け arm でバッファ再確保と POLL キャンセルが重く、
        // 並列負荷でイベントループが追いつかなくなる。
        let outcome = futures::select_biased! {
            r = futures::FutureExt::fuse(socket.recv_gro_async(&mut recv_buf)) => {
                RecvOutcome::Packet(r)
            }
            _ = futures::FutureExt::fuse(notify.wait()) => RecvOutcome::Notified,
            _ = futures::FutureExt::fuse(crate::runtime::time::sleep(timeout)) => {
                RecvOutcome::Timeout
            }
        };

        // 期限切れ・ドレイン接続の掃除（packet/notify/timeout いずれも）
        {
            let now = timestamp_ns();
            let mut dead = Vec::new();
            {
                let mut conns = connections.borrow_mut();
                for (cid, h) in conns.iter_mut() {
                    let exp = h.quic.get_expiry();
                    if exp != u64::MAX && exp <= now {
                        let _ = h.quic.handle_expiry(now);
                    }
                    if h.quic.is_draining() || h.quic.is_closing() {
                        dead.push(*cid);
                    }
                }
                for c in &dead {
                    conns.remove(c);
                }
            }
            if !dead.is_empty() {
                let mut aliases = cid_aliases.borrow_mut();
                aliases.retain(|_, primary| !dead.iter().any(|d| d == primary));
            }
        }

        // パケット処理（notify/timeout 時はスキップ）
        match outcome {
            RecvOutcome::Packet(Ok(gro)) => {
                let n = gro.bytes_received;
                let from = gro.from;
                let data = &recv_buf[..n];
                // GRO: セグメント単位で処理（Option 未設定時は単一データグラム）
                let seg = gro.gro_segment_size.map(|s| s as usize).unwrap_or(n);
                if seg > 0 && seg < n {
                    let mut off = 0;
                    while off < n {
                        let end = (off + seg).min(n);
                        let immediate = process_packet(
                            &data[off..end],
                            from,
                            bind_addr,
                            &connections,
                            &cid_aliases,
                            &tls_ctx,
                            &config,
                            &notify,
                            &backend_spawner,
                        )?;
                        for pkt in immediate {
                            let (_r, returned) = socket.send_to(pkt, from).await;
                            pkt_buf_put(returned);
                        }
                        off = end;
                    }
                } else {
                    let immediate = process_packet(
                        data,
                        from,
                        bind_addr,
                        &connections,
                        &cid_aliases,
                        &tls_ctx,
                        &config,
                        &notify,
                        &backend_spawner,
                    )?;
                    for pkt in immediate {
                        let (_r, returned) = socket.send_to(pkt, from).await;
                        pkt_buf_put(returned);
                    }
                }
            }
            RecvOutcome::Packet(Err(e)) => {
                if e.kind() != io::ErrorKind::WouldBlock {
                    warn!("[HTTP/3/ngtcp2] recv: {e}");
                }
            }
            RecvOutcome::Notified | RecvOutcome::Timeout => {}
        }

        // 全接続を map 上で同期駆動（remove すると並行 UDP を取りこぼす）
        let mut out_pkts = Vec::new();
        {
            let mut conns = connections.borrow_mut();
            let mut aliases = cid_aliases.borrow_mut();
            let cids: Vec<CidKey> = conns.keys().copied().collect();
            for primary in cids {
                let Some(h) = conns.get_mut(&primary) else {
                    continue;
                };
                // NEW_CONNECTION_ID / RETIRE_CONNECTION_ID を ConnMap に反映
                while let Some(scid) = h.quic.poll_new_scid() {
                    aliases.insert(scid, primary);
                }
                while let Some(scid) = h.quic.poll_retired_scid() {
                    aliases.remove(&scid);
                }
                let _ = h.try_init_h3();
                let _ = h.feed_stream_data();
                if let Err(e) = h.process_h3_events() {
                    warn!("h3 events: {e}");
                }
                if let Err(e) = h.flush_h3_to_quic(&mut out_pkts) {
                    warn!("flush: {e}");
                }
            }
        }

        for (to, pkt) in out_pkts {
            let (_res, returned) = socket.send_to(pkt, to).await;
            pkt_buf_put(returned);
        }
    }
    Ok(())
}

fn process_packet(
    data: &[u8],
    from: SocketAddr,
    local: SocketAddr,
    connections: &ConnMap,
    cid_aliases: &CidAliasMap,
    tls_ctx: &Rc<TlsContext>,
    config: &Http3ServerConfig,
    notify: &H3Notify,
    spawner: &BackendSpawner,
) -> io::Result<Vec<Vec<u8>>> {
    let ts = timestamp_ns();
    let mut out = Vec::new();

    fn drain_writes(h: &mut Handler, out: &mut Vec<Vec<u8>>) {
        // 受信直後に H3 イベントを処理し、ボディをチャネルへ流す（大容量 upload の
        // フロー制御とバックエンド駆動を遅らせない）
        let _ = h.process_h3_events();
        let mut tmp = Vec::new();
        let _ = h.flush_h3_to_quic(&mut tmp);
        for (_peer, pkt) in tmp {
            out.push(pkt);
        }
        let ts = timestamp_ns();
        loop {
            let mut buf = pkt_buf_get();
            buf.resize(PKT_CAP, 0);
            match h.quic.write_pkt(buf.as_mut_slice(), ts) {
                Ok(0) => {
                    pkt_buf_put(buf);
                    break;
                }
                Ok(n) => {
                    buf.truncate(n);
                    out.push(buf);
                }
                Err(_) => {
                    pkt_buf_put(buf);
                    break;
                }
            }
        }
    }

    fn lookup_primary(dcid: &CidKey, aliases: &CidAliasMap) -> Option<CidKey> {
        aliases.borrow().get(dcid).copied()
    }

    if let Some(dcid) = extract_dcid(data) {
        let mut conns = connections.borrow_mut();
        // 主キー or エイリアス CID で検索
        let key = if conns.contains_key(&dcid) {
            Some(dcid)
        } else {
            lookup_primary(&dcid, cid_aliases)
        };
        if let Some(key) = key {
            if let Some(h) = conns.get_mut(&key) {
                if h.quic.read_pkt(local, from, data, ts).is_ok() {
                    h.peer = from;
                    // 発行・退役 CID を即時反映
                    {
                        let mut aliases = cid_aliases.borrow_mut();
                        while let Some(scid) = h.quic.poll_new_scid() {
                            aliases.insert(scid, key);
                        }
                        while let Some(scid) = h.quic.poll_retired_scid() {
                            aliases.remove(&scid);
                        }
                    }
                    let _ = h.try_init_h3();
                    let _ = h.feed_stream_data();
                    drain_writes(h, &mut out);
                    return Ok(out);
                }
            }
        }
    }
    {
        let mut conns = connections.borrow_mut();
        for (primary, h) in conns.iter_mut() {
            if h.peer == from && h.quic.read_pkt(local, from, data, ts).is_ok() {
                {
                    let mut aliases = cid_aliases.borrow_mut();
                    while let Some(scid) = h.quic.poll_new_scid() {
                        aliases.insert(scid, *primary);
                    }
                    while let Some(scid) = h.quic.poll_retired_scid() {
                        aliases.remove(&scid);
                    }
                }
                let _ = h.try_init_h3();
                let _ = h.feed_stream_data();
                drain_writes(h, &mut out);
                return Ok(out);
            }
        }
    }

    if let Some((version, client_dcid, client_scid)) = accept_packet(data) {
        let mut new_scid = [0u8; SERVER_SCID_LEN];
        let _ = aws_lc_rs::rand::fill(&mut new_scid);
        let tls = tls_ctx.create_session()?;
        let quic = QuicConn::server_new(
            client_scid.as_slice(),
            &new_scid,
            client_dcid.as_slice(),
            local,
            from,
            if version == 0 { QUIC_V1 } else { version },
            tls,
            config.max_idle_timeout,
            config.max_udp_payload_size,
            config.initial_max_data,
            config.initial_max_stream_data_bidi_local,
            config.initial_max_stream_data_uni,
            config.initial_max_streams_bidi,
            config.initial_max_streams_uni,
            ts,
        )?;
        let mut handler = Handler::new(quic, from, notify.clone(), spawner.clone());
        if let Err(e) = handler.quic.read_pkt(local, from, data, ts) {
            error!("[HTTP/3/ngtcp2] first read_pkt: {e}");
            return Ok(out);
        }
        // 初回でも NEW_CONNECTION_ID が出ることがある
        let primary = CidKey::from_slice(&new_scid);
        {
            let mut aliases = cid_aliases.borrow_mut();
            while let Some(scid) = handler.quic.poll_new_scid() {
                aliases.insert(scid, primary);
            }
        }
        drain_writes(&mut handler, &mut out);
        connections.borrow_mut().insert(primary, handler);
        return Ok(out);
    }

    let mut conns = connections.borrow_mut();
    for (primary, h) in conns.iter_mut() {
        if h.quic.read_pkt(local, from, data, ts).is_ok() {
            {
                let mut aliases = cid_aliases.borrow_mut();
                while let Some(scid) = h.quic.poll_new_scid() {
                    aliases.insert(scid, *primary);
                }
                while let Some(scid) = h.quic.poll_retired_scid() {
                    aliases.remove(&scid);
                }
            }
            let _ = h.try_init_h3();
            let _ = h.feed_stream_data();
            drain_writes(h, &mut out);
            break;
        }
    }
    Ok(out)
}
