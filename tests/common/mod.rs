//! テスト用共通ヘルパーモジュール
//!
//! 統合テストおよびE2Eテストで使用する共通のユーティリティを提供します。

use std::net::{TcpListener, SocketAddr};
use std::path::PathBuf;
use std::time::Duration;
use std::io::{Read, Write};

/// テスト用の自己署名TLS証明書を生成
pub fn generate_test_certs(output_dir: &std::path::Path) -> std::io::Result<(PathBuf, PathBuf)> {
    use rcgen::{generate_simple_self_signed, CertifiedKey};
    
    let subject_alt_names = vec![
        "localhost".to_string(),
        "127.0.0.1".to_string(),
        "::1".to_string(),
    ];
    
    let CertifiedKey { cert, signing_key } = generate_simple_self_signed(subject_alt_names)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e))?;
    
    let cert_path = output_dir.join("test_cert.pem");
    let key_path = output_dir.join("test_key.pem");
    
    std::fs::write(&cert_path, cert.pem())?;
    std::fs::write(&key_path, signing_key.serialize_pem())?;
    
    Ok((cert_path, key_path))
}

/// 動的に空きポートを取得
pub fn get_available_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().port()
}

/// 複数の空きポートを取得
pub fn get_available_ports(count: usize) -> Vec<u16> {
    let mut ports = Vec::with_capacity(count);
    let mut listeners = Vec::with_capacity(count);
    
    for _ in 0..count {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        ports.push(listener.local_addr().unwrap().port());
        listeners.push(listener);
    }
    
    // リスナーはドロップされてポートが解放される
    ports
}

/// テスト用エコーサーバー
pub struct EchoServer {
    listener: Option<TcpListener>,
    handle: Option<std::thread::JoinHandle<()>>,
    pub addr: SocketAddr,
}

impl EchoServer {
    /// 新しいエコーサーバーを起動
    pub fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        
        // リスナーをクローンしてスレッドに渡す
        let listener_clone = listener.try_clone().unwrap();
        
        let handle = std::thread::spawn(move || {
            // 最初の接続のみ処理
            if let Ok((mut stream, _)) = listener_clone.accept() {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                let mut buf = [0u8; 1024];
                loop {
                    match stream.read(&mut buf) {
                        Ok(0) => break,
                        Ok(n) => {
                            if stream.write_all(&buf[..n]).is_err() {
                                break;
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
        });
        
        Self {
            listener: Some(listener),
            handle: Some(handle),
            addr,
        }
    }
    
    /// サーバーのアドレスを取得
    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }
    
    /// サーバーのポートを取得
    #[allow(dead_code)] // APIの一貫性のため保持
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

impl Drop for EchoServer {
    fn drop(&mut self) {
        // リスナーをドロップして接続を終了
        self.listener.take();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// テスト用HTTPサーバー（シンプルなレスポンス）
pub struct SimpleHttpServer {
    handle: Option<std::thread::JoinHandle<()>>,
    pub addr: SocketAddr,
    shutdown: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl SimpleHttpServer {
    /// 新しいHTTPサーバーを起動
    /// 
    /// # Arguments
    /// * `response_body` - 返却するレスポンスボディ
    /// * `server_id` - サーバー識別子（X-Server-Id ヘッダーに使用）
    pub fn start(response_body: &'static str, server_id: &'static str) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let shutdown = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let shutdown_clone = shutdown.clone();
        
        let _ = listener.set_nonblocking(true);
        
        let handle = std::thread::spawn(move || {
            while !shutdown_clone.load(std::sync::atomic::Ordering::Relaxed) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let _ = stream.set_read_timeout(Some(Duration::from_millis(100)));
                        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
                        
                        // リクエストを読み取る
                        let mut buf = [0u8; 4096];
                        let _ = stream.read(&mut buf);
                        
                        // シンプルなHTTPレスポンス
                        let response = format!(
                            "HTTP/1.1 200 OK\r\n\
                             Content-Type: text/plain\r\n\
                             Content-Length: {}\r\n\
                             X-Server-Id: {}\r\n\
                             Connection: close\r\n\
                             \r\n\
                             {}",
                            response_body.len(),
                            server_id,
                            response_body
                        );
                        
                        let _ = stream.write_all(response.as_bytes());
                    }
                    Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => break,
                }
            }
        });
        
        Self {
            handle: Some(handle),
            addr,
            shutdown,
        }
    }
    
    /// サーバーのアドレスを取得
    pub fn address(&self) -> String {
        format!("127.0.0.1:{}", self.addr.port())
    }
    
    /// サーバーのポートを取得
    pub fn port(&self) -> u16 {
        self.addr.port()
    }
}

impl Drop for SimpleHttpServer {
    fn drop(&mut self) {
        self.shutdown.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}


/// テスト用設定ファイルを生成
pub fn generate_test_config(
    https_port: u16,
    http_port: u16,
    cert_path: &std::path::Path,
    key_path: &std::path::Path,
    backend_urls: &[String],
    output_path: &std::path::Path,
) -> std::io::Result<()> {
    let backends: String = backend_urls.iter()
        .map(|url| format!("\"{}\"", url))
        .collect::<Vec<_>>()
        .join(", ");
    
    let config = format!(r#"
[server]
listen = "127.0.0.1:{https_port}"
http_listen = "127.0.0.1:{http_port}"
redirect_http_to_https = false
threads = 1

[tls]
cert_path = "{cert_path}"
key_path = "{key_path}"

[logging]
level = "debug"

[upstreams."backend"]
algorithm = "round_robin"
servers = [{backends}]

[path_routes."localhost"."/"]
type = "Upstream"
upstream = "backend"

[path_routes."localhost"."/".security]
add_response_headers = {{ "X-Test-Header" = "test-value", "X-Proxied-By" = "veil" }}
remove_response_headers = ["Server"]

[path_routes."127.0.0.1"."/"]
type = "Upstream"
upstream = "backend"
"#,
        https_port = https_port,
        http_port = http_port,
        cert_path = cert_path.display(),
        key_path = key_path.display(),
        backends = backends,
    );
    
    std::fs::write(output_path, config)
}

/// ポートが利用可能になるまで待機
pub fn wait_for_port(port: u16, timeout: Duration) -> bool {
    let start = std::time::Instant::now();
    while start.elapsed() < timeout {
        if std::net::TcpStream::connect(format!("127.0.0.1:{}", port)).is_ok() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}


#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_get_available_port() {
        let port = get_available_port();
        assert!(port > 0);
    }
    
    #[test]
    fn test_get_multiple_ports() {
        let ports = get_available_ports(5);
        assert_eq!(ports.len(), 5);
        
        // 全て異なるポート
        let mut unique = std::collections::HashSet::new();
        for port in &ports {
            unique.insert(*port);
        }
        assert_eq!(unique.len(), 5);
    }
    
    #[test]
    fn test_echo_server() {
        let server = EchoServer::start();
        let addr = server.address();
        
        let mut stream = std::net::TcpStream::connect(&addr).unwrap();
        stream.write_all(b"hello").unwrap();
        
        let mut buf = [0u8; 5];
        stream.read_exact(&mut buf).unwrap();
        assert_eq!(&buf, b"hello");
    }
    
    #[test]
    fn test_simple_http_server() {
        let server = SimpleHttpServer::start("test response", "server1");
        
        // シンプルなHTTPリクエスト
        let mut stream = std::net::TcpStream::connect(server.address()).unwrap();
        stream.write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n").unwrap();
        
        let mut response = String::new();
        let _ = stream.set_read_timeout(Some(Duration::from_secs(1)));
        let _ = stream.read_to_string(&mut response);
        
        assert!(response.contains("200 OK"));
        assert!(response.contains("test response"));
        assert!(response.contains("X-Server-Id: server1"));
    }
}

