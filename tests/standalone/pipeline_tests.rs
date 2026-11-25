//! F-322: HTTP/1.1 のパイプラインの応答をまとめて送る（次の要求が届いている間は応答を溜める）経路の回帰試験。
//!
//! 応答の順・途中の `Connection: close`・溜める量の上限・ソケットへ直接書く経路（静的配信の sendfile・上流への中継）が
//! 溜めた応答の後に来るとき・次の要求が届いていない要求で溜めた分を送ること（送らずに待つとクライアントが止まる）を、
//! 平文と TLS（rustls）の HTTP/1.1 で確かめる。

use super::standalone::*;
use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// 固定応答・静的配信・上流への中継・リダイレクトのルートを、平文と TLS の HTTP/1.1 で受ける veil を起動する。
/// 戻り値は（プロセス, 平文のポート, TLS のポート）
fn spawn_pipeline_mix(dir: &Path, upstream: u16) -> (VeilProcess, u16, u16) {
    let (cert, key) = write_self_signed(dir);
    let www = dir.join("www");
    std::fs::create_dir_all(&www).unwrap();
    std::fs::write(www.join("a.txt"), "static-a").unwrap();
    let port = free_port();
    let tls_port = free_port();
    let big = "b".repeat(200);
    let config = format!(
        r#"
[server]
listen = "127.0.0.1:{tls_port}"
listen_cleartext = "127.0.0.1:{port}"
enable_secure_redirect = false
threads = 1
[logging]
level = "warn"
[tls]
cert_path = "{cert}"
key_path = "{key}"
ktls_enabled = false

[[route]]
[route.conditions]
path = "/fixed"
[route.action]
type = "Respond"
status = 200
body = "f"

[[route]]
[route.conditions]
path = "/big"
[route.action]
type = "Respond"
status = 200
body = "{big}"

[[route]]
[route.conditions]
path = "/redir"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/next"
redirect_status = 302
preserve_path = false

[[route]]
[route.conditions]
path = "/proxy/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:{upstream}/"

[[route]]
[route.conditions]
path = "/static/*"
[route.action]
type = "File"
path = "{www}"
"#,
        cert = tp(&cert),
        key = tp(&key),
        www = tp(&www),
    );
    let cfg_path = dir.join("config.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let veil = VeilProcess::spawn(&cfg_path, &[], &[]);
    assert!(
        wait_for_port(port, Duration::from_secs(20))
            && wait_for_port(tls_port, Duration::from_secs(20)),
        "{}",
        veil.log()
    );
    (veil, port, tls_port)
}

/// 上流: 要求の本文をそのまま返す（GET は空の本文）。接続ごとに 1 要求で閉じる
fn spawn_echo_upstream() -> u16 {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming() {
            let Ok(mut s) = s else { continue };
            std::thread::spawn(move || {
                let _ = s.set_read_timeout(Some(Duration::from_secs(10)));
                let mut buf = Vec::new();
                let mut tmp = [0u8; 4096];
                let (end, cl) = loop {
                    if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                        let head = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
                        let cl: usize = head
                            .lines()
                            .find_map(|l| l.strip_prefix("content-length:"))
                            .and_then(|v| v.trim().parse().ok())
                            .unwrap_or(0);
                        break (end + 4, cl);
                    }
                    match s.read(&mut tmp) {
                        Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
                        _ => return,
                    }
                };
                while buf.len() < end + cl {
                    match s.read(&mut tmp) {
                        Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
                        _ => return,
                    }
                }
                let body = &buf[end..end + cl];
                let mut resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                )
                .into_bytes();
                resp.extend_from_slice(body);
                let _ = s.write_all(&resp);
            });
        }
    });
    port
}

/// 読んだ応答（状態・`Connection` ヘッダ・`Location`・本文）
#[derive(Debug, PartialEq)]
struct Resp {
    status: u16,
    close: bool,
    location: Option<String>,
    body: String,
}

/// HTTP/1.1 の応答を 1 つ読む（`head` は HEAD の応答）。`buf` は前の応答の後ろに読んだ分を持ち越す。
/// 接続が閉じた・時間切れなら `None`
fn read_resp(s: &mut impl Read, buf: &mut Vec<u8>, head: bool) -> Option<Resp> {
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let h = String::from_utf8_lossy(&buf[..end]).to_ascii_lowercase();
            let cl: usize = if head {
                0
            } else {
                h.lines()
                    .find_map(|l| l.strip_prefix("content-length:"))
                    .map(|v| v.trim().parse().unwrap())
                    .unwrap_or(0)
            };
            if buf.len() >= end + 4 + cl {
                let status = h[9..12].parse().unwrap();
                let close = h
                    .lines()
                    .filter_map(|l| l.strip_prefix("connection:"))
                    .any(|v| v.contains("close"));
                let location = h
                    .lines()
                    .find_map(|l| l.strip_prefix("location:"))
                    .map(|v| v.trim().to_string());
                let body = String::from_utf8_lossy(&buf[end + 4..end + 4 + cl]).to_string();
                buf.drain(..end + 4 + cl);
                return Some(Resp {
                    status,
                    close,
                    location,
                    body,
                });
            }
        }
        let mut tmp = [0u8; 8192];
        match s.read(&mut tmp) {
            Ok(n) if n > 0 => buf.extend_from_slice(&tmp[..n]),
            _ => return None,
        }
    }
}

fn get(path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: localhost\r\n\r\n")
}

/// 1 本の接続で確かめる: `rw` は平文の `TcpStream` か TLS のストリーム
fn check_pipelines(name: &str, s: &mut (impl Read + Write), log: &dyn Fn() -> String) {
    let mut buf = Vec::new();

    // 1. 次の要求が届いていない要求で、溜めた分を送る（3 本だけ送って 3 本とも返る。送らずに待つと止まる）
    let batch: String = [get("/fixed"), get("/fixed"), get("/fixed")].concat();
    s.write_all(batch.as_bytes()).unwrap();
    s.flush().unwrap();
    for i in 0..3 {
        let r = read_resp(s, &mut buf, false)
            .unwrap_or_else(|| panic!("{name}: response {i} of 3 was not sent\n{}", log()));
        assert_eq!(
            (r.status, r.body.as_str(), r.close),
            (200, "f", false),
            "{name}"
        );
    }

    // 2. 溜めた応答の後に、ソケットへ直接書く経路（sendfile）・上流への中継・リダイレクト・HEAD が来ても順が崩れない
    let batch: String = [
        get("/fixed"),
        get("/static/a.txt"),
        get("/fixed"),
        "POST /proxy/echo HTTP/1.1\r\nHost: localhost\r\nContent-Length: 2\r\n\r\npp".to_string(),
        get("/fixed"),
        get("/redir"),
        "HEAD /fixed HTTP/1.1\r\nHost: localhost\r\n\r\n".to_string(),
        get("/big"),
        get("/fixed"),
    ]
    .concat();
    s.write_all(batch.as_bytes()).unwrap();
    s.flush().unwrap();
    let big = "b".repeat(200);
    let want: [(u16, &str, bool); 9] = [
        (200, "f", false),
        (200, "static-a", false),
        (200, "f", false),
        (200, "pp", false),
        (200, "f", false),
        (302, "", false),
        (200, "", true),
        (200, big.as_str(), false),
        (200, "f", false),
    ];
    for (i, (status, body, head)) in want.iter().enumerate() {
        let r = read_resp(s, &mut buf, *head).unwrap_or_else(|| {
            panic!(
                "{name}: response {i} of the mixed batch is missing\n{}",
                log()
            )
        });
        assert_eq!(
            (r.status, r.body.as_str()),
            (*status, *body),
            "{name}: response {i}"
        );
        if *status == 302 {
            assert_eq!(
                r.location.as_deref(),
                Some("https://example.com/next"),
                "{name}"
            );
        }
    }

    // 3. 溜める量の上限を何度も超える数（応答は計 80KB 超）でも、全部が順に返る
    let mut batch = String::new();
    for i in 0..300 {
        batch.push_str(&get(if i % 3 == 0 { "/fixed" } else { "/big" }));
    }
    s.write_all(batch.as_bytes()).unwrap();
    s.flush().unwrap();
    for i in 0..300 {
        let r = read_resp(s, &mut buf, false)
            .unwrap_or_else(|| panic!("{name}: response {i} of 300 is missing\n{}", log()));
        let want = if i % 3 == 0 { "f" } else { big.as_str() };
        assert_eq!(
            (r.status, r.body.as_str()),
            (200, want),
            "{name}: response {i} of 300"
        );
    }

    // 4. 途中の `Connection: close`: その応答に `close` を付けて閉じ、後ろの要求には答えない
    let batch: String = [
        get("/fixed"),
        get("/fixed"),
        "GET /fixed HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n".to_string(),
        get("/fixed"),
    ]
    .concat();
    s.write_all(batch.as_bytes()).unwrap();
    s.flush().unwrap();
    for i in 0..3 {
        let r = read_resp(s, &mut buf, false).unwrap_or_else(|| {
            panic!(
                "{name}: response {i} before the close is missing\n{}",
                log()
            )
        });
        assert_eq!(
            (r.status, r.body.as_str(), r.close),
            (200, "f", i == 2),
            "{name}: response {i}"
        );
    }
    assert!(
        read_resp(s, &mut buf, false).is_none(),
        "{name}: a request after `Connection: close` was answered"
    );
}

/// F-322: 平文の HTTP/1.1 のパイプライン
#[test]
fn f322_pipelined_responses_cleartext() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_echo_upstream();
    let (veil, port, _) = spawn_pipeline_mix(dir.path(), upstream);
    let mut s = connect_local(port, Duration::from_secs(2)).unwrap();
    s.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    check_pipelines("cleartext", &mut s, &|| veil.log());
}

/// F-322: TLS（rustls）の HTTP/1.1 のパイプライン（溜めた平文をまとめて暗号化して 1 回で送る）
#[test]
fn f322_pipelined_responses_tls() {
    let dir = tempfile::tempdir().unwrap();
    let upstream = spawn_echo_upstream();
    let (veil, _, tls_port) = spawn_pipeline_mix(dir.path(), upstream);
    let _ = veil::tls_provider::provider::default_provider().install_default();
    let mut config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let name = rustls::pki_types::ServerName::try_from("localhost").unwrap();
    let conn = rustls::ClientConnection::new(Arc::new(config), name).unwrap();
    let sock = connect_local(tls_port, Duration::from_secs(2)).unwrap();
    sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    let mut tls = rustls::StreamOwned::new(conn, sock);
    check_pipelines("tls", &mut tls, &|| veil.log());
}

#[derive(Debug)]
struct NoVerify;

impl rustls::client::danger::ServerCertVerifier for NoVerify {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
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
        veil::tls_provider::provider::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}
