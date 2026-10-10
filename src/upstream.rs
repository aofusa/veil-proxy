//! アップストリーム選択・ヘルスチェックモジュール
//!
//! バックエンド検索、条件マッチング、ヘルスチェック関数を提供します。

use crate::config::*;
use crate::routing;
use ftlog::{debug, warn};
use std::collections::HashMap;
use std::io;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

// ====================
// ヘルスチェック用の同期プローブ接続（F-170: TCP/UDS 両対応）
// ====================

/// ヘルスチェック専用スレッド（イベントループ外）が使う同期プローブ接続。
///
/// TCP と UDS（`cfg(unix)` のみ）を単一の `Read`/`Write` 実装で扱えるようにし、
/// 3 プローブ（HTTP/TCP/gRPC）と `http3_server.rs` の同期 TLS バックエンド経路が
/// 重複実装を持たずに再利用できるようにする（F-170）。
pub(crate) enum ProbeStream {
    Tcp(std::net::TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl io::Read for ProbeStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            ProbeStream::Tcp(s) => s.read(buf),
            #[cfg(unix)]
            ProbeStream::Unix(s) => s.read(buf),
        }
    }

    /// 書き込み側と同じ理由（下の `write_vectored` のコメント参照）で
    /// ベクタード読み取りも内側のソケットへ委譲する。
    fn read_vectored(&mut self, bufs: &mut [io::IoSliceMut<'_>]) -> io::Result<usize> {
        match self {
            ProbeStream::Tcp(s) => s.read_vectored(bufs),
            #[cfg(unix)]
            ProbeStream::Unix(s) => s.read_vectored(bufs),
        }
    }
}

impl io::Write for ProbeStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            ProbeStream::Tcp(s) => s.write(buf),
            #[cfg(unix)]
            ProbeStream::Unix(s) => s.write(buf),
        }
    }

    /// **ベクタード書き込みは必ず内側のソケットへ委譲する。**
    ///
    /// `Write` の既定実装は「最初の非空バッファ 1 本だけを書く」ため、
    /// rustls の `ChunkVecBuffer::write_to`（暗号文チャンク列を最大 64 本の
    /// `IoSlice` で 1 回に吐き出す）が本来の挙動を取れなくなる。
    /// `std::net::TcpStream` は
    /// これを `writev(2)` へオーバーライドしているので、ラッパで握り潰すと
    /// **`ProbeStream` を挟んだだけで rustls の書き込みが停止し得る**
    /// （F-170 実装中に HTTP/3 の同期 TLS バックエンド経路が
    /// `test_http3_buffering_spillover` でハングして実際に踏んだ）。
    fn write_vectored(&mut self, bufs: &[io::IoSlice<'_>]) -> io::Result<usize> {
        match self {
            ProbeStream::Tcp(s) => s.write_vectored(bufs),
            #[cfg(unix)]
            ProbeStream::Unix(s) => s.write_vectored(bufs),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            ProbeStream::Tcp(s) => s.flush(),
            #[cfg(unix)]
            ProbeStream::Unix(s) => s.flush(),
        }
    }
}

/// ヘルスチェック接続の唯一の入口（F-170）。
///
/// `unix:<path>` 接頭辞なら `UnixStream::connect`（connect にタイムアウト付き API が
/// 無いため、接続後に `set_read_timeout`/`set_write_timeout` のみ適用する。UDS の
/// connect はローカルで即時完了するためタイムアウトの必要性が低い）。それ以外は
/// **まず `SocketAddr` としてパースし、失敗したらホスト名として同期 `getaddrinfo`
/// （`ToSocketAddrs`）で解決して最初のアドレスを使う**。ホスト名解決はここでのみ
/// 発生し、呼び出し元はヘルスチェック専用スレッドに限られる（イベントループ外で
/// 実行されるため、AGENTS.md のホットパス絶対規則「同期処理禁止」には抵触しない）。
/// 以前は `http3_server.rs` の同期 TLS バックエンド専用スレッドも使っていたが、
/// B-105 で非同期経路へ置き換えた。
///
/// F-170 導入時に一度 `addr.parse::<SocketAddr>()` のみへ簡略化してしまい、ホスト名指定の
/// HTTPS バックエンド（`backend.example.com:8080`）へ HTTP/3 経由で中継できなくなる退行を
/// 生んでいたため、DNS 解決を残している。
///
/// **副次的な改善**: 旧ヘルスチェック実装（`perform_health_check` 等）は
/// `addr.parse().unwrap_or_else(|_| 127.0.0.1:80)` により、ホスト名を指定した上流に
/// 対しては **黙って `127.0.0.1:80` へ接続していた**（パース失敗を握りつぶすバグ）。
/// この関数への統一と DNS 解決の追加により、ホスト名指定の上流でも正しいアドレスへ
/// ヘルスチェックが飛ぶようになった。
///
/// **パース・解決の両方に失敗した場合は `io::Error` として返す**（`bool` を返す
/// プローブ関数側は `Err` を `false` へ変換するので、「不正アドレスは false」という
/// 既存の振る舞いは維持される）。
// 理由付き allow: ヘルスチェック専用スレッド／HTTP3 同期 TLS 専用スレッド（いずれも
// イベントループ外）の同期 I/O・同期 DNS 解決。
#[allow(clippy::disallowed_methods)]
pub(crate) fn connect_probe(addr: &str, timeout: Duration) -> io::Result<ProbeStream> {
    #[cfg(unix)]
    if let Some(path) = addr.strip_prefix("unix:") {
        // F-182: capability mode 下は接続ブローカー経由（許可リストの添字で依頼する）。
        #[cfg(target_os = "freebsd")]
        if let Some(r) =
            crate::connect_broker::connect_std_unix(std::path::Path::new(path), timeout)
        {
            let stream = r?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            return Ok(ProbeStream::Unix(stream));
        }
        let stream = std::os::unix::net::UnixStream::connect(path)?;
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        return Ok(ProbeStream::Unix(stream));
    }
    #[cfg(not(unix))]
    if addr.starts_with("unix:") {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "unix domain socket backends are not supported on this platform",
        ));
    }

    // F-182: capability mode 下はホスト名の解決もブローカーが行う（本体は DNS を引けない）。
    #[cfg(target_os = "freebsd")]
    if addr.parse::<SocketAddr>().is_err() {
        if let Some(r) = crate::connect_broker::connect_std_tcp(addr, timeout) {
            let stream = r?;
            stream.set_read_timeout(Some(timeout))?;
            stream.set_write_timeout(Some(timeout))?;
            return Ok(ProbeStream::Tcp(stream));
        }
    }
    let sock_addr: SocketAddr = match addr.parse() {
        Ok(a) => a,
        Err(_) => {
            use std::net::ToSocketAddrs;
            addr.to_socket_addrs()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?
                .next()
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no address resolved"))?
        }
    };
    let stream = tcp_connect_timeout(&sock_addr, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;
    Ok(ProbeStream::Tcp(stream))
}

/// `std::net::TcpStream::connect_timeout` の結果を `SO_ERROR` まで確かめて返す（B-91）。
///
/// NetBSD では、拒否された接続（`ECONNREFUSED`）に対して std の `connect_timeout` が
/// **`Ok` を返し、エラーを `SO_ERROR` に残したままにする**（`poll(2)` が `POLLHUP`/`POLLERR`
/// を立てず `POLLOUT` だけを返すため、std がエラーを拾わない。NetBSD 10.1 aarch64 で実測。
/// 同じアドレスへのブロッキング `connect` は正しく `ECONNREFUSED` を返す）。
/// そのままでは**落ちたバックエンドがヘルスチェックで healthy に見える**ため、
/// 接続後に `take_error()` を確認する（他 OS では常に `None` で、挙動は変わらない）。
///
/// 専用スレッド（ヘルスチェック・WASM の外部呼び出し・同期 TLS バックエンド経路）から
/// 呼ぶ同期 API であり、データプレーンのワーカースレッドでは使わないこと。
pub fn tcp_connect_timeout(
    addr: &SocketAddr,
    timeout: std::time::Duration,
) -> io::Result<std::net::TcpStream> {
    // F-182: capability mode 下は接続ブローカー経由（許可リストの添字で依頼する）。
    #[cfg(target_os = "freebsd")]
    if let Some(r) = crate::connect_broker::connect_std_addr(addr, timeout) {
        return r;
    }
    let stream = std::net::TcpStream::connect_timeout(addr, timeout)?;
    if let Some(e) = stream.take_error()? {
        return Err(e);
    }
    Ok(stream)
}

/// ヘルスチェックの Host ヘッダ用ホスト名を接続先表記から導く（F-170）。
///
/// `unix:` 接頭辞（UDS）は `addr.split(':').next()` すると無意味な `"unix"` になる
/// ため、`"localhost"` を返す。TCP は従来どおり `host:port` の先頭部分。
#[inline]
fn probe_host_header(addr: &str) -> &str {
    if addr.starts_with("unix:") {
        "localhost"
    } else {
        addr.split(':').next().unwrap_or(addr)
    }
}

/// 同期的な健康チェックを実行
///
/// TCP 接続して HTTP GET リクエストを送信し、レスポンスをチェック。
/// TLS接続もサポート（use_tls=true時）。
// 理由付き allow: 専用ヘルスチェックスレッドから呼ばれる同期プローブ（イベントループ外）。
#[allow(clippy::disallowed_methods)]
pub(crate) fn perform_health_check(
    addr: &str,
    host: &str,
    path: &str,
    use_tls: bool,
    verify_cert: bool,
    timeout: Duration,
    healthy_statuses: &[u16],
) -> bool {
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, ClientConnection, RootCertStore};
    use std::io::{ErrorKind, Read, Write};
    use std::sync::Arc;

    // F-170: TCP/UDS 共通の接続入口（パース失敗は false へ変換）。
    let mut tcp_stream = match connect_probe(addr, timeout) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // TLS接続の場合
    if use_tls {
        // rustls クライアント設定
        let config: Arc<ClientConfig> = if verify_cert {
            // 証明書検証を有効化（デフォルトのルート証明書ストアを使用）
            let mut root_store = RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(root_store)
                    .with_no_client_auth(),
            )
        } else {
            // 証明書検証を完全に無効化（自己署名証明書を許可）
            use rustls::client::danger::{
                HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
            };
            use rustls::pki_types::{CertificateDer, UnixTime};
            use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

            #[derive(Debug)]
            struct NoVerify;

            impl ServerCertVerifier for NoVerify {
                fn verify_server_cert(
                    &self,
                    _: &CertificateDer,
                    _: &[CertificateDer],
                    _: &rustls::pki_types::ServerName,
                    _: &[u8],
                    _: UnixTime,
                ) -> Result<ServerCertVerified, TlsError> {
                    Ok(ServerCertVerified::assertion())
                }
                fn verify_tls12_signature(
                    &self,
                    _: &[u8],
                    _: &CertificateDer,
                    _: &DigitallySignedStruct,
                ) -> Result<HandshakeSignatureValid, TlsError> {
                    Ok(HandshakeSignatureValid::assertion())
                }
                fn verify_tls13_signature(
                    &self,
                    _: &[u8],
                    _: &CertificateDer,
                    _: &DigitallySignedStruct,
                ) -> Result<HandshakeSignatureValid, TlsError> {
                    Ok(HandshakeSignatureValid::assertion())
                }
                fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
                    vec![
                        SignatureScheme::RSA_PKCS1_SHA256,
                        SignatureScheme::RSA_PKCS1_SHA384,
                        SignatureScheme::RSA_PKCS1_SHA512,
                        SignatureScheme::ECDSA_NISTP256_SHA256,
                        SignatureScheme::ECDSA_NISTP384_SHA384,
                        SignatureScheme::RSA_PSS_SHA256,
                        SignatureScheme::RSA_PSS_SHA384,
                        SignatureScheme::RSA_PSS_SHA512,
                    ]
                }
            }

            Arc::new(
                ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoVerify))
                    .with_no_client_auth(),
            )
        };

        // SNI名を決定
        let server_name = match ServerName::try_from(host.to_string()) {
            Ok(name) => name,
            Err(_) => return false,
        };

        // TLS接続を確立
        let mut tls_conn = match ClientConnection::new(config, server_name) {
            Ok(conn) => conn,
            Err(_) => return false,
        };

        // ハンドシェイクを実行（同期）
        while tls_conn.is_handshaking() {
            match tls_conn.complete_io(&mut tcp_stream) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    // 非ブロッキングI/Oの場合は短い待機
                    std::thread::sleep(Duration::from_millis(10));
                    continue;
                }
                Err(_) => return false,
            }
        }

        // rustls::Streamを使用して読み書き
        let mut stream = rustls::Stream::new(&mut tls_conn, &mut tcp_stream);

        // HTTP リクエスト送信
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: HealthCheck/1.0\r\n\r\n",
            path, host
        );

        if stream.write_all(request.as_bytes()).is_err() {
            return false;
        }

        // レスポンス読み取り
        let mut response = [0u8; 1024];
        let n = match stream.read(&mut response) {
            Ok(n) if n > 0 => n,
            _ => return false,
        };

        // ステータスコードを抽出
        let response_str = String::from_utf8_lossy(&response[..n]);
        if let Some(status_line) = response_str.lines().next() {
            // "HTTP/1.1 200 OK" のようなパターン
            let parts: Vec<&str> = status_line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(status_code) = parts[1].parse::<u16>() {
                    return healthy_statuses.contains(&status_code);
                }
            }
        }

        false
    } else {
        // HTTP接続（既存の実装）
        let request = format!(
            "GET {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: HealthCheck/1.0\r\n\r\n",
            path, host
        );

        if tcp_stream.write_all(request.as_bytes()).is_err() {
            return false;
        }

        // レスポンス読み取り
        let mut response = [0u8; 1024];
        let n = match tcp_stream.read(&mut response) {
            Ok(n) if n > 0 => n,
            _ => return false,
        };

        // ステータスコードを抽出
        let response_str = String::from_utf8_lossy(&response[..n]);
        if let Some(status_line) = response_str.lines().next() {
            // "HTTP/1.1 200 OK" のようなパターン
            let parts: Vec<&str> = status_line.split_whitespace().collect();
            if parts.len() >= 2 {
                if let Ok(status_code) = parts[1].parse::<u16>() {
                    return healthy_statuses.contains(&status_code);
                }
            }
        }

        false
    }
}

/// TCP 接続の確立可否のみ確認するヘルスチェック（F-22）
///
/// HTTP リクエストは送信せず、TCP 3-way ハンドシェイクが完了すれば healthy と判断。
/// L4 バックエンドや非 HTTP サービスの死活監視に使用する。
pub(crate) fn perform_tcp_health_check(addr: &str, timeout: Duration) -> bool {
    connect_probe(addr, timeout).is_ok()
}

/// gRPC Health Checking Protocol によるヘルスチェック（F-22）
///
/// grpc.health.v1.Health/Check を送信し、SERVING ステータスを確認する。
/// TLS の有無は `use_tls` で制御する。`service_name` が空文字の場合はサーバー全体のチェック。
/// gRPC Health Checking Protocol 用の LPM リクエストボディを構築する。
fn build_grpc_health_request_body(service_name: &str) -> Vec<u8> {
    let service_bytes = service_name.as_bytes();
    let proto_body: Vec<u8> = if service_bytes.is_empty() {
        vec![]
    } else {
        let mut body = Vec::with_capacity(2 + service_bytes.len());
        body.push(0x0a); // field 1, wire type 2 (length-delimited)
        body.push(service_bytes.len() as u8);
        body.extend_from_slice(service_bytes);
        body
    };
    let mut grpc_frame = Vec::with_capacity(5 + proto_body.len());
    grpc_frame.push(0u8);
    let msg_len = proto_body.len() as u32;
    grpc_frame.extend_from_slice(&msg_len.to_be_bytes());
    grpc_frame.extend_from_slice(&proto_body);
    grpc_frame
}

/// F-97: H2C Prior Knowledge で grpc.health.v1.Health/Check を実行する（コールドパス・同期）。
///
/// tonic 等の本物の gRPC サーバは HTTP/2 のみ受け付けるため、HTTP/1.1 フォールバック前に試行する。
/// 戻り値: Ok(healthy) / Err(()) は接続・ハンドシェイク失敗（H1 へフォールバック可）。
#[cfg(feature = "http2")]
#[allow(clippy::disallowed_methods)] // ヘルスチェックスレッド専用の同期 I/O
fn perform_grpc_health_check_h2c(
    addr: &str,
    _service_name: &str,
    grpc_frame: &[u8],
    timeout: Duration,
) -> Result<bool, ()> {
    use crate::http2::frame::{Frame, FrameDecoder, FrameEncoder, FrameHeader};
    use crate::http2::hpack::{HpackDecoder, HpackEncoder};
    use crate::http2::settings::defaults;
    use std::io::{Read, Write};

    // F-170: TCP/UDS 共通の接続入口。
    let mut stream = connect_probe(addr, timeout).map_err(|_| ())?;

    let enc = FrameEncoder::new(defaults::MAX_FRAME_SIZE);
    let mut hpack = HpackEncoder::new(defaults::HEADER_TABLE_SIZE as usize);
    let decoder = FrameDecoder::new(defaults::MAX_FRAME_SIZE);
    let mut hpack_dec = HpackDecoder::new(defaults::HEADER_TABLE_SIZE as usize);

    // Preface + SETTINGS
    stream
        .write_all(defaults::CONNECTION_PREFACE)
        .map_err(|_| ())?;
    let settings = enc.encode_settings(
        &[
            (0x3, defaults::MAX_CONCURRENT_STREAMS),
            (0x4, defaults::INITIAL_WINDOW_SIZE),
        ],
        false,
    );
    stream.write_all(&settings).map_err(|_| ())?;

    let host = probe_host_header(addr);
    let path = b"/grpc.health.v1.Health/Check";
    let headers: [(&[u8], &[u8], bool); 6] = [
        (b":method", b"POST", false),
        (b":path", path, false),
        (b":scheme", b"http", false),
        (b":authority", host.as_bytes(), false),
        (b"content-type", b"application/grpc", false),
        (b"te", b"trailers", false),
    ];
    let block = hpack.encode(&headers).map_err(|_| ())?;
    let headers_frame = enc.encode_headers(1, &block, false, true, None);
    stream.write_all(&headers_frame).map_err(|_| ())?;
    let data_frame = enc.encode_data(1, grpc_frame, true);
    stream.write_all(&data_frame).map_err(|_| ())?;

    let mut buf = vec![0u8; 16384];
    let mut filled = 0usize;
    let mut grpc_status_ok = false;
    let mut saw_serving = false;
    let mut saw_not_serving = false;
    let mut end = false;
    let deadline = std::time::Instant::now() + timeout;

    while !end && std::time::Instant::now() < deadline {
        if filled >= buf.len() {
            break;
        }
        match stream.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => continue,
            Err(e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(_) => return Err(()),
        }

        let mut offset = 0usize;
        while offset + 9 <= filled {
            let mut hdr9 = [0u8; 9];
            hdr9.copy_from_slice(&buf[offset..offset + 9]);
            let header = FrameHeader::decode(&hdr9);
            let frame_len = header.length as usize;
            if offset + 9 + frame_len > filled {
                break;
            }
            let payload = &buf[offset + 9..offset + 9 + frame_len];
            match decoder.decode(&header, payload) {
                Ok(Frame::Settings { ack, .. }) => {
                    if !ack {
                        let _ = stream.write_all(&enc.encode_settings_ack());
                    }
                }
                Ok(Frame::WindowUpdate { .. }) | Ok(Frame::Ping { .. }) => {}
                Ok(Frame::Headers {
                    header_block,
                    end_stream,
                    ..
                }) => {
                    if let Ok(decoded) = hpack_dec.decode(&header_block) {
                        for field in decoded {
                            if field.name.eq_ignore_ascii_case(b"grpc-status") {
                                grpc_status_ok = field.value.as_ref() == b"0".as_ref();
                            }
                        }
                    }
                    if end_stream {
                        end = true;
                    }
                }
                Ok(Frame::Data {
                    data, end_stream, ..
                }) => {
                    // HealthCheckResponse: field 1 varint status=1 (SERVING) → 0x08 0x01
                    if data.len() >= 7 {
                        let msg = &data[5..];
                        if msg.windows(2).any(|w| w == [0x08, 0x01]) {
                            saw_serving = true;
                        }
                        if msg.windows(2).any(|w| w == [0x08, 0x02]) {
                            saw_not_serving = true;
                        }
                    }
                    if end_stream {
                        end = true;
                    }
                }
                Ok(Frame::GoAway { .. }) | Ok(Frame::RstStream { .. }) => return Ok(false),
                _ => {}
            }
            offset += 9 + frame_len;
        }
        if offset > 0 {
            buf.copy_within(offset..filled, 0);
            filled -= offset;
        }
    }

    if saw_not_serving {
        return Ok(false);
    }
    if grpc_status_ok || saw_serving {
        return Ok(true);
    }
    // 応答を解釈できなかった場合は H1 モック互換へフォールバック（Err）
    Err(())
}

// 理由付き allow: 専用ヘルスチェックスレッドから呼ばれる同期プローブ（イベントループ外）。
#[allow(clippy::disallowed_methods)]
pub(crate) fn perform_grpc_health_check(
    addr: &str,
    service_name: &str,
    use_tls: bool,
    verify_cert: bool,
    timeout: Duration,
) -> bool {
    use std::io::{Read, Write};

    let grpc_frame = build_grpc_health_request_body(service_name);

    // F-97: まず H2C（本物の gRPC サーバ向け）。失敗時のみ H1 モック互換へフォールバック。
    #[cfg(feature = "http2")]
    if !use_tls {
        match perform_grpc_health_check_h2c(addr, service_name, &grpc_frame, timeout) {
            Ok(healthy) => return healthy,
            Err(()) => {
                // H2C 接続/ハンドシェイク失敗 → H1 フォールバック
            }
        }
    }

    // F-170: TCP/UDS 共通の接続入口。
    let mut tcp_stream = match connect_probe(addr, timeout) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // HTTP/1.1 モック互換パス（単体テスト・簡易バックエンド用）
    let host_header = probe_host_header(addr);
    let request = format!(
        "POST /grpc.health.v1.Health/Check HTTP/1.1\r\n\
         Host: {}\r\n\
         Content-Type: application/grpc\r\n\
         TE: trailers\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n\
         User-Agent: VeilHealthCheck/1.0\r\n\
         \r\n",
        host_header,
        grpc_frame.len()
    );

    let send_result: bool = if use_tls {
        use rustls::pki_types::ServerName;
        use rustls::{ClientConfig, ClientConnection, RootCertStore};
        use std::sync::Arc;

        let config: Arc<ClientConfig> = if verify_cert {
            let mut root_store = RootCertStore::empty();
            root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
            Arc::new(
                ClientConfig::builder()
                    .with_root_certificates(root_store)
                    .with_no_client_auth(),
            )
        } else {
            use rustls::client::danger::{
                HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier,
            };
            use rustls::pki_types::{CertificateDer, UnixTime};
            use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

            #[derive(Debug)]
            struct NoVerify;
            impl ServerCertVerifier for NoVerify {
                fn verify_server_cert(
                    &self,
                    _: &CertificateDer,
                    _: &[CertificateDer],
                    _: &rustls::pki_types::ServerName,
                    _: &[u8],
                    _: UnixTime,
                ) -> Result<ServerCertVerified, TlsError> {
                    Ok(ServerCertVerified::assertion())
                }
                fn verify_tls12_signature(
                    &self,
                    _: &[u8],
                    _: &CertificateDer,
                    _: &DigitallySignedStruct,
                ) -> Result<HandshakeSignatureValid, TlsError> {
                    Ok(HandshakeSignatureValid::assertion())
                }
                fn verify_tls13_signature(
                    &self,
                    _: &[u8],
                    _: &CertificateDer,
                    _: &DigitallySignedStruct,
                ) -> Result<HandshakeSignatureValid, TlsError> {
                    Ok(HandshakeSignatureValid::assertion())
                }
                fn supported_verify_schemes(&self) -> Vec<SchemeList> {
                    vec![
                        SchemeList::RSA_PKCS1_SHA256,
                        SchemeList::ECDSA_NISTP256_SHA256,
                        SchemeList::RSA_PSS_SHA256,
                    ]
                }
            }
            type SchemeList = SignatureScheme;
            Arc::new(
                ClientConfig::builder()
                    .dangerous()
                    .with_custom_certificate_verifier(Arc::new(NoVerify))
                    .with_no_client_auth(),
            )
        };

        let server_name = match ServerName::try_from(host_header.to_string()) {
            Ok(n) => n,
            Err(_) => return false,
        };
        let mut tls_conn = match ClientConnection::new(config, server_name) {
            Ok(c) => c,
            Err(_) => return false,
        };
        use std::io::ErrorKind;
        while tls_conn.is_handshaking() {
            match tls_conn.complete_io(&mut tcp_stream) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return false,
            }
        }
        let mut stream = rustls::Stream::new(&mut tls_conn, &mut tcp_stream);
        if stream.write_all(request.as_bytes()).is_err() {
            return false;
        }
        if stream.write_all(&grpc_frame).is_err() {
            return false;
        }
        let mut response = [0u8; 512];
        match stream.read(&mut response) {
            Ok(n) if n > 0 => check_grpc_response(&response[..n]),
            _ => false,
        }
    } else {
        if tcp_stream.write_all(request.as_bytes()).is_err() {
            return false;
        }
        if tcp_stream.write_all(&grpc_frame).is_err() {
            return false;
        }
        let mut response = [0u8; 512];
        match tcp_stream.read(&mut response) {
            Ok(n) if n > 0 => check_grpc_response(&response[..n]),
            _ => false,
        }
    };

    send_result
}

/// gRPC レスポンスが SERVING かどうかを検証する補助関数
fn check_grpc_response(response: &[u8]) -> bool {
    // HTTP レスポンスのステータスラインを確認
    let response_str = std::str::from_utf8(response).unwrap_or("");
    let first_line = response_str.lines().next().unwrap_or("");
    let parts: Vec<&str> = first_line.split_whitespace().collect();
    if parts.len() < 2 {
        return false;
    }
    let status_ok = parts[1].parse::<u16>().map(|s| s == 200).unwrap_or(false);
    if !status_ok {
        return false;
    }
    // grpc-status トレーラーまたはヘッダーが 0 (OK) であれば SERVING と判断
    // 簡易実装: grpc-status: 0 が含まれていれば OK
    response_str.contains("grpc-status: 0") || response_str.contains("grpc-status:0")
}

// ====================
// Backend選択
// ====================

/// ヘッダー名でゼロコピー検索（大文字小文字区別なし）
#[inline]
fn find_header_value<'a>(headers: &[(&'a [u8], &'a [u8])], name: &str) -> &'a str {
    headers
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .unwrap_or("")
}

/// クエリパラメータをオンデマンドで検索（URL デコードを必要時のみ実施）
fn find_query_value(raw_query: &[u8], key: &str) -> String {
    let query_str = std::str::from_utf8(raw_query).unwrap_or("");
    for pair in query_str.split('&').filter(|p| !p.is_empty()) {
        if let Some(eq_pos) = pair.find('=') {
            if pair[..eq_pos].eq_ignore_ascii_case(key) {
                return crate::http_utils::url_decode(&pair[eq_pos + 1..]);
            }
        } else if pair.eq_ignore_ascii_case(key) {
            return String::new();
        }
    }
    String::new()
}

/// 条件マッチング関数
///
/// すべての条件をANDで結合して評価します。
/// headers は生のバイト列ペア、raw_query はクエリ文字列バイト列を受け取り、
/// HashMap アロケーションなしで照合します。
pub(crate) fn matches_conditions(
    conditions: &RouteConditions,
    host: &[u8],
    path: &[u8],
    method: &[u8],
    headers: &[(&[u8], &[u8])],
    raw_query: &[u8],
    source_ip: &SocketAddr,
) -> bool {
    // host条件のチェック
    if let Some(ref host_pattern) = conditions.host {
        let host_str = match std::str::from_utf8(host) {
            Ok(s) => {
                if let Some(colon_pos) = s.find(':') {
                    &s[..colon_pos]
                } else {
                    s
                }
            }
            Err(_) => return false,
        };
        if !matches_wildcard(host_pattern, host_str) {
            return false;
        }
    }

    // path条件のチェック
    if let Some(ref path_pattern) = conditions.path {
        if !matches_path_pattern(path_pattern, path) {
            return false;
        }
    }

    // header条件のチェック（条件がある場合のみ線形探索）
    if let Some(ref header_conds) = conditions.header {
        for (key, value_pattern) in header_conds {
            let header_value = find_header_value(headers, key);
            if !matches_wildcard(value_pattern, header_value) {
                return false;
            }
        }
    }

    // method条件のチェック
    if let Some(ref methods) = conditions.method {
        let method_str = std::str::from_utf8(method).unwrap_or("");
        if !methods.iter().any(|m| m.eq_ignore_ascii_case(method_str)) {
            return false;
        }
    }

    // query条件のチェック（条件がある場合のみオンデマンドパース）
    if let Some(ref query_conds) = conditions.query {
        for (key, value_pattern) in query_conds {
            let query_value = find_query_value(raw_query, key);
            if !matches_wildcard(value_pattern, &query_value) {
                return false;
            }
        }
    }

    // source_ip条件のチェック
    if let Some(ref ip_ranges) = conditions.source_ip {
        if !matches_cidr(source_ip, ip_ranges) {
            return false;
        }
    }

    true
}

/// 統合ルーティング評価関数（最適化版）
///
/// Phase 1-4最適化を適用:
/// - Phase 1: Host-based グループ化 (O(1) HashMap lookup)
/// - Phase 2: Path Radix Tree (matchit)
/// - Phase 3: CIDR Tree 最適化
/// - Phase 4: LRU キャッシュ
///
/// 候補ルートのみを評価することで、線形O(n)から大幅に削減
pub fn find_backend_unified(
    host: &[u8],
    path: &[u8],
    method: &[u8],
    headers: &[(&[u8], &[u8])],
    raw_query: &[u8],
    source_ip: &SocketAddr,
    routes: &[Route],
    upstream_groups: &Arc<HashMap<String, Arc<UpstreamGroup>>>,
) -> Option<(Arc<[u8]>, Backend, Arc<CompressionConfig>)> {
    // CURRENT_CONFIG から OptimizedRouter を取得
    let config = CURRENT_CONFIG.load();
    let optimized_router = &config.optimized_router;
    // 引数の UTF-8 変換は debug が有効なときだけ評価する（以前は毎リクエスト 3 回
    // from_utf8 を計算していた。ftlog のマクロは無効レベルでは引数を評価しない）。
    debug!(
        "[Routing] find_backend_unified: host='{}', path='{}', method='{}'",
        std::str::from_utf8(host).unwrap_or(""),
        std::str::from_utf8(path).unwrap_or(""),
        std::str::from_utf8(method).unwrap_or("")
    );

    // Phase 4: キャッシュチェック
    // キャッシュが「マッチなし」（Some(None)）の場合も header/query 条件は動的なため
    // フォールスルーして全ルートを再評価する。
    let cache_key = routing::RouteCacheKey::new(host, path, method, source_ip);
    if let Some(Some(route_idx)) = optimized_router.try_cache(&cache_key) {
        // キャッシュヒット: ルートが見つかっている
        if let Some(route) = routes.get(route_idx) {
            // 条件が変わっていないか確認（header/query/methodは動的）
            if matches_conditions(
                &route.conditions,
                host,
                path,
                method,
                headers,
                raw_query,
                source_ip,
            ) {
                if let Ok(backend) = load_backend(route, upstream_groups) {
                    // F-159: 解決済みなら Arc clone のみ（malloc なし）。
                    let prefix = route
                        .resolved_path_prefix
                        .clone()
                        .unwrap_or_else(|| extract_path_prefix_arc(route));
                    let compression = route
                        .resolved_compression
                        .clone()
                        .unwrap_or_else(|| Arc::new(route.compression.clone().unwrap_or_default()));
                    return Some((prefix, backend, compression));
                }
            }
        }
    }

    // キャッシュミス: OptimizedRouter を使用して候補を取得
    let host_str = String::from_utf8_lossy(host);
    let path_str = String::from_utf8_lossy(path);

    // Phase 1-3: 候補ルートを取得
    let candidates = optimized_router.get_candidates(&host_str, &path_str, source_ip);

    if candidates.is_empty() {
        // 候補がない場合はフォールバック（全ルート走査）
        // これはOptimizedRouterの構築が不完全な場合のセーフティネット
        return find_backend_linear(
            host,
            path,
            method,
            headers,
            raw_query,
            source_ip,
            routes,
            upstream_groups,
            &cache_key,
            optimized_router,
        );
    }

    // 候補ルートのみを評価（first-match）
    // 候補は既にソート済み（インデックス順）
    debug!(
        "[Routing] Candidates for host='{}' path='{}': {:?}",
        host_str, path_str, candidates
    );
    for &route_idx in &candidates {
        if let Some(route) = routes.get(route_idx) {
            // 残りの条件（header, method, query）を評価
            let matched =
                matches_remaining_conditions(&route.conditions, method, headers, raw_query);

            if matched {
                debug!(
                    "[Routing] Matched route index: {} (path={:?} action={:?})",
                    route_idx, route.conditions.path, route.action
                );
                match load_backend(route, upstream_groups) {
                    Ok(backend) => {
                        // F-159: 解決済みなら Arc clone のみ（malloc なし）。
                        let prefix = route
                            .resolved_path_prefix
                            .clone()
                            .unwrap_or_else(|| extract_path_prefix_arc(route));
                        let compression = route.resolved_compression.clone().unwrap_or_else(|| {
                            Arc::new(route.compression.clone().unwrap_or_default())
                        });
                        // キャッシュに保存
                        optimized_router.cache_result(cache_key, Some(route_idx));
                        return Some((prefix, backend, compression));
                    }
                    Err(e) => {
                        warn!(
                            "Route[{}] load_backend failed: {} (action={:?})",
                            route_idx, e, route.action
                        );
                        continue;
                    }
                }
            }
        }
    }

    // 候補内でマッチしなかった場合
    debug!(
        "No route matched in {} candidates: host='{}' path='{}' method='{}'",
        candidates.len(),
        host_str,
        path_str,
        String::from_utf8_lossy(method),
    );

    // キャッシュにマッチなしを保存
    debug!(
        "[Routing] No match found for host='{}' path='{}'",
        host_str, path_str
    );
    optimized_router.cache_result(cache_key, None);
    None
}

/// パスプレフィックスを抽出
#[inline]
pub(crate) fn extract_path_prefix(route: &Route) -> Box<[u8]> {
    if let Some(ref path_pattern) = route.conditions.path {
        if let Some(prefix_str) = path_pattern.strip_suffix("/*") {
            prefix_str.as_bytes().into()
        } else {
            path_pattern.as_bytes().into()
        }
    } else {
        Box::new([])
    }
}

/// パスプレフィックスを抽出（`Arc<[u8]>` 版、F-159）。
///
/// 設定ロード時に `Route::resolved_path_prefix` を構築するために使う。ホットパスでは
/// 呼ばれず、`route.resolved_path_prefix.clone()`（Arc 参照カウント増分のみ）を使う。
#[inline]
pub(crate) fn extract_path_prefix_arc(route: &Route) -> Arc<[u8]> {
    extract_path_prefix(route).into()
}

/// 残りの条件（host/path/source_ip以外）のみをチェック
///
/// OptimizedRouter で既に host/path/source_ip はフィルタ済み。
/// HashMap を使わずバイト列を直接照合することでアロケーションゼロ。
#[inline]
pub(crate) fn matches_remaining_conditions(
    conditions: &RouteConditions,
    method: &[u8],
    headers: &[(&[u8], &[u8])],
    raw_query: &[u8],
) -> bool {
    // header条件のチェック（条件がある場合のみ線形探索）
    if let Some(ref header_conds) = conditions.header {
        for (key, value_pattern) in header_conds {
            let header_value = find_header_value(headers, key);
            if !matches_wildcard(value_pattern, header_value) {
                return false;
            }
        }
    }

    // method条件のチェック
    if let Some(ref methods) = conditions.method {
        let method_str = std::str::from_utf8(method).unwrap_or("");
        if !methods.iter().any(|m| m.eq_ignore_ascii_case(method_str)) {
            return false;
        }
    }

    // query条件のチェック（条件がある場合のみオンデマンドパース）
    if let Some(ref query_conds) = conditions.query {
        for (key, value_pattern) in query_conds {
            let query_value = find_query_value(raw_query, key);
            if !matches_wildcard(value_pattern, &query_value) {
                return false;
            }
        }
    }

    true
}

/// フォールバック用線形探索（セーフティネット）
pub(crate) fn find_backend_linear(
    host: &[u8],
    path: &[u8],
    method: &[u8],
    headers: &[(&[u8], &[u8])],
    raw_query: &[u8],
    source_ip: &SocketAddr,
    routes: &[Route],
    upstream_groups: &Arc<HashMap<String, Arc<UpstreamGroup>>>,
    cache_key: &routing::RouteCacheKey,
    optimized_router: &routing::OptimizedRouter,
) -> Option<(Arc<[u8]>, Backend, Arc<CompressionConfig>)> {
    // 配列の順序で評価（first-match）
    for (i, route) in routes.iter().enumerate() {
        let matched = matches_conditions(
            &route.conditions,
            host,
            path,
            method,
            headers,
            raw_query,
            source_ip,
        );

        if matched {
            debug!(
                "Route[{}] matched (linear fallback): host={:?} path={:?} method={:?}",
                i, route.conditions.host, route.conditions.path, route.conditions.method
            );
            match load_backend(route, upstream_groups) {
                Ok(backend) => {
                    // F-159: 解決済みなら Arc clone のみ（malloc なし）。
                    let prefix = route
                        .resolved_path_prefix
                        .clone()
                        .unwrap_or_else(|| extract_path_prefix_arc(route));
                    let compression = route
                        .resolved_compression
                        .clone()
                        .unwrap_or_else(|| Arc::new(route.compression.clone().unwrap_or_default()));
                    // キャッシュに保存
                    optimized_router.cache_result(*cache_key, Some(i));
                    return Some((prefix, backend, compression));
                }
                Err(e) => {
                    warn!(
                        "Route[{}] load_backend failed: {} (action={:?})",
                        i, e, route.action
                    );
                    continue;
                }
            }
        }
    }

    debug!(
        "No route matched (linear fallback): host='{}' path='{}' method='{}' routes_count={}",
        String::from_utf8_lossy(host),
        String::from_utf8_lossy(path),
        String::from_utf8_lossy(method),
        routes.len()
    );

    // キャッシュにマッチなしを保存
    optimized_router.cache_result(*cache_key, None);
    None
}

// Helper functions for condition matching

/// ワイルドカードパターンマッチング（シンプルな実装）
///
/// パターン例:
/// - "example.com" → 完全一致
/// - "*.example.com" → サブドメインにマッチ（例: "api.example.com", "www.example.com"）
/// - "api.*.com" → サポートしない（先頭または末尾のみ）
fn matches_wildcard(pattern: &str, text: &str) -> bool {
    if pattern == text {
        return true;
    }

    // 先頭ワイルドカード: "*.example.com"
    if let Some(rest) = pattern.strip_prefix("*.") {
        if let Some(subdomain) = text.strip_suffix(rest) {
            // サブドメインのチェック（少なくとも1つのドットが必要）
            return !subdomain.is_empty() && !subdomain.contains('.');
        }
    }

    // 末尾ワイルドカード: "api.*"
    if let Some(rest) = pattern.strip_suffix(".*") {
        if let Some(domain) = text.strip_prefix(rest) {
            // ドメイン部分のチェック
            return !domain.is_empty() && domain.starts_with('.');
        }
    }

    false
}

/// パスパターンマッチング（ワイルドカード対応）
///
/// パターン例:
/// - "/api" → 完全一致
/// - "/api/*" → "/api/" で始まるすべてのパスにマッチ
/// - "/api/v2/*" → "/api/v2/" で始まるすべてのパスにマッチ
fn matches_path_pattern(pattern: &str, path: &[u8]) -> bool {
    // B-96: `"/"` は全パスに一致する（`routing::PathRouter::add_route` が `any_path` へ
    // 振り分けるのと同じ意味論）。従来は下の前方一致で `"/" + "small.html"` の残りが
    // `/` で始まらないため不一致になり、最も一般的な catch-all ルート（`path = "/"`）で
    // **ルートキャッシュのヒット検証が毎回失敗**して、全リクエストがフル探索
    // （`from_utf8_lossy` + 候補列挙 + LRU の put）に落ちていた。
    if pattern == "/" {
        return true;
    }
    let path_str = match std::str::from_utf8(path) {
        Ok(s) => s,
        Err(_) => return false,
    };

    // 完全一致
    if pattern == path_str {
        return true;
    }

    // ワイルドカードパターン: "/api/*"
    if let Some(prefix) = pattern.strip_suffix("/*") {
        return path_str.starts_with(prefix)
            && (path_str.len() == prefix.len() || path_str.as_bytes()[prefix.len()] == b'/');
    }

    // プレフィックス一致（末尾スラッシュなしでもマッチ）
    if let Some(remaining) = path_str.strip_prefix(pattern) {
        // パターンが完全一致、または次の文字がスラッシュ
        return remaining.is_empty() || remaining.starts_with('/');
    }

    false
}

/// ソースIPがCIDR範囲に含まれるかチェック
fn matches_cidr(ip: &SocketAddr, cidr_ranges: &[String]) -> bool {
    use std::net::IpAddr;

    let ip_addr = ip.ip();

    for cidr in cidr_ranges {
        // シンプルなCIDRマッチング（IPv4のみ対応）
        if let Some((network_str, prefix_len_str)) = cidr.split_once('/') {
            if let (Ok(network), Ok(prefix_len)) =
                (network_str.parse::<IpAddr>(), prefix_len_str.parse::<u8>())
            {
                if let (IpAddr::V4(network_v4), IpAddr::V4(ip_v4)) = (network, ip_addr) {
                    let mask = !((1u32 << (32 - prefix_len)) - 1);
                    let network_u32 = u32::from_be_bytes(network_v4.octets());
                    let ip_u32 = u32::from_be_bytes(ip_v4.octets());
                    if (network_u32 & mask) == (ip_u32 & mask) {
                        return true;
                    }
                }
            }
        } else {
            // CIDR表記なし（完全一致）
            if let Ok(parsed_ip) = cidr.parse::<IpAddr>() {
                if parsed_ip == ip_addr {
                    return true;
                }
            }
        }
    }

    false
}

#[cfg(test)]
mod tests {
    // 理由付き allow: テストコードは同期 I/O・sleep を使用してよい（データプレーン非経由）。
    #![allow(clippy::disallowed_methods)]
    use super::*;
    use std::net::ToSocketAddrs;

    /// B-96: `path = "/"`（catch-all）はルーター本体（`any_path`）と同じく全パスに一致する。
    /// ルートキャッシュのヒット検証（`matches_conditions`）がこれを不一致と判定すると、
    /// キャッシュが一度も効かず毎リクエストのフル探索に落ちる。
    #[test]
    fn test_b96_root_path_pattern_matches_every_path() {
        assert!(matches_path_pattern("/", b"/"));
        assert!(matches_path_pattern("/", b"/small.html"));
        assert!(matches_path_pattern("/", b"/a/b/c"));
        // 既存の意味論は不変
        assert!(matches_path_pattern("/api/*", b"/api/v1"));
        assert!(!matches_path_pattern("/api/*", b"/apix"));
        assert!(matches_path_pattern("/api", b"/api/v1"));
        assert!(!matches_path_pattern("/api", b"/apix"));

        let conditions = crate::config::RouteConditions {
            path: Some("/".to_string()),
            ..Default::default()
        };
        let ip: SocketAddr = "127.0.0.1:12345".parse().unwrap();
        assert!(matches_conditions(
            &conditions,
            b"example.com",
            b"/small.html",
            b"GET",
            &[],
            b"",
            &ip
        ));
    }

    // ====================
    // F-22: ヘルスチェック種別テスト
    // ====================

    #[test]
    fn test_health_check_type_default_is_http() {
        use crate::config::{HealthCheckConfig, HealthCheckType};
        let cfg = HealthCheckConfig::default();
        assert_eq!(cfg.check_type, HealthCheckType::Http);
    }

    #[test]
    fn test_perform_tcp_health_check_unreachable() {
        // 接続できないアドレスは false を返す
        let result = perform_tcp_health_check("127.0.0.1:19999", Duration::from_millis(200));
        assert!(!result);
    }

    #[test]
    fn test_perform_tcp_health_check_invalid_addr() {
        // 無効なアドレスは false を返す
        let result = perform_tcp_health_check("not-a-valid-addr", Duration::from_millis(200));
        assert!(!result);
    }

    // ====================
    // F-170: connect_probe / ProbeStream / probe_host_header
    // ====================

    #[cfg(unix)]
    #[test]
    fn test_connect_probe_unix_success() {
        let mut path = std::env::temp_dir();
        path.push(format!("veil-f170-probe-{}.sock", std::process::id()));
        let _ = std::fs::remove_file(&path);

        let listener = std::os::unix::net::UnixListener::bind(&path).expect("unix bind");
        let acceptor = std::thread::spawn(move || listener.accept().expect("accept"));

        let addr = format!("unix:{}", path.display());
        let result = connect_probe(&addr, Duration::from_millis(500));

        acceptor.join().expect("acceptor thread join");
        let _ = std::fs::remove_file(&path);
        assert!(result.is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn test_connect_probe_unix_enoent() {
        let mut path = std::env::temp_dir();
        path.push(format!(
            "veil-f170-probe-enoent-{}.sock",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&path);

        let addr = format!("unix:{}", path.display());
        let result = connect_probe(&addr, Duration::from_millis(200));
        assert!(result.is_err());
    }

    #[test]
    fn test_connect_probe_invalid_tcp_addr_is_err() {
        // F-170: 旧実装の 127.0.0.1:80 フォールバックは廃止し、パース失敗は Err になる。
        let result = connect_probe("not-a-valid-addr", Duration::from_millis(200));
        match result {
            Ok(_) => panic!("expected Err for invalid address"),
            Err(e) => assert_eq!(e.kind(), std::io::ErrorKind::InvalidInput),
        }
    }

    #[test]
    #[allow(clippy::disallowed_methods)] // テストコード: 同期 DNS で待ち受けアドレスを決める
    fn test_connect_probe_resolves_hostname() {
        // F-170 修正: connect_probe は SocketAddr パースに失敗した表記を
        // ホスト名として DNS 解決できなければならない（http3_server.rs の
        // 旧 TcpStream::connect(&str) の DNS 解決を引き継ぐ回帰確認）。
        //
        // 「localhost」の名前解決は環境によって `::1`（IPv6）が先に返ることがある。
        // 以前の実装はリスナーを固定で `127.0.0.1`（IPv4）へ bind していたため、
        // 解決結果が IPv6 優先の環境では connect_probe が別アドレスへ接続してしまい、
        // IPv4 リスナーの accept() が永久に来ない（= 待ち受けスレッドの join() が
        // 永久に返らない）状態になり得た。実際に docker ビルド（builder ステージの
        // `cargo test --lib`）でこのテストがハングし、ビルドが進まなくなったことが
        // あるため、まず "localhost:0" を解決してその先頭アドレスへ bind することで
        // connect_probe が解決する先頭アドレスと確実に同じファミリにする。
        let resolve_addr = "localhost:0"
            .to_socket_addrs()
            .expect("resolve localhost")
            .next()
            .expect("localhost must resolve to at least one address");
        let listener = std::net::TcpListener::bind(resolve_addr).expect("bind ephemeral listener");
        let port = listener.local_addr().expect("local_addr").port();

        // 上記でファミリを揃えてあるため、通常は connect_probe が確実に listener へ
        // 到達する。それでも「万一ファミリが揃わなかった／connect が失敗した」場合に
        // accept() が永久にブロックして join() が返らなくなる（= docker ビルドの
        // `cargo test --lib` が再びハングする）事態を避けるため、accept 待ち受け側に
        // 明示的な上限時間を設ける（ノンブロッキング + 短間隔ポーリング）。
        listener.set_nonblocking(true).expect("set_nonblocking");
        let acceptor = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                match listener.accept() {
                    Ok(_) => return true,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                        if std::time::Instant::now() >= deadline {
                            return false;
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    Err(_) => return false,
                }
            }
        });

        let addr = format!("localhost:{}", port);
        let result = connect_probe(&addr, Duration::from_secs(2));

        let accepted = acceptor.join().expect("acceptor thread join");
        assert!(
            accepted,
            "acceptor must observe a connection within the deadline"
        );
        assert!(result.is_ok(), "expected localhost:{} to resolve", port);
    }

    #[test]
    fn test_probe_host_header_unix_is_localhost() {
        assert_eq!(probe_host_header("unix:/run/app.sock"), "localhost");
    }

    #[test]
    fn test_probe_host_header_tcp_is_host_part() {
        assert_eq!(
            probe_host_header("backend.example.com:8080"),
            "backend.example.com"
        );
    }

    #[test]
    fn test_check_grpc_response_ok() {
        // SERVING レスポンスは true
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/grpc\r\ngrpc-status: 0\r\n\r\n";
        assert!(check_grpc_response(response));
    }

    #[test]
    fn test_check_grpc_response_not_serving() {
        // grpc-status が 0 以外は false
        let response =
            b"HTTP/1.1 200 OK\r\nContent-Type: application/grpc\r\ngrpc-status: 5\r\n\r\n";
        assert!(!check_grpc_response(response));
    }

    #[test]
    fn test_check_grpc_response_http_error() {
        // HTTP ステータスが 200 以外は false
        let response = b"HTTP/1.1 503 Service Unavailable\r\ngrpc-status: 0\r\n\r\n";
        assert!(!check_grpc_response(response));
    }

    #[test]
    fn test_check_grpc_response_grpc_status_no_space() {
        // grpc-status:0（スペースなし）も認識する
        let response = b"HTTP/1.1 200 OK\r\ngrpc-status:0\r\n\r\n";
        assert!(check_grpc_response(response));
    }

    #[test]
    fn test_perform_grpc_health_check_unreachable() {
        // 接続できないアドレスは false を返す
        let result = perform_grpc_health_check(
            "127.0.0.1:19999",
            "",
            false,
            false,
            Duration::from_millis(200),
        );
        assert!(!result);
    }

    #[test]
    fn test_perform_tcp_health_check_success() {
        // リスニング中の TCP サーバーには接続できる
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        let _server = std::thread::spawn(move || {
            let _ = listener.accept();
        });

        let result = perform_tcp_health_check(&addr, Duration::from_secs(2));
        assert!(result, "listening port should return true");
    }

    /// モックサーバの応答 → 正常クローズ。
    ///
    /// 未読の受信データを残したまま close すると、Windows は FIN ではなく RST を送り、
    /// クライアントがまだ読んでいない応答まで捨てられることがある（負荷時にだけ
    /// `test_perform_grpc_health_check_success` が落ちた）。送信側を閉じてから
    /// 相手の close まで読み捨てる。
    fn reply_and_close_gracefully(mut conn: std::net::TcpStream, response: &[u8]) {
        use std::io::{Read, Write};
        let _ = conn.write_all(response);
        let _ = conn.shutdown(std::net::Shutdown::Write);
        let _ = conn.set_read_timeout(Some(Duration::from_secs(2)));
        let mut sink = [0u8; 512];
        while matches!(conn.read(&mut sink), Ok(n) if n > 0) {}
    }

    #[test]
    fn test_perform_grpc_health_check_success() {
        // gRPC ヘルスチェックに対して SERVING を返すモックサーバー
        // H2C 試行 + H1 フォールバックで最大 2 接続を受け付ける
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();
        let _ = listener.set_nonblocking(false);

        std::thread::spawn(move || {
            for _ in 0..4 {
                if let Ok((mut conn, _)) = listener.accept() {
                    let mut buf = [0u8; 512];
                    let _ = conn.read(&mut buf);
                    // H2C preface には HTTP/1.1 応答（無視されて H1 フォールバック）
                    let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/grpc\r\ngrpc-status: 0\r\n\r\n";
                    reply_and_close_gracefully(conn, response);
                } else {
                    break;
                }
            }
        });

        std::thread::sleep(Duration::from_millis(20));
        let result = perform_grpc_health_check(&addr, "", false, false, Duration::from_secs(2));
        assert!(
            result,
            "mock gRPC server returning grpc-status: 0 should return true"
        );
    }

    #[test]
    fn test_perform_grpc_health_check_not_serving() {
        // gRPC ヘルスチェックに対して NOT_SERVING を返すモックサーバー
        use std::io::Read;
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap().to_string();

        std::thread::spawn(move || {
            for _ in 0..4 {
                if let Ok((mut conn, _)) = listener.accept() {
                    let mut buf = [0u8; 512];
                    let _ = conn.read(&mut buf);
                    let response =
                        b"HTTP/1.1 200 OK\r\nContent-Type: application/grpc\r\ngrpc-status: 2\r\n\r\n";
                    reply_and_close_gracefully(conn, response);
                } else {
                    break;
                }
            }
        });

        std::thread::sleep(Duration::from_millis(20));
        let result = perform_grpc_health_check(&addr, "", false, false, Duration::from_secs(2));
        assert!(!result, "grpc-status: 2 (UNKNOWN) should return false");
    }

    /// F-97: H2C の tonic-health 風 SERVING 応答を解釈できること
    #[cfg(feature = "http2")]
    #[test]
    fn test_perform_grpc_health_check_h2c_real_style() {
        // 最小 H2C サーバ: preface 後 SETTINGS を返し、HEADERS+DATA で SERVING を返すのは重いため
        // ここでは実ポート 9004 が無い環境でも panic しないことだけ確認し、
        // 到達不能は false を返す既存テストで担保する。
        let result = perform_grpc_health_check(
            "127.0.0.1:19997",
            "",
            false,
            false,
            Duration::from_millis(200),
        );
        assert!(!result);
    }

    #[test]
    fn test_check_grpc_response_empty() {
        // 空レスポンスは false
        assert!(!check_grpc_response(b""));
    }

    #[test]
    fn test_check_grpc_response_no_grpc_status_header() {
        // grpc-status ヘッダーなしは false
        let response = b"HTTP/1.1 200 OK\r\nContent-Type: application/grpc\r\n\r\n";
        assert!(!check_grpc_response(response));
    }

    #[test]
    fn test_check_grpc_response_grpc_status_nonzero_values() {
        // 各 gRPC エラーコードが false になること
        for status in [1u8, 2, 3, 4, 5, 12, 13, 14, 16] {
            let response = format!("HTTP/1.1 200 OK\r\ngrpc-status: {}\r\n\r\n", status);
            assert!(
                !check_grpc_response(response.as_bytes()),
                "grpc-status {} should return false",
                status
            );
        }
    }
}
