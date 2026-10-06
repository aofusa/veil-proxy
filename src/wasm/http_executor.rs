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
/// `BodyFilterResult::Pause` は非同期ボディ処理待ちに相当するが、現状どのプロトコル
/// 経路も async body pause を実装していない（h1/h2 のヘッダフィルタと同様、警告ログを
/// 出して元の本文を継続する）。
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
pub async fn apply_wasm_request_body(
    engine: &FilterEngine,
    modules: &[crate::wasm_plugin_config::ModuleRef],
    body: bytes::Bytes,
    end_of_stream: bool,
) -> WasmBodyOutcome {
    if modules.is_empty() {
        return WasmBodyOutcome::Continue(body);
    }
    match engine
        .on_request_body_with_modules(modules, body.clone(), end_of_stream)
        .await
    {
        BodyFilterResult::Continue { body } => WasmBodyOutcome::Continue(body),
        BodyFilterResult::LocalResponse(resp) => WasmBodyOutcome::LocalResponse(resp),
        BodyFilterResult::Pause => {
            ftlog::warn!(
                "[wasm] on_request_body requested Pause, but async body pause is not yet \
                 supported; continuing with the original body"
            );
            WasmBodyOutcome::Continue(body)
        }
    }
}

/// レスポンスボディに WASM `on_response_body` を適用する。
///
/// `modules` が空なら一切コストをかけずそのまま返す。
pub async fn apply_wasm_response_body(
    engine: &FilterEngine,
    modules: &[crate::wasm_plugin_config::ModuleRef],
    body: bytes::Bytes,
    end_of_stream: bool,
) -> WasmBodyOutcome {
    if modules.is_empty() {
        return WasmBodyOutcome::Continue(body);
    }
    match engine
        .on_response_body_with_modules(modules, body.clone(), end_of_stream)
        .await
    {
        BodyFilterResult::Continue { body } => WasmBodyOutcome::Continue(body),
        BodyFilterResult::LocalResponse(resp) => WasmBodyOutcome::LocalResponse(resp),
        BodyFilterResult::Pause => {
            ftlog::warn!(
                "[wasm] on_response_body requested Pause, but async body pause is not yet \
                 supported; continuing with the original body"
            );
            WasmBodyOutcome::Continue(body)
        }
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
    let timeout = Duration::from_millis(pending.call.timeout_ms as u64);

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
        .write_all(request.as_bytes())
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

    // Create TLS config with system roots
    let mut root_store = rustls::RootCertStore::empty();

    // Add webpki roots
    root_store.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());

    let config = ClientConfig::builder()
        .with_root_certificates(root_store)
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
        .write_all(request.as_bytes())
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

/// Build HTTP/1.1 request string
fn build_http_request(host: &str, pending: &GlobalPendingCall) -> Result<String, String> {
    let mut request = String::new();

    // Determine method and path from headers
    let method = pending
        .call
        .headers
        .iter()
        .find(|(k, _)| k.as_slice() == b":method")
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .unwrap_or("GET");

    let path = pending
        .call
        .headers
        .iter()
        .find(|(k, _)| k.as_slice() == b":path")
        .and_then(|(_, v)| std::str::from_utf8(v).ok())
        .unwrap_or("/");

    // Request line
    request.push_str(&format!("{} {} HTTP/1.1\r\n", method, path));

    // Host header
    request.push_str(&format!("Host: {}\r\n", host));

    // Add other headers (skip pseudo-headers)
    for (key, value) in &pending.call.headers {
        if key.first().copied() != Some(b':') {
            let k = std::str::from_utf8(key).unwrap_or("");
            let v = std::str::from_utf8(value).unwrap_or("");
            request.push_str(&format!("{}: {}\r\n", k, v));
        }
    }

    // Content-Length if body exists
    if !pending.call.body.is_empty() {
        request.push_str(&format!("Content-Length: {}\r\n", pending.call.body.len()));
    }

    // Connection header
    request.push_str("Connection: close\r\n");

    // End headers
    request.push_str("\r\n");

    // Body
    if !pending.call.body.is_empty() {
        request.push_str(&String::from_utf8_lossy(&pending.call.body));
    }

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

        let request = build_http_request("example.com", &pending).unwrap();

        assert!(request.starts_with("GET /api/test HTTP/1.1\r\n"));
        assert!(request.contains("Host: example.com\r\n"));
        assert!(request.contains("user-agent: wasm-client\r\n"));
        assert!(request.ends_with("\r\n\r\n"));
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
