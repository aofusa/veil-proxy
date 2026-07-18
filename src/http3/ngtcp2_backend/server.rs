//! ngtcp2 + nghttp3 HTTP/3 サーバ本体

use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Write as _};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::ptr;
use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ftlog::{debug, error, info, warn};
use nghttp3_sys::nghttp3_vec;

use crate::config::{Backend, CURRENT_CONFIG, SHUTDOWN_FLAG};
use crate::http3::common::{secure_zero, Http3ServerConfig};
use crate::logging::log_access;
use crate::metrics::{http3_stream_closed, http3_stream_opened, Http3ActiveConnGuard};
use crate::pool::MAX_HEADER_SIZE;
use crate::proxy::{check_security, SecurityCheckResult};
use crate::udp::QuicUdpSocket;
use crate::upstream::find_backend_unified;

use super::conn::{accept_packet, timestamp_ns, QuicConn, QUIC_V1};
use super::crypto::TlsContext;
use super::h3::{H3Conn, H3Event};

type ConnMap = Rc<RefCell<HashMap<Vec<u8>, Handler>>>;

struct Handler {
    quic: QuicConn,
    h3: Option<H3Conn>,
    peer: SocketAddr,
    client_ip: String,
    bodies: HashMap<i64, Vec<u8>>,
    headers: HashMap<i64, Vec<(Vec<u8>, Vec<u8>)>>,
    _metric: Http3ActiveConnGuard,
}

impl Handler {
    fn new(quic: QuicConn, peer: SocketAddr) -> Self {
        Self {
            quic,
            h3: None,
            client_ip: peer.ip().to_string(),
            peer,
            bodies: HashMap::new(),
            headers: HashMap::new(),
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
                let n = h3.read_stream(sd.stream_id, &sd.data, sd.fin, ts)?;
                self.quic.extend_max_stream_offset(sd.stream_id, n as u64);
            }
        }
        Ok(())
    }

    async fn process_h3_events(&mut self) -> io::Result<()> {
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
                        pending_data.push((stream_id, data));
                    }
                    H3Event::DataEnd { stream_id } => finished.push(stream_id),
                    H3Event::Reset { stream_id } => {
                        self.bodies.remove(&stream_id);
                        self.headers.remove(&stream_id);
                    }
                }
            }
        }
        for (sid, data) in pending_data {
            self.bodies.entry(sid).or_default().extend_from_slice(&data);
        }
        for (sid, headers) in pending_headers {
            let size: usize = headers
                .iter()
                .map(|(n, v)| n.len().saturating_add(v.len()))
                .sum();
            if size > MAX_HEADER_SIZE {
                self.send_error(sid, 431, b"Request Header Fields Too Large")
                    .await?;
                continue;
            }
            http3_stream_opened();
            self.headers.insert(sid, headers);
        }
        for sid in finished {
            if let Some(headers) = self.headers.remove(&sid) {
                let body = self.bodies.remove(&sid).unwrap_or_default();
                self.handle_request(sid, headers, body).await?;
                http3_stream_closed();
            }
        }
        Ok(())
    }

    async fn handle_request(
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

        let path_wo_query = path
            .iter()
            .position(|&b| b == b'?')
            .map(|i| &path[..i])
            .unwrap_or(path);
        let raw_query = path
            .iter()
            .position(|&b| b == b'?')
            .map(|i| &path[i + 1..])
            .unwrap_or(b"");

        // :authority と Host の矛盾を拒否
        let host_hdr = header_value(&headers, b"host");
        if crate::http_utils::authority_host_mismatch(authority, host_hdr) {
            self.send_error(stream_id, 400, b"Bad Request: :authority/Host mismatch")
                .await?;
            log_access(
                method,
                authority,
                path,
                user_agent,
                content_length,
                400,
                0,
                start,
                &self.client_ip,
                "",
            );
            return Ok(());
        }

        debug!(
            "[HTTP/3/ngtcp2] {} {} from {}",
            String::from_utf8_lossy(method),
            String::from_utf8_lossy(path),
            self.client_ip
        );

        let cfg = CURRENT_CONFIG.load();

        // メトリクス
        {
            let prom = &cfg.prometheus_config;
            if prom.enabled {
                if let Ok(p) = std::str::from_utf8(path_wo_query) {
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
                        self.send_response(stream_id, h, body).await?;
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
            path_wo_query,
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
                    path_wo_query,
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

        let (_prefix, backend, _route_comp) = match backend_result {
            Some(b) => b,
            None => {
                self.send_error(stream_id, 404, b"Not Found").await?;
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
            self.send_error(stream_id, status, msg).await?;
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
            Backend::Proxy(group, _, _, _, _, _) => {
                let Some(server) = group.select(&self.client_ip) else {
                    self.send_error(stream_id, 502, b"Bad Gateway").await?;
                    return Ok(());
                };
                let target = server.target.clone();
                let status = self
                    .proxy_buffered(stream_id, &target, method, path, &headers, &body)
                    .await
                    .unwrap_or(502);
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
                self.send_response(stream_id, h, data).await?;
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
                let rel = if path_wo_query == b"/" {
                    index.as_deref().unwrap_or("index.html")
                } else {
                    std::str::from_utf8(path_wo_query)
                        .unwrap_or("")
                        .trim_start_matches('/')
                };
                let file_path = if is_dir {
                    base.join(rel)
                } else {
                    base.as_ref().clone()
                };
                match read_file_offload(&file_path).await {
                    Ok(data) => {
                        let h = vec![
                            (b":status".to_vec(), b"200".to_vec()),
                            (
                                b"content-type".to_vec(),
                                mime_guess::from_path(&file_path)
                                    .first_or_octet_stream()
                                    .essence_str()
                                    .as_bytes()
                                    .to_vec(),
                            ),
                            (
                                b"content-length".to_vec(),
                                data.len().to_string().into_bytes(),
                            ),
                        ];
                        let blen = data.len() as u64;
                        self.send_response(stream_id, h, data).await?;
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
                    Err(_) => {
                        self.send_error(stream_id, 404, b"Not Found").await?;
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
                    }
                }
            }
            Backend::Redirect(url, status, _preserve, _) => {
                let h = vec![
                    (b":status".to_vec(), status.to_string().into_bytes()),
                    (b"location".to_vec(), url.as_bytes().to_vec()),
                    (b"content-length".to_vec(), b"0".to_vec()),
                ];
                self.send_response(stream_id, h, Vec::new()).await?;
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

    async fn proxy_buffered(
        &mut self,
        stream_id: i64,
        target: &crate::config::ProxyTarget,
        method: &[u8],
        path: &[u8],
        headers: &[(Vec<u8>, Vec<u8>)],
        body: &[u8],
    ) -> io::Result<u16> {
        use crate::runtime::tcp::TcpStream;
        use std::os::fd::AsRawFd;

        let addr = format!("{}:{}", target.host, target.port);
        let backend = match crate::runtime::time::timeout(
            Duration::from_secs(30),
            TcpStream::connect_str(&addr),
        )
        .await
        {
            Ok(Ok(s)) => s,
            _ => {
                self.send_error(stream_id, 502, b"Bad Gateway").await?;
                return Ok(502);
            }
        };

        let mut req = format!(
            "{} {} HTTP/1.1\r\nHost: {}:{}\r\nConnection: close\r\n",
            String::from_utf8_lossy(method),
            String::from_utf8_lossy(path),
            target.host,
            target.port
        );
        if !body.is_empty() {
            req.push_str(&format!("Content-Length: {}\r\n", body.len()));
        }
        for (n, v) in headers {
            if n.starts_with(b":") {
                continue;
            }
            let nl = String::from_utf8_lossy(n);
            if nl.eq_ignore_ascii_case("host") || nl.eq_ignore_ascii_case("connection") {
                continue;
            }
            req.push_str(&format!("{}: {}\r\n", nl, String::from_utf8_lossy(v)));
        }
        req.push_str("\r\n");
        let mut req_bytes = req.into_bytes();
        req_bytes.extend_from_slice(body);

        // write
        let fd = backend.as_raw_fd();
        let mut off = 0;
        while off < req_bytes.len() {
            let n = unsafe {
                libc::write(
                    fd,
                    req_bytes[off..].as_ptr() as *const _,
                    req_bytes.len() - off,
                )
            };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    backend.writable().await?;
                    continue;
                }
                self.send_error(stream_id, 502, b"Bad Gateway").await?;
                return Ok(502);
            }
            off += n as usize;
        }

        // read response
        let mut resp = Vec::new();
        let mut buf = vec![0u8; 16 * 1024];
        loop {
            let n = unsafe { libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len()) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    if backend.readable().await.is_err() {
                        break;
                    }
                    continue;
                }
                break;
            }
            if n == 0 {
                break;
            }
            resp.extend_from_slice(&buf[..n as usize]);
        }

        let (status, rh, rbody) = parse_http_response(&resp);
        let mut h = vec![(b":status".to_vec(), status.to_string().into_bytes())];
        for (n, v) in rh {
            let nl = String::from_utf8_lossy(&n);
            if nl.eq_ignore_ascii_case("transfer-encoding")
                || nl.eq_ignore_ascii_case("connection")
                || nl.eq_ignore_ascii_case("keep-alive")
            {
                continue;
            }
            h.push((n, v));
        }
        self.send_response(stream_id, h, rbody).await?;
        Ok(status)
    }

    async fn send_response(
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

    async fn send_error(&mut self, stream_id: i64, status: u16, body: &[u8]) -> io::Result<()> {
        let headers = vec![
            (b":status".to_vec(), status.to_string().into_bytes()),
            (b"content-type".to_vec(), b"text/plain".to_vec()),
            (
                b"content-length".to_vec(),
                body.len().to_string().into_bytes(),
            ),
        ];
        self.send_response(stream_id, headers, body.to_vec()).await
    }

    fn flush_h3_to_quic(&mut self, out_pkts: &mut Vec<(SocketAddr, Vec<u8>)>) -> io::Result<()> {
        let ts = timestamp_ns();
        let peer = self.peer;
        loop {
            let mut vecs = [nghttp3_vec {
                base: ptr::null_mut(),
                len: 0,
            }; 16];
            let Some((sid, data, fin)) = ({
                let Some(h3) = self.h3.as_mut() else {
                    break;
                };
                match h3.write_stream(&mut vecs)? {
                    Some((sid, fin, nvec)) => {
                        let mut data = Vec::new();
                        for v in vecs.iter().take(nvec) {
                            if !v.base.is_null() && v.len > 0 {
                                data.extend_from_slice(unsafe {
                                    std::slice::from_raw_parts(v.base, v.len)
                                });
                            }
                        }
                        Some((sid, data, fin))
                    }
                    None => None,
                }
            }) else {
                break;
            };

            let mut off = 0;
            while off < data.len() || (fin && off == data.len()) {
                let chunk = if off < data.len() {
                    &data[off..]
                } else {
                    &[][..]
                };
                let is_fin = fin && off + chunk.len() >= data.len();
                let mut buf = vec![0u8; 1350];
                match self.quic.write_stream(&mut buf, sid, chunk, is_fin, ts) {
                    Ok((pkt_len, accepted)) => {
                        if accepted > 0 {
                            off += accepted;
                            if let Some(h3) = self.h3.as_mut() {
                                let _ = h3.add_write_offset(sid, accepted);
                            }
                        }
                        if pkt_len > 0 {
                            out_pkts.push((peer, buf[..pkt_len].to_vec()));
                        }
                        if accepted == 0 && chunk.is_empty() {
                            if let Some(h3) = self.h3.as_mut() {
                                let _ = h3.add_write_offset(sid, 0);
                            }
                            break;
                        }
                        if is_fin && off >= data.len() {
                            break;
                        }
                        if accepted == 0 && pkt_len == 0 {
                            break;
                        }
                    }
                    Err(e) => {
                        warn!("[HTTP/3/ngtcp2] write_stream: {e}");
                        break;
                    }
                }
            }
        }
        loop {
            let mut buf = vec![0u8; 1350];
            match self.quic.write_pkt(&mut buf, ts) {
                Ok(0) => break,
                Ok(n) => out_pkts.push((peer, buf[..n].to_vec())),
                Err(e) => {
                    debug!("[HTTP/3/ngtcp2] write_pkt: {e}");
                    break;
                }
            }
        }
        Ok(())
    }
}

fn header_value<'a>(headers: &'a [(Vec<u8>, Vec<u8>)], name: &[u8]) -> Option<&'a [u8]> {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_slice())
}

fn parse_http_response(response: &[u8]) -> (u16, Vec<(Vec<u8>, Vec<u8>)>, Vec<u8>) {
    let Some(header_end) = response.windows(4).position(|w| w == b"\r\n\r\n") else {
        return (502, Vec::new(), response.to_vec());
    };
    let header_block = &response[..header_end];
    let body = response[header_end + 4..].to_vec();
    let mut status = 502u16;
    let mut headers = Vec::new();
    for (i, line) in header_block.split(|&b| b == b'\n').enumerate() {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if i == 0 {
            if let Some(code) = line
                .split(|&b| b == b' ')
                .nth(1)
                .and_then(|s| std::str::from_utf8(s).ok())
                .and_then(|s| s.parse().ok())
            {
                status = code;
            }
            continue;
        }
        if let Some(colon) = line.iter().position(|&b| b == b':') {
            let name = line[..colon].to_vec();
            let mut val = &line[colon + 1..];
            if val.starts_with(b" ") {
                val = &val[1..];
            }
            headers.push((name, val.to_vec()));
        }
    }
    (status, headers, body)
}

async fn read_file_offload(path: &Path) -> io::Result<Vec<u8>> {
    let path = path.to_path_buf();
    // コールド/低頻度: offload でイベントループをブロックしない
    crate::runtime::offload::offload(move || std::fs::read(&path)).await
}

struct TempPem {
    path: PathBuf,
}

impl Drop for TempPem {
    fn drop(&mut self) {
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

    let mut recv_buf = vec![0u8; 65536];

    loop {
        if SHUTDOWN_FLAG.load(Ordering::Relaxed) {
            info!("[HTTP/3/ngtcp2] shutdown");
            break;
        }

        let timeout = {
            let conns = connections.borrow();
            let now = timestamp_ns();
            let mut min_wait = Duration::from_millis(50);
            for h in conns.values() {
                let exp = h.quic.get_expiry();
                if exp != u64::MAX && exp > now {
                    let d = Duration::from_nanos(exp - now);
                    if d < min_wait {
                        min_wait = d;
                    }
                }
            }
            min_wait
        };

        let recv_fut = socket.recv_from(std::mem::take(&mut recv_buf));
        let recv = crate::runtime::time::timeout(timeout, recv_fut).await;
        match recv {
            Ok((Ok((n, from)), buf)) => {
                recv_buf = buf;
                let data = &recv_buf[..n];
                process_packet(data, from, bind_addr, &connections, &tls_ctx, &config).await?;
            }
            Ok((Err(e), buf)) => {
                recv_buf = buf;
                if e.kind() != io::ErrorKind::WouldBlock {
                    warn!("[HTTP/3/ngtcp2] recv: {e}");
                }
            }
            Err(_) => {
                // timeout — return buffer from cancelled future is lost; re-alloc if needed
                if recv_buf.is_empty() {
                    recv_buf = vec![0u8; 65536];
                }
                let now = timestamp_ns();
                let mut dead = Vec::new();
                {
                    let mut conns = connections.borrow_mut();
                    for (cid, h) in conns.iter_mut() {
                        let exp = h.quic.get_expiry();
                        if exp != u64::MAX && exp <= now {
                            if h.quic.handle_expiry(now).is_err() {
                                dead.push(cid.clone());
                            }
                        }
                        if h.quic.is_draining() || h.quic.is_closing() {
                            dead.push(cid.clone());
                        }
                    }
                    for c in dead {
                        conns.remove(&c);
                    }
                }
            }
        }

        // drive connections
        let cids: Vec<Vec<u8>> = connections.borrow().keys().cloned().collect();
        let mut out_pkts = Vec::new();
        for cid in cids {
            let mut conns = connections.borrow_mut();
            let Some(h) = conns.get_mut(&cid) else {
                continue;
            };
            let _ = h.try_init_h3();
            let _ = h.feed_stream_data();
            if let Err(e) = h.process_h3_events().await {
                warn!("h3 events: {e}");
            }
            if let Err(e) = h.flush_h3_to_quic(&mut out_pkts) {
                warn!("flush: {e}");
            }
        }

        for (to, pkt) in out_pkts {
            let (_res, _) = socket.send_to(pkt, to).await;
        }
    }
    Ok(())
}

async fn process_packet(
    data: &[u8],
    from: SocketAddr,
    local: SocketAddr,
    connections: &ConnMap,
    tls_ctx: &Rc<TlsContext>,
    config: &Http3ServerConfig,
) -> io::Result<()> {
    let ts = timestamp_ns();

    // 同一 peer の既存接続を優先
    {
        let mut conns = connections.borrow_mut();
        for h in conns.values_mut() {
            if h.peer == from && h.quic.read_pkt(local, from, data, ts).is_ok() {
                return Ok(());
            }
        }
    }

    if let Some((version, _dcid, scid)) = accept_packet(data) {
        let mut new_scid = [0u8; 16];
        let _ = aws_lc_rs::rand::fill(&mut new_scid);
        let tls = tls_ctx.create_session()?;
        let quic = QuicConn::server_new(
            &scid,
            &new_scid,
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
        let mut handler = Handler::new(quic, from);
        if let Err(e) = handler.quic.read_pkt(local, from, data, ts) {
            error!("[HTTP/3/ngtcp2] first read_pkt: {e}");
            return Ok(());
        }
        connections.borrow_mut().insert(new_scid.to_vec(), handler);
        return Ok(());
    }

    // フォールバック: 全接続試行
    let mut conns = connections.borrow_mut();
    for h in conns.values_mut() {
        if h.quic.read_pkt(local, from, data, ts).is_ok() {
            break;
        }
    }
    Ok(())
}
