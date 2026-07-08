//! QUIC/HTTP/3 クライアント（container_security P-03 用）。
//!
//! 環境変数: VEIL_HOST, VEIL_HTTP3_PORT (default 443)
//! 終了: 0 = 200 系応答ボディ受信、1 = 失敗

use quiche::h3::NameValue;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

fn log_line(path: &str, msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", msg);
    }
    eprintln!("{}", msg);
}

fn main() {
    // QUIC の :authority / SNI は IP よりホスト名が安定（Docker では veil-proxy エイリアス）
    let host = env::var("VEIL_SNI")
        .or_else(|_| env::var("VEIL_HOST"))
        .unwrap_or_else(|_| "veil-proxy".into());
    let port: u16 = env::var("VEIL_HTTP3_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(443);
    let path = env::var("HTTP3_PATH").unwrap_or_else(|_| "/".into());
    let report = env::var("HTTP3_REPORT").unwrap_or_else(|_| "/results/http3_client_report.txt".into());

    let _ = std::fs::write(&report, "");
    log_line(&report, &format!("http3_client start host={} port={} path={}", host, port, path));

    match send_http3_get(&host, port, &path) {
        Ok(size) => {
            log_line(&report, &format!("http3_client: ok body_bytes={}", size));
            std::process::exit(0);
        }
        Err(e) => {
            log_line(&report, &format!("http3_client: FAIL {}", e));
            std::process::exit(1);
        }
    }
}

fn send_http3_get(host: &str, port: u16, path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;
    config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;
    config.verify_peer(false);
    config.set_max_idle_timeout(5000);
    config.set_max_recv_udp_payload_size(1350);
    config.set_max_send_udp_payload_size(1350);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_disable_active_migration(true);

    let local_addr: SocketAddr = "0.0.0.0:0".parse()?;
    let socket = UdpSocket::bind(local_addr)?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;

    let peer_addr: SocketAddr = format!("{}:{}", host, port)
        .to_socket_addrs()?
        .next()
        .ok_or("DNS resolve failed")?;
    socket.connect(peer_addr)?;

    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    getrandom::getrandom(&mut scid).map_err(|_| "random scid failed")?;
    let scid = quiche::ConnectionId::from_ref(&scid);

    let mut conn = quiche::connect(Some(host), &scid, socket.local_addr()?, peer_addr, &mut config)?;

    let mut buf = [0u8; 65535];
    let mut out = [0u8; 1350];

    let (write, _) = conn.send(&mut out)?;
    socket.send(&out[..write])?;

    let start = Instant::now();
    let timeout = Duration::from_secs(15);

    while !conn.is_established() {
        if start.elapsed() > timeout {
            return Err("QUIC handshake timeout".into());
        }
        match socket.recv(&mut buf) {
            Ok(len) => {
                let recv_info = quiche::RecvInfo {
                    from: peer_addr,
                    to: socket.local_addr()?,
                };
                conn.recv(&mut buf[..len], recv_info)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(Box::new(e)),
        }
        while let Ok((write, _)) = conn.send(&mut out) {
            socket.send(&out[..write])?;
        }
    }

    let h3_config = quiche::h3::Config::new()?;
    let mut h3_conn = quiche::h3::Connection::with_transport(&mut conn, &h3_config)?;

    let headers = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
    ];

    let stream_id = h3_conn.send_request(&mut conn, &headers, true)?;
    while let Ok((write, _)) = conn.send(&mut out) {
        socket.send(&out[..write])?;
    }

    let mut response_size = 0usize;
    let mut status: Option<u16> = None;
    let start = Instant::now();

    while start.elapsed() < timeout {
        match socket.recv(&mut buf) {
            Ok(len) => {
                let recv_info = quiche::RecvInfo {
                    from: peer_addr,
                    to: socket.local_addr()?,
                };
                conn.recv(&mut buf[..len], recv_info)?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => {
                if conn.is_closed() {
                    break;
                }
                continue;
            }
            Err(e) => return Err(Box::new(e)),
        }

        loop {
            match h3_conn.poll(&mut conn) {
                Ok((id, quiche::h3::Event::Headers { list, .. })) if id == stream_id => {
                    for h in &list {
                        if h.name() == b":status" {
                            status = std::str::from_utf8(h.value())
                                .ok()
                                .and_then(|s| s.parse().ok());
                        }
                        response_size += h.name().len() + h.value().len();
                    }
                }
                Ok((id, quiche::h3::Event::Data)) if id == stream_id => {
                    while let Ok(read) = h3_conn.recv_body(&mut conn, id, &mut buf) {
                        response_size += read;
                    }
                }
                Ok((id, quiche::h3::Event::Finished)) if id == stream_id => {
                    match status {
                        Some(200) | Some(301) | Some(302) => return Ok(response_size),
                        Some(code) => return Err(format!("unexpected status {}", code).into()),
                        None => return Err("no :status header".into()),
                    }
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => return Err(Box::new(e)),
            }
        }

        while let Ok((write, _)) = conn.send(&mut out) {
            socket.send(&out[..write])?;
        }
        if conn.is_closed() {
            break;
        }
    }

    Err("HTTP/3 response timeout".into())
}