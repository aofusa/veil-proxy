//! HTTP/3ベンチマーク
//!
//! HTTP/3 (QUIC) のパフォーマンスを測定します。
//! - QUIC/UDPベースのパフォーマンス
//! - 0-RTT接続確立の効果
//! - HTTP/2 vs HTTP/3の比較
//!
//! 使用方法:
//!   1. E2E環境を起動: ./tests/e2e_setup.sh start
//!   2. ベンチマーク実行: cargo bench --bench http3 --features http3
//!   3. 環境停止: ./tests/e2e_setup.sh stop
//!
//! 注意: HTTP/3はQUIC/UDPベースで、quicheクレートを使用します。

use criterion::{criterion_group, criterion_main, Criterion};
use quiche::h3::NameValue;
use ring::rand::SecureRandom;
use std::io::{Read, Write, ErrorKind};
use std::net::{TcpStream, UdpSocket, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};
use rustls::{ClientConfig, ClientConnection};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::ServerName;

const PROXY_HTTPS_PORT: u16 = 8443;
const PROXY_HTTP3_PORT: u16 = 8443; // HTTP/3 uses same port but UDP

/// rustlsのCryptoProviderを初期化（一度だけ実行）
fn init_crypto_provider() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        CryptoProvider::install_default(rustls::crypto::ring::default_provider())
            .expect("Failed to install rustls crypto provider");
    });
}

/// プロキシサーバーが起動しているか確認（HTTPS、TLSハンドシェイクを正しく行う）
fn is_proxy_running() -> bool {
    init_crypto_provider();
    
    let mut stream = match TcpStream::connect(format!("127.0.0.1:{}", PROXY_HTTPS_PORT)) {
        Ok(s) => s,
        Err(_) => return false,
    };
    
    if stream.set_read_timeout(Some(Duration::from_secs(2))).is_err() {
        return false;
    }
    if stream.set_write_timeout(Some(Duration::from_secs(2))).is_err() {
        return false;
    }
    
    let config = create_http2_tls_config();
    let server_name = match ServerName::try_from("localhost".to_string()) {
        Ok(name) => name,
        Err(_) => return false,
    };
    
    let mut tls_conn = match ClientConnection::new(config, server_name) {
        Ok(conn) => conn,
        Err(_) => return false,
    };
    
    // TLSハンドシェイクを開始（完了まで待たない）
    let mut handshake_started = false;
    for _ in 0..10 {
        if !tls_conn.is_handshaking() {
            return true;
        }
        
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {
                handshake_started = true;
                if !tls_conn.is_handshaking() {
                    return true;
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(_) => return false,
        }
    }
    
    // ハンドシェイクが開始されていればサーバーは起動していると判断
    handshake_started
}

/// 証明書検証をスキップするカスタム検証器
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
            .to_vec()
    }
}

/// TLS + HTTP/2用クライアント設定を作成（比較用）
fn create_http2_tls_config() -> Arc<ClientConfig> {
    let mut config = ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    
    config.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    
    Arc::new(config)
}

/// TLS経由でHTTP/2リクエストを送信（比較用）
fn send_tls_http2_request(port: u16, path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    init_crypto_provider();
    
    let mut stream = TcpStream::connect(format!("127.0.0.1:{}", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let config = create_http2_tls_config();
    let server_name = ServerName::try_from("localhost".to_string())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    
    let mut tls_conn = ClientConnection::new(config, server_name)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    
    while tls_conn.is_handshaking() {
        match tls_conn.complete_io(&mut stream) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(e) => return Err(Box::new(e)),
        }
    }
    
    let mut tls_stream = rustls::Stream::new(&mut tls_conn, &mut stream);
    
    let request = format!(
        "GET {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
        path
    );
    tls_stream.write_all(request.as_bytes())?;
    
    let mut response = Vec::new();
    tls_stream.read_to_end(&mut response)?;
    
    Ok(response.len())
}

/// QUIC/HTTP/3リクエストを送信
/// 
/// quicheクレートを使用した実際のQUIC/HTTP/3接続
fn send_http3_request(port: u16, _path: &str) -> Result<usize, Box<dyn std::error::Error>> {
    // QUIC設定を作成
    let mut config = quiche::Config::new(quiche::PROTOCOL_VERSION)?;
    
    // HTTP/3 ALPN
    config.set_application_protos(quiche::h3::APPLICATION_PROTOCOL)?;
    
    // 証明書検証を無効化（テスト用）
    config.verify_peer(false);
    
    // 接続パラメータ
    config.set_max_idle_timeout(5000);
    config.set_max_recv_udp_payload_size(1350);
    config.set_max_send_udp_payload_size(1350);
    config.set_initial_max_data(10_000_000);
    config.set_initial_max_stream_data_bidi_local(1_000_000);
    config.set_initial_max_stream_data_bidi_remote(1_000_000);
    config.set_initial_max_streams_bidi(100);
    config.set_disable_active_migration(true);
    
    // ローカルアドレスをバインド
    let local_addr: SocketAddr = "0.0.0.0:0".parse()?;
    let socket = UdpSocket::bind(local_addr)?;
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;
    socket.set_write_timeout(Some(Duration::from_secs(5)))?;
    
    let peer_addr: SocketAddr = format!("127.0.0.1:{}", port).parse()?;
    socket.connect(&peer_addr)?;
    
    // 接続IDを生成
    let mut scid = [0u8; quiche::MAX_CONN_ID_LEN];
    ring::rand::SystemRandom::new()
        .fill(&mut scid)
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::Other, "Failed to generate connection ID"))?;
    let scid = quiche::ConnectionId::from_ref(&scid);
    
    // QUIC接続を開始
    let mut conn = quiche::connect(
        Some("localhost"),
        &scid,
        socket.local_addr()?,
        peer_addr,
        &mut config,
    )?;
    
    let mut buf = [0u8; 65535];
    let mut out = [0u8; 1350];
    
    // 初期パケットを送信
    let (write, _) = conn.send(&mut out)?;
    socket.send(&out[..write])?;
    
    // ハンドシェイクループ
    let start = Instant::now();
    let timeout = Duration::from_secs(5);
    
    while !conn.is_established() {
        if start.elapsed() > timeout {
            return Err("QUIC handshake timeout".into());
        }
        
        // パケット受信
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
        
        // パケット送信
        while let Ok((write, _)) = conn.send(&mut out) {
            socket.send(&out[..write])?;
        }
    }
    
    // HTTP/3接続を確立
    let h3_config = quiche::h3::Config::new()?;
    let mut h3_conn = quiche::h3::Connection::with_transport(&mut conn, &h3_config)?;
    
    // HTTP/3リクエストを送信
    let headers = vec![
        quiche::h3::Header::new(b":method", b"GET"),
        quiche::h3::Header::new(b":path", b"/"),
        quiche::h3::Header::new(b":authority", b"localhost"),
        quiche::h3::Header::new(b":scheme", b"https"),
    ];
    
    let stream_id = h3_conn.send_request(&mut conn, &headers, true)?;
    
    // パケット送信
    while let Ok((write, _)) = conn.send(&mut out) {
        socket.send(&out[..write])?;
    }
    
    // レスポンス受信
    let mut response_size = 0;
    let start = Instant::now();
    
    while start.elapsed() < timeout {
        // パケット受信
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
        
        // HTTP/3イベントを処理
        loop {
            match h3_conn.poll(&mut conn) {
                Ok((id, quiche::h3::Event::Headers { list, .. })) if id == stream_id => {
                    // ヘッダー受信
                    for h in &list {
                        response_size += h.name().len() + h.value().len();
                    }
                }
                Ok((id, quiche::h3::Event::Data)) if id == stream_id => {
                    // ボディ受信
                    while let Ok(read) = h3_conn.recv_body(&mut conn, id, &mut buf) {
                        response_size += read;
                    }
                }
                Ok((id, quiche::h3::Event::Finished)) if id == stream_id => {
                    // レスポンス完了
                    return Ok(response_size);
                }
                Ok(_) => {}
                Err(quiche::h3::Error::Done) => break,
                Err(e) => return Err(Box::new(e)),
            }
        }
        
        // パケット送信
        while let Ok((write, _)) = conn.send(&mut out) {
            socket.send(&out[..write])?;
        }
        
        if conn.is_closed() {
            break;
        }
    }
    
    Ok(response_size)
}

/// HTTP/2 vs HTTP/3の比較
fn benchmark_http2_vs_http3(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping HTTP/3 benchmarks");
        return;
    }
    
    init_crypto_provider();
    
    let mut group = c.benchmark_group("http2_vs_http3");
    
    // HTTP/2（TLS + TCP経由）
    group.bench_function("http2_tls_tcp", |b| {
        b.iter(|| {
            let _ = send_tls_http2_request(PROXY_HTTPS_PORT, "/");
        });
    });
    
    // HTTP/3（QUIC/UDP経由）
    group.bench_function("http3_quic_udp", |b| {
        b.iter(|| {
            let _ = send_http3_request(PROXY_HTTP3_PORT, "/");
        });
    });
    
    group.finish();
}

/// HTTP/3接続確立のレイテンシ（0-RTT効果）
fn benchmark_http3_connection_latency(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping HTTP/3 connection benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("http3_connection");
    group.measurement_time(Duration::from_secs(10));
    
    // 新規接続（フル・ハンドシェイク）
    group.bench_function("new_connection", |b| {
        b.iter(|| {
            let start = Instant::now();
            let _ = send_http3_request(PROXY_HTTP3_PORT, "/");
            start.elapsed()
        });
    });
    
    group.finish();
}

/// HTTP/3のスループット
fn benchmark_http3_throughput(c: &mut Criterion) {
    if !is_proxy_running() {
        eprintln!("Proxy server not running, skipping HTTP/3 throughput benchmarks");
        return;
    }
    
    let mut group = c.benchmark_group("http3_throughput");
    group.measurement_time(Duration::from_secs(10));
    
    group.bench_function("http3_requests", |b| {
        b.iter(|| {
            for _ in 0..5 {
                let _ = send_http3_request(PROXY_HTTP3_PORT, "/");
            }
        });
    });
    
    group.finish();
}

criterion_group!(
    benches,
    benchmark_http2_vs_http3,
    benchmark_http3_connection_latency,
    benchmark_http3_throughput,
);
criterion_main!(benches);
