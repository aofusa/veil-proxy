//! 自己完結型 E2E の共通ハーネス（veil プロセスの起動・停止、平文 HTTP/1.1 クライアント）。
// 理由付き allow: テストハーネスは同期 I/O・sleep・std::net を意図的に使う（データプレーンとは別プロセス）。
#![allow(clippy::disallowed_methods)]

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// cargo がビルドした veil バイナリ。
pub fn veil_bin() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_veil"))
}

/// 設定（TOML の基本文字列）に埋め込むパス。Windows の `\\` はエスケープとして読まれるので `/` にする。
pub fn tp(p: &Path) -> String {
    p.display().to_string().replace('\\', "/")
}

/// 空いている TCP ポートを 1 つ返す（bind して即解放）。
pub fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// 自己署名証明書（localhost / 127.0.0.1）を書き出す。
pub fn write_self_signed(dir: &Path) -> (PathBuf, PathBuf) {
    write_self_signed_named(dir, "cert.pem", "key.pem", "localhost")
}

/// 自己署名証明書を任意のファイル名・CN で書き出す。
pub fn write_self_signed_named(
    dir: &Path,
    cert_name: &str,
    key_name: &str,
    cn: &str,
) -> (PathBuf, PathBuf) {
    let rcgen::CertifiedKey { cert, signing_key } = rcgen::generate_simple_self_signed(vec![
        cn.to_string(),
        "localhost".to_string(),
        "127.0.0.1".to_string(),
    ])
    .unwrap();
    let cert_path = dir.join(cert_name);
    let key_path = dir.join(key_name);
    std::fs::write(&cert_path, cert.pem()).unwrap();
    std::fs::write(&key_path, signing_key.serialize_pem()).unwrap();
    (cert_path, key_path)
}

/// テスト用に起動した veil プロセス。Drop で SIGTERM → 待機（猶予後 SIGKILL）。
pub struct VeilProcess {
    child: Option<Child>,
    log_path: PathBuf,
    _log_dir: tempfile::TempDir,
}

impl VeilProcess {
    /// `-c <config>` で起動する。`env` は追加の環境変数、`args` は追加の引数。
    pub fn spawn(config: &Path, env: &[(&str, &str)], args: &[&str]) -> Self {
        Self::spawn_with(veil_bin().as_path(), config, env, args)
    }

    /// 任意の実行ファイル（ライブラリのサンプル等）を `-c` 無しで起動する場合にも使える汎用版。
    pub fn spawn_with(bin: &Path, config: &Path, env: &[(&str, &str)], args: &[&str]) -> Self {
        let log_dir = tempfile::tempdir().unwrap();
        let log_path = log_dir.path().join("veil.log");
        let log = std::fs::File::create(&log_path).unwrap();
        let mut cmd = Command::new(bin);
        if !config.as_os_str().is_empty() {
            cmd.arg("-c").arg(config);
        }
        cmd.args(args)
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log));
        for (k, v) in env {
            cmd.env(k, v);
        }
        let child = cmd.spawn().expect("failed to spawn veil");
        VeilProcess {
            child: Some(child),
            log_path,
            _log_dir: log_dir,
        }
    }

    pub fn pid(&self) -> i32 {
        self.child.as_ref().unwrap().id() as i32
    }

    /// 標準出力・標準エラーの内容（失敗時の診断用）。
    pub fn log(&self) -> String {
        std::fs::read_to_string(&self.log_path).unwrap_or_default()
    }

    #[cfg(unix)]
    pub fn sighup(&self) {
        unsafe {
            libc::kill(self.pid(), libc::SIGHUP);
        }
    }

    /// SIGTERM を送り、終了を待つ（最大 `wait`）。終了コードを返す（タイムアウトなら None）。
    pub fn terminate(&mut self, wait: Duration) -> Option<std::process::ExitStatus> {
        let mut child = self.child.take()?;
        #[cfg(unix)]
        unsafe {
            libc::kill(child.id() as i32, libc::SIGTERM);
        }
        // Windows にシグナルは無い（終了待ちの代わりに即終了させる）
        #[cfg(windows)]
        let _ = child.kill();
        let deadline = Instant::now() + wait;
        loop {
            if let Ok(Some(st)) = child.try_wait() {
                return Some(st);
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for VeilProcess {
    fn drop(&mut self) {
        let _ = self.terminate(Duration::from_secs(10));
    }
}

/// `port` に接続できるようになるまで待つ。
pub fn wait_for_port(port: u16, timeout: Duration) -> bool {
    wait_until(timeout, || {
        TcpStream::connect_timeout(
            &format!("127.0.0.1:{}", port).parse().unwrap(),
            Duration::from_millis(200),
        )
        .is_ok()
    })
}

/// 条件が真になるまで 100ms 間隔で待つ。
pub fn wait_until(timeout: Duration, mut f: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + timeout;
    loop {
        if f() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

/// 平文 HTTP/1.1 の GET（`Connection: close`）。
pub fn http1_get(port: u16, path: &str) -> std::io::Result<HttpResponse> {
    http1_request(port, "GET", path, &[], b"")
}

/// 平文 HTTP/1.1 の任意リクエスト（`Connection: close`、本文は Content-Length 付き）。
pub fn http1_request(
    port: u16,
    method: &str,
    path: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> std::io::Result<HttpResponse> {
    let mut s = TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", port).parse().unwrap(),
        Duration::from_secs(2),
    )?;
    s.set_read_timeout(Some(Duration::from_secs(10)))?;
    let mut req = format!(
        "{} {} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\nContent-Length: {}\r\n",
        method,
        path,
        body.len()
    );
    for (k, v) in headers {
        req.push_str(&format!("{}: {}\r\n", k, v));
    }
    req.push_str("\r\n");
    s.write_all(req.as_bytes())?;
    s.write_all(body)?;
    let mut raw = Vec::new();
    s.read_to_end(&mut raw)?;
    parse_http1_response(&raw)
}

/// HTTP/1.1 応答（Content-Length / chunked / close 区切り）を解析する。
pub fn parse_http1_response(raw: &[u8]) -> std::io::Result<HttpResponse> {
    let bad = |m: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, m.to_string());
    let head_end = raw
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .ok_or_else(|| bad("no header terminator"))?;
    let head = std::str::from_utf8(&raw[..head_end]).map_err(|_| bad("non-utf8 head"))?;
    let mut lines = head.split("\r\n");
    let status_line = lines.next().ok_or_else(|| bad("empty"))?;
    let status: u16 = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| bad("bad status line"))?;
    let headers: Vec<(String, String)> = lines
        .filter_map(|l| {
            let (n, v) = l.split_once(':')?;
            Some((n.trim().to_string(), v.trim().to_string()))
        })
        .collect();
    let rest = &raw[head_end + 4..];
    let chunked = headers.iter().any(|(n, v)| {
        n.eq_ignore_ascii_case("transfer-encoding") && v.to_ascii_lowercase().contains("chunked")
    });
    let body = if chunked {
        decode_chunked(rest).ok_or_else(|| bad("bad chunked body"))?
    } else if let Some(cl) = headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.parse::<usize>().ok())
    {
        rest[..cl.min(rest.len())].to_vec()
    } else {
        rest.to_vec()
    };
    Ok(HttpResponse {
        status,
        headers,
        body,
    })
}

fn decode_chunked(mut data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    loop {
        let line_end = data.windows(2).position(|w| w == b"\r\n")?;
        let size_str = std::str::from_utf8(&data[..line_end]).ok()?;
        let size = usize::from_str_radix(size_str.split(';').next()?.trim(), 16).ok()?;
        data = &data[line_end + 2..];
        if size == 0 {
            return Some(out);
        }
        out.extend_from_slice(data.get(..size)?);
        data = data.get(size + 2..)?;
    }
}

/// TLS ハンドシェイクしてサーバ証明書（DER の先頭 1 枚）を返す（検証しない）。
pub fn tls_peer_cert(port: u16, sni: &str) -> std::io::Result<Vec<u8>> {
    use std::sync::Arc;
    let _ = veil::tls_provider::provider::default_provider().install_default();
    let config = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(NoVerify))
        .with_no_client_auth();
    let name = rustls::pki_types::ServerName::try_from(sni.to_string())
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut conn = rustls::ClientConnection::new(Arc::new(config), name)
        .map_err(|e| std::io::Error::other(e.to_string()))?;
    let mut sock = TcpStream::connect_timeout(
        &format!("127.0.0.1:{}", port).parse().unwrap(),
        Duration::from_secs(2),
    )?;
    sock.set_read_timeout(Some(Duration::from_secs(5)))?;
    while conn.is_handshaking() {
        conn.complete_io(&mut sock)?;
    }
    conn.peer_certificates()
        .and_then(|c| c.first())
        .map(|c| c.as_ref().to_vec())
        .ok_or_else(|| std::io::Error::other("no peer certificate"))
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

/// h2c（prior knowledge）で GET し、(status, body) を返す（h2 クレート + tokio のテスト専用クライアント）。
pub fn h2c_get(port: u16, path: &str) -> (u16, Vec<u8>) {
    let (status, _, body) = h2c_get_with_headers(port, path);
    (status, body)
}

/// h2c で `body_len` バイトの本文を POST して応答のステータスを返す（`timeout` を過ぎたら None）。
/// 本文は相手の流量制御ウィンドウに従って送る（ウィンドウが補充されなければ止まる）。
pub fn h2c_post(port: u16, path: &str, body_len: usize, timeout: Duration) -> Option<u16> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let run = async {
            let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
                .await
                .expect("connect");
            let (mut client, conn) = h2::client::handshake(tcp).await.expect("h2c handshake");
            tokio::spawn(async move {
                let _ = conn.await;
            });
            let req = http::Request::builder()
                .method("POST")
                .uri(format!("http://localhost:{}{}", port, path))
                .body(())
                .unwrap();
            let (resp, mut send) = client.send_request(req, false).expect("send");
            let resp = tokio::spawn(resp);
            let chunk = bytes::Bytes::from(vec![b'x'; 16 * 1024]);
            let mut left = body_len;
            while left > 0 {
                send.reserve_capacity(left.min(chunk.len()));
                let cap = match std::future::poll_fn(|cx| send.poll_capacity(cx)).await {
                    Some(Ok(n)) => n,
                    // 応答が先に来てストリームが閉じた
                    _ => break,
                };
                let n = cap.min(left);
                if send.send_data(chunk.slice(..n), n == left).is_err() {
                    break;
                }
                left -= n;
            }
            let resp = resp.await.expect("join").expect("response");
            resp.status().as_u16()
        };
        tokio::time::timeout(timeout, run).await.ok()
    })
}

/// `h2c_get` の応答ヘッダ付き版。(status, ヘッダ（名前は小文字）, body)。
pub fn h2c_get_with_headers(port: u16, path: &str) -> (u16, Vec<(String, String)>, Vec<u8>) {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async move {
        let tcp = tokio::net::TcpStream::connect(("127.0.0.1", port))
            .await
            .expect("connect");
        let (mut client, conn) = h2::client::handshake(tcp).await.expect("h2c handshake");
        tokio::spawn(async move {
            let _ = conn.await;
        });
        let req = http::Request::builder()
            .method("GET")
            .uri(format!("http://localhost:{}{}", port, path))
            .body(())
            .unwrap();
        let (resp, _) = client.send_request(req, true).expect("send");
        let resp = resp.await.expect("response");
        let status = resp.status().as_u16();
        let headers = resp
            .headers()
            .iter()
            .map(|(n, v)| {
                (
                    n.as_str().to_string(),
                    String::from_utf8_lossy(v.as_bytes()).into_owned(),
                )
            })
            .collect();
        let mut body = resp.into_body();
        let mut out = Vec::new();
        while let Some(chunk) = body.data().await {
            let chunk = chunk.expect("data");
            let _ = body.flow_control().release_capacity(chunk.len());
            out.extend_from_slice(&chunk);
        }
        (status, headers, out)
    })
}
