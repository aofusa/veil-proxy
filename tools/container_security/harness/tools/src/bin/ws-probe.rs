//! WebSocket 仕様外コントロールフレーム送信プローブ（P-09）。
//!
//! TLS 上で Upgrade を試行し、接続が維持される場合は不正フレームを送信する。
//! 環境変数: VEIL_HOST, VEIL_HTTPS_PORT, WS_PATH (default /ws/)

use std::env;
use std::fs::OpenOptions;
use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::DigitallySignedStruct;
use rustls::SignatureScheme;

fn log_line(path: &str, msg: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{}", msg);
    }
    eprintln!("{}", msg);
}

fn main() {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let host = env::var("VEIL_HOST").unwrap_or_else(|_| "veil-proxy".into());
    let port: u16 = env::var("VEIL_HTTPS_PORT")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(443);
    let path = env::var("WS_PATH").unwrap_or_else(|_| "/ws/".into());
    let report = env::var("WS_PROBE_REPORT").unwrap_or_else(|_| "/results/ws_probe_report.txt".into());

    let _ = std::fs::write(&report, "");
    log_line(&report, &format!("ws_probe start host={} port={} path={}", host, port, path));

    match run_probe(&host, port, &path, &report) {
        Ok(()) => {
            log_line(&report, "ws_probe: ok");
            std::process::exit(0);
        }
        Err(e) => {
            // Upgrade 拒否・上流不可時の切断は想定内。生存確認は呼び出し側 health で行う。
            log_line(&report, &format!("ws_probe: tolerated_error={}", e));
            log_line(&report, "ws_probe: ok (connection rejected or closed)");
            std::process::exit(0);
        }
    }
}

fn run_probe(host: &str, port: u16, path: &str, report: &str) -> Result<(), Box<dyn std::error::Error>> {
    let addr = format!("{}:{}", host, port);
    let sock = addr
        .to_socket_addrs()?
        .next()
        .ok_or("DNS resolve failed")?;
    let tcp = TcpStream::connect_timeout(&sock, Duration::from_secs(5))?;
    tcp.set_read_timeout(Some(Duration::from_secs(5)))?;
    tcp.set_write_timeout(Some(Duration::from_secs(5)))?;

    let config = Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(SkipVerify))
            .with_no_client_auth(),
    );
    let server_name: ServerName<'static> = host.to_string().try_into()?;
    let mut tls = rustls::ClientConnection::new(config, server_name)?;
    let mut stream = tcp;
    let mut tls_stream = rustls::Stream::new(&mut tls, &mut stream);

    let key = "dGhlIHNhbXBsZSBub25jZQ==";
    let req = format!(
        "GET {} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: {}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        path, host, key
    );
    tls_stream.write_all(req.as_bytes())?;
    tls_stream.flush()?;

    let mut resp = vec![0u8; 4096];
    let n = match tls_stream.read(&mut resp) {
        Ok(n) => n,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => 0,
        Err(e) => return Err(Box::new(e)),
    };
    let resp_str = String::from_utf8_lossy(&resp[..n]);
    log_line(report, &format!("upgrade_response_bytes={}", n));
    let upgraded = resp_str.contains("101");

    if upgraded {
        log_line(report, "upgrade: 101 Switching Protocols");
        send_malformed_ws_frames(&mut tls_stream, report)?;
    } else {
        log_line(report, "upgrade: not 101 (sending raw frames on TLS stream anyway)");
        // 仕様外バイト列を送信（サーバが拒否しても生存することを確認）
        let garbage: [u8; 32] = [
            0xFF, 0xFF, 0xFF, 0xFF, 0x81, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x0A, 0x48, 0x65,
            0x6C, 0x6C, 0x6F, 0xFF, 0xFE, 0xFD, 0xFC, 0xFB, 0xFA, 0xF9, 0xF8, 0xF7, 0xF6, 0xF5,
            0xF4, 0xF3, 0xF2, 0xF1,
        ];
        let _ = tls_stream.write_all(&garbage);
        let _ = tls_stream.flush();
    }

    // 接続を閉じてプロキシ生存を確認（呼び出し側が health を実施）
    Ok(())
}

fn send_malformed_ws_frames<S: Read + Write>(
    stream: &mut S,
    report: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    // RSV ビットセット + 不正 opcode 0x3（未使用）
    stream.write_all(&[0x70, 0x00])?;
    // 巨大 length（16-bit）だがペイロード不足
    stream.write_all(&[0x8F, 0xFF, 0x01, 0x02])?;
    // 不正 ping（マスクなし・クライアントフレームはマスク必須だがサーバ向けに試行）
    stream.write_all(&[0x89, 0x04, b'a', b'b', b'c', b'd'])?;
    // 0x0 継続フレーム単独（プロトコル違反）
    stream.write_all(&[0x00, 0x05, b'h', b'e', b'l', b'l', b'o'])?;
    stream.flush()?;
    log_line(report, "malformed_ws_frames: sent");
    Ok(())
}

#[derive(Debug)]
struct SkipVerify;

impl ServerCertVerifier for SkipVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        vec![
            SignatureScheme::RSA_PKCS1_SHA256,
            SignatureScheme::ECDSA_NISTP256_SHA256,
            SignatureScheme::ED25519,
        ]
    }
}