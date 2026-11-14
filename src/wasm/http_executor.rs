//! WASM HTTP Call Executor
//!
//! Provides synchronous HTTP client for executing WASM proxy_http_call requests
//! from the tick thread.
//!
//! F-132: このモジュールにはボディフィルタ（`on_request_body` / `on_response_body`）
//! を h1/h2/h3 の各プロトコル経路から共用するためのヘルパも置く（現状 HTTP/3 のみ配線）。

use std::io::{BufReader, Read, Write};
use std::net::TcpStream;
use std::sync::Arc;
use std::time::Duration;

use super::engine::{BodyFilterResult, FilterEngine};
use super::persistent_context::GlobalPendingCall;
use super::types::{HttpCallResponse, LocalResponse};

/// ボディフィルタ適用結果（h1/h2/h3 共有）。
///
/// 守れない Pause はエンジンが失敗として `failure_policy` で扱う（B-300・F-293）ので、ここには来ない。
pub enum WasmBodyOutcome {
    /// フィルタ後の本文で継続する。
    Continue(bytes::Bytes),
    /// WASM モジュールがローカル応答を要求した。
    LocalResponse(LocalResponse),
}

/// リクエストボディに WASM `on_request_body` を適用する。
///
/// `modules` が空なら一切コストをかけずそのまま返す
/// （ホットパス絶対規則: WASM 未設定時は一切コストを増やさない）。
/// `end_of_stream` は呼び出し元がボディ全体を保持しているかどうかを示す
/// （HTTP/3 の Proxy 経路は WASM 適用時に必ず `Decision::Buffer` に落ちるため常に true）。
///
/// F-271: HTTP/1.1・HTTP/2・HTTP/3 の 3 経路がこの関数を共有する。`request` を渡すと、本文のコールバックから
/// 要求のメソッド・パス（プロパティ `request.path`）・ヘッダが見える。
pub async fn apply_wasm_request_body(
    engine: &FilterEngine,
    modules: &crate::wasm_plugin_config::WasmModules,
    body: bytes::Bytes,
    end_of_stream: bool,
    request: Option<&super::RequestMeta<'_>>,
) -> WasmBodyOutcome {
    if modules.is_empty() {
        return WasmBodyOutcome::Continue(body);
    }
    // 呼び出し元の future に本体（数 KB）を埋め込まない（Wasm を使わない要求の move が重くなる。B-240）
    match Box::pin(engine.on_request_body_with_modules(
        modules,
        body,
        end_of_stream,
        request,
        modules.sessions(),
    ))
    .await
    {
        BodyFilterResult::Continue { body } => WasmBodyOutcome::Continue(body),
        BodyFilterResult::LocalResponse(resp) => WasmBodyOutcome::LocalResponse(resp),
    }
}

/// レスポンスボディに WASM `on_response_body` を適用する。
///
/// `modules` が空なら一切コストをかけずそのまま返す。
pub async fn apply_wasm_response_body(
    engine: &FilterEngine,
    modules: &crate::wasm_plugin_config::WasmModules,
    body: bytes::Bytes,
    end_of_stream: bool,
) -> WasmBodyOutcome {
    if modules.is_empty() {
        return WasmBodyOutcome::Continue(body);
    }
    // 呼び出し元の future に本体（数 KB）を埋め込まない（B-240）
    match Box::pin(engine.on_response_body_with_modules(
        modules,
        body,
        end_of_stream,
        modules.sessions(),
    ))
    .await
    {
        BodyFilterResult::Continue { body } => WasmBodyOutcome::Continue(body),
        BodyFilterResult::LocalResponse(resp) => WasmBodyOutcome::LocalResponse(resp),
    }
}

/// レスポンスヘッダの `content-length` を新しい本文長へ更新する。
///
/// B-46: WASM がレスポンス本文を書き換えた場合、古い `content-length` を残したまま
/// 送出すると本文長と不一致になり、nghttp3 が malformed message として
/// `H3_MESSAGE_ERROR` で拒否する（既存ヘッダ重複追加も同様に拒否される）。
/// 既存の `content-length`（大小文字問わず）をすべて除去してから新しい値を 1 つ追加する。
pub fn set_content_length_header(headers: &mut Vec<(Vec<u8>, Vec<u8>)>, len: usize) {
    headers.retain(|(name, _)| !name.eq_ignore_ascii_case(b"content-length"));
    headers.push((b"content-length".to_vec(), len.to_string().into_bytes()));
}

/// `[wasm] http_call_max_timeout_ms`（F-282）
static MAX_TIMEOUT_MS: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(crate::wasm::DEFAULT_HTTP_CALL_MAX_TIMEOUT_MS);

/// `[wasm] http_call_max_timeout_ms` を反映する（起動時・リロード時）
pub fn set_max_timeout_ms(ms: u32) {
    MAX_TIMEOUT_MS.store(ms.max(1), std::sync::atomic::Ordering::Relaxed);
}

/// ゲストの `timeout_ms` を上限で打ち切る（ホスト関数で負の値は 0 にしてある。0 は 1ms）
fn clamp_timeout(timeout_ms: u32) -> Duration {
    let cap = MAX_TIMEOUT_MS
        .load(std::sync::atomic::Ordering::Relaxed)
        .max(1);
    Duration::from_millis(timeout_ms.clamp(1, cap) as u64)
}

/// Execute a pending HTTP call and return the response
///
/// This function makes a synchronous HTTP/1.1 request to the upstream.
/// For HTTPS upstreams, it uses rustls with blocking I/O.
pub fn execute_http_call(
    pending: &GlobalPendingCall,
    upstream_host: &str,
    upstream_port: u16,
    use_tls: bool,
) -> Result<HttpCallResponse, String> {
    let timeout = clamp_timeout(pending.call.timeout_ms);

    // Connect to upstream
    let addr = format!("{}:{}", upstream_host, upstream_port);
    let stream = crate::upstream::tcp_connect_timeout(
        &addr
            .parse()
            .map_err(|e| format!("Invalid address: {}", e))?,
        timeout,
    )
    .map_err(|e| format!("Connection failed: {}", e))?;

    stream.set_read_timeout(Some(timeout)).ok();
    stream.set_write_timeout(Some(timeout)).ok();

    if use_tls {
        execute_https_request(stream, upstream_host, pending)
    } else {
        execute_http_request(stream, upstream_host, pending)
    }
}

/// Execute HTTP/1.1 request without TLS
fn execute_http_request(
    mut stream: TcpStream,
    host: &str,
    pending: &GlobalPendingCall,
) -> Result<HttpCallResponse, String> {
    // Build request
    let request = build_http_request(host, pending)?;

    // Send request
    stream
        .write_all(&request)
        .map_err(|e| format!("Write failed: {}", e))?;

    // Read response
    parse_http_response(stream)
}

/// Execute HTTPS request with TLS
fn execute_https_request(
    stream: TcpStream,
    host: &str,
    pending: &GlobalPendingCall,
) -> Result<HttpCallResponse, String> {
    use rustls::pki_types::ServerName;
    use rustls::ClientConfig;

    // Trust anchors from `[tls_client] ca` (built-in roots by default)
    let config = ClientConfig::builder()
        .with_root_certificates(crate::trust_store::default_roots())
        .with_no_client_auth();

    let server_name = ServerName::try_from(host.to_string())
        .map_err(|_| format!("Invalid server name: {}", host))?;

    let mut conn = rustls::ClientConnection::new(Arc::new(config), server_name)
        .map_err(|e| format!("TLS handshake failed: {}", e))?;

    // Create stream binding with proper lifetime
    let mut stream_owned = stream;
    let mut tls_stream = rustls::Stream::new(&mut conn, &mut stream_owned);

    // Build and send request
    let request = build_http_request(host, pending)?;
    tls_stream
        .write_all(&request)
        .map_err(|e| format!("TLS write failed: {}", e))?;

    // Read response
    let mut response_data = Vec::new();
    let mut buf = [0u8; 4096];

    loop {
        match tls_stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response_data.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(e) => return Err(format!("TLS read failed: {}", e)),
        }

        // Simple check if we have a complete response
        if response_data.len() > 12 && response_data.windows(4).any(|w| w == b"\r\n\r\n") {
            // Check if we have Content-Length or chunked
            let header_str = String::from_utf8_lossy(&response_data);
            if let Some(body_start) = header_str.find("\r\n\r\n") {
                let headers = &header_str[..body_start];

                // Check for Content-Length
                if let Some(cl_line) = headers
                    .lines()
                    .find(|l| l.to_lowercase().starts_with("content-length:"))
                {
                    if let Ok(content_length) = cl_line[15..].trim().parse::<usize>() {
                        let body_received = response_data.len() - body_start - 4;
                        if body_received >= content_length {
                            break;
                        }
                    }
                } else if !headers
                    .to_lowercase()
                    .contains("transfer-encoding: chunked")
                {
                    // No Content-Length and not chunked - assume complete
                    break;
                }
            }
        }
    }

    parse_http_response_from_bytes(&response_data)
}

/// HTTP/1.1 の要求を組み立てる。
///
/// モジュールが渡した `:method`・`:path`・ヘッダは検査し、CR・LF 等で要求を分けられる値は断る（モジュールが
/// クライアントの要求の値をそのまま渡すと、呼び出し先へ別の要求を足せた）。本文の長さと接続の扱い
/// （`Host`・`Content-Length`・`Transfer-Encoding`・`Connection`）は veil が決め、モジュールの値は使わない。
/// 本文はバイト列のまま付ける（以前は UTF-8 として読み直していて、バイナリの本文が変わり `Content-Length` と
/// 食い違っていた）
fn build_http_request(host: &str, pending: &GlobalPendingCall) -> Result<Vec<u8>, String> {
    let headers = &pending.call.headers;
    let pseudo = |name: &[u8]| {
        headers
            .iter()
            .find(|(k, _)| k.as_slice() == name)
            .map(|(_, v)| v.as_slice())
    };
    let method = pseudo(b":method").unwrap_or(b"GET");
    let path = pseudo(b":path").unwrap_or(b"/");
    if !crate::http_utils::is_valid_header_name(method) {
        return Err("invalid :method in http_call".to_string());
    }
    if path.is_empty() || path.iter().any(|&b| b <= 0x20 || b == 0x7f) {
        return Err("invalid :path in http_call".to_string());
    }
    let body = &pending.call.body;

    let mut request = Vec::with_capacity(256 + body.len());
    request.extend_from_slice(method);
    request.push(b' ');
    request.extend_from_slice(path);
    request.extend_from_slice(b" HTTP/1.1\r\nHost: ");
    request.extend_from_slice(host.as_bytes());
    request.extend_from_slice(b"\r\n");
    for (key, value) in headers {
        if key.first().copied() == Some(b':') {
            continue;
        }
        if !crate::http_utils::is_valid_header_name(key)
            || !crate::http_utils::is_valid_header_value(value)
        {
            return Err("invalid header in http_call".to_string());
        }
        if [
            b"host".as_slice(),
            b"content-length",
            b"transfer-encoding",
            b"connection",
        ]
        .iter()
        .any(|n| key.eq_ignore_ascii_case(n))
        {
            continue;
        }
        request.extend_from_slice(key);
        request.extend_from_slice(b": ");
        request.extend_from_slice(value);
        request.extend_from_slice(b"\r\n");
    }
    if !body.is_empty() {
        request.extend_from_slice(format!("Content-Length: {}\r\n", body.len()).as_bytes());
    }
    request.extend_from_slice(b"Connection: close\r\n\r\n");
    request.extend_from_slice(body);
    Ok(request)
}

/// Parse HTTP response from TcpStream
fn parse_http_response(stream: TcpStream) -> Result<HttpCallResponse, String> {
    let mut reader = BufReader::new(stream);
    let mut response_data = Vec::new();

    // Read all available data
    let mut buf = [0u8; 4096];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => response_data.extend_from_slice(&buf[..n]),
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
            Err(ref e) if e.kind() == std::io::ErrorKind::TimedOut => break,
            Err(e) => return Err(format!("Read failed: {}", e)),
        }
    }

    parse_http_response_from_bytes(&response_data)
}

/// Parse HTTP response from bytes
fn parse_http_response_from_bytes(data: &[u8]) -> Result<HttpCallResponse, String> {
    if data.is_empty() {
        return Err("Empty response".to_string());
    }

    let response_str = String::from_utf8_lossy(data);

    // Find header/body boundary
    let header_end = response_str
        .find("\r\n\r\n")
        .ok_or("Invalid response: no header/body boundary")?;

    let header_section = &response_str[..header_end];
    let body_start = header_end + 4;

    // Parse status line
    let status_line = header_section
        .lines()
        .next()
        .ok_or("Invalid response: no status line")?;

    let status_parts: Vec<&str> = status_line.splitn(3, ' ').collect();
    if status_parts.len() < 2 {
        return Err("Invalid status line".to_string());
    }

    let status_code: u16 = status_parts[1].parse().map_err(|_| "Invalid status code")?;

    // Parse headers
    // F-62: Proxy-Wasm SDK の get_http_call_response_headers は Envoy 互換の
    // `:status` 擬似ヘッダを期待するため先頭に付与する
    let mut headers = Vec::new();
    headers.push((b":status".to_vec(), status_code.to_string().into_bytes()));
    for line in header_section.lines().skip(1) {
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().as_bytes().to_vec();
            let value = line[colon_pos + 1..].trim().as_bytes().to_vec();
            headers.push((key, value));
        }
    }

    // Extract body
    let body = if body_start < data.len() {
        data[body_start..].to_vec()
    } else {
        Vec::new()
    };

    Ok(HttpCallResponse {
        status_code,
        headers,
        body,
        trailers: Vec::new(),
    })
}

/// Execute HTTP call with provided connection details
///
/// This is a convenience wrapper for execute_http_call that returns
/// an error response on failure instead of Result.
pub fn execute_http_call_safe(
    pending: &GlobalPendingCall,
    upstream_host: &str,
    upstream_port: u16,
    use_tls: bool,
) -> HttpCallResponse {
    match execute_http_call(pending, upstream_host, upstream_port, use_tls) {
        Ok(response) => {
            ftlog::debug!(
                "[wasm:http_call] HTTP call completed: status={} body_len={}",
                response.status_code,
                response.body.len()
            );
            response
        }
        Err(e) => {
            ftlog::error!(
                "[wasm:http_call] HTTP call failed: module='{}' error={}",
                pending.module_name,
                e
            );
            HttpCallResponse {
                status_code: 504,
                headers: vec![(b"x-wasm-error".to_vec(), b"http_call_failed".to_vec())],
                body: format!("HTTP call failed: {}", e).into_bytes(),
                trailers: Vec::new(),
            }
        }
    }
}

#[cfg(test)]
mod tests {

    /// F-282: ゲストの timeout_ms は上限で打ち切る（負の値はホスト関数で 0 にしてある）
    #[test]
    fn http_call_timeout_is_clamped() {
        set_max_timeout_ms(crate::wasm::DEFAULT_HTTP_CALL_MAX_TIMEOUT_MS);
        assert_eq!(clamp_timeout(u32::MAX), Duration::from_millis(60_000));
        assert_eq!(clamp_timeout(0), Duration::from_millis(1));
        assert_eq!(clamp_timeout(250), Duration::from_millis(250));
    }

    use super::*;
    use crate::wasm::types::PendingHttpCall;

    #[test]
    fn test_build_http_request() {
        let pending = GlobalPendingCall {
            module_name: "test".to_string(),
            token: 1,
            call: PendingHttpCall {
                token: 1,
                upstream: "backend".to_string(),
                timeout_ms: 5000,
                headers: vec![
                    (b":method".to_vec(), b"GET".to_vec()),
                    (b":path".to_vec(), b"/api/test".to_vec()),
                    (b"user-agent".to_vec(), b"wasm-client".to_vec()),
                ],
                body: vec![],
                trailers: vec![],
            },
        };

        let request =
            String::from_utf8(build_http_request("example.com", &pending).unwrap()).unwrap();

        assert!(request.starts_with("GET /api/test HTTP/1.1\r\n"));
        assert!(request.contains("Host: example.com\r\n"));
        assert!(request.contains("user-agent: wasm-client\r\n"));
        assert!(request.ends_with("\r\n\r\n"));
    }

    /// モジュールが渡した値で要求を分けられない（CR・LF を含む値・token でない名前・空白を含む `:path` は断る）。
    /// 枠組みのヘッダはモジュールの値を使わず、本文はバイト列のまま長さどおりに付く
    #[test]
    fn build_http_request_rejects_injection_and_keeps_binary_body() {
        let call = |headers: Vec<(&[u8], &[u8])>, body: &[u8]| GlobalPendingCall {
            module_name: "test".to_string(),
            token: 1,
            call: PendingHttpCall {
                token: 1,
                upstream: "backend".to_string(),
                timeout_ms: 5000,
                headers: headers
                    .into_iter()
                    .map(|(k, v)| (k.to_vec(), v.to_vec()))
                    .collect(),
                body: body.to_vec(),
                trailers: vec![],
            },
        };
        for bad in [
            vec![(b":path".as_slice(), b"/a HTTP/1.1\r\nX: y".as_slice())],
            vec![(b":method".as_slice(), b"GET /x".as_slice())],
            vec![(
                b"x-user".as_slice(),
                b"a\r\n\r\nGET /admin HTTP/1.1".as_slice(),
            )],
            vec![(b"x-user".as_slice(), b"a\nb".as_slice())],
            vec![(b"bad name".as_slice(), b"v".as_slice())],
        ] {
            assert!(build_http_request("example.com", &call(bad, b"")).is_err());
        }
        let body = [0xffu8, 0xfe, b'\r', b'\n', 0x00, 0x80];
        let req = build_http_request(
            "example.com",
            &call(
                vec![
                    (b":method".as_slice(), b"POST".as_slice()),
                    (b":path", b"/upload"),
                    (b"Content-Length", b"1"),
                    (b"transfer-encoding", b"chunked"),
                    (b"Host", b"evil.example"),
                    (b"x-ok", b"1"),
                ],
                &body,
            ),
        )
        .unwrap();
        let head_end = req.windows(4).position(|w| w == b"\r\n\r\n").unwrap() + 4;
        let head = std::str::from_utf8(&req[..head_end]).unwrap();
        assert_eq!(
            head,
            "POST /upload HTTP/1.1\r\nHost: example.com\r\nx-ok: 1\r\nContent-Length: 6\r\nConnection: close\r\n\r\n"
        );
        assert_eq!(&req[head_end..], &body);
    }

    #[test]
    fn test_parse_http_response() {
        let response_bytes = b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"ok\": true}\r\n";

        let response = parse_http_response_from_bytes(response_bytes).unwrap();

        assert_eq!(response.status_code, 200);
        assert!(response
            .headers
            .iter()
            .any(|(k, v)| k.as_slice() == b"Content-Type" && v.as_slice() == b"application/json"));
        assert!(response.body.starts_with(b"{\"ok\":"));
    }
}
