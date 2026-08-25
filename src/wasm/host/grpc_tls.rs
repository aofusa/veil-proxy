//! gRPC-over-h2/h2c 呼び出し用の TCP/TLS ストリーム抽象（F-134 / F-139）
//!
//! `grpc_executor.rs`（テスト・後方互換用の同期フォールバック）と
//! `grpc_pool.rs`（F-139 の接続プール・ノンブロッキング状態機械）の
//! 両方から共有される。ロジックの重複を避けるため、この 1 箇所へ集約する。

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::Arc;

/// クライアントコネクションプリフェース (RFC 7540 Section 3.5)
pub(super) const CONNECTION_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// gRPC-over-h2/h2c 呼び出し用の TCP ストリーム抽象。
///
/// F-134 フォローアップ: 従来は平文 `TcpStream` 決め打ちで gRPC over TLS（h2, ALPN
/// `h2`）上流に接続できなかった。`src/wasm/http_executor.rs::execute_https_request` と
/// 同じ設計（rustls + webpki-roots のシステムルート）を踏襲し、`Read`/`Write` を
/// 実装した列挙体で呼び出し側のロジック（フレーム送受信ループ）を TLS の有無に
/// 関わらず共通化する。
///
/// F-139: 接続プール（`grpc_pool.rs`）ではこのストリームを `set_nonblocking(true)` で
/// 使う。`Read`/`Write` はブロッキング/ノンブロッキングいずれの下位ソケットに対しても
/// そのまま委譲するだけなので、両方の用途に流用できる。
pub(super) enum ClientStream {
    Plain(TcpStream),
    Tls(Box<rustls::StreamOwned<rustls::ClientConnection, TcpStream>>),
}

impl ClientStream {
    /// 下位 `TcpStream` を非ブロッキングモードに切り替える（F-139 の接続プール専用）。
    pub(super) fn set_nonblocking(&self, nonblocking: bool) -> std::io::Result<()> {
        match self {
            ClientStream::Plain(s) => s.set_nonblocking(nonblocking),
            ClientStream::Tls(s) => s.sock.set_nonblocking(nonblocking),
        }
    }
}

impl Read for ClientStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ClientStream::Plain(s) => s.read(buf),
            ClientStream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for ClientStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ClientStream::Plain(s) => s.write(buf),
            ClientStream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ClientStream::Plain(s) => s.flush(),
            ClientStream::Tls(s) => s.flush(),
        }
    }
}

/// TLS 上流用の rustls `ClientConnection` を確立し、`TcpStream` を包む。
///
/// システムルート（webpki-roots）で検証する。クライアント証明書認証は
/// veil の他の WASM 発呼経路（`proxy_http_call`）と同様に未対応。
pub(super) fn wrap_tls(stream: TcpStream, host: &str) -> Result<ClientStream, String> {
    use rustls::pki_types::ServerName;
    use rustls::ClientConfig;

    let mut root_store = rustls::RootCertStore::empty();
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let mut config = ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    // gRPC over TLS は ALPN `h2` を要求するサーバーが多い。
    config.alpn_protocols = vec![b"h2".to_vec()];

    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| format!("invalid server name: {host}"))?;

    let conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("TLS handshake setup failed: {e}"))?;

    Ok(ClientStream::Tls(Box::new(rustls::StreamOwned::new(
        conn, stream,
    ))))
}
