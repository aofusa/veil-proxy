//! WASM HTTP Call Executor
//!
//! Provides synchronous HTTP client for executing WASM proxy_http_call requests
//! from the tick thread.

use std::io::{Read, Write, BufReader};
use std::net::TcpStream;
use std::time::Duration;
use std::sync::Arc;

use super::types::HttpCallResponse;
use super::persistent_context::GlobalPendingCall;

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
    let stream = TcpStream::connect_timeout(
        &addr.parse().map_err(|e| format!("Invalid address: {}", e))?,
        timeout,
    ).map_err(|e| format!("Connection failed: {}", e))?;
    
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
    stream.write_all(request.as_bytes())
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
    use rustls::ClientConfig;
    use rustls::pki_types::ServerName;
    
    // Create TLS config with system roots
    let mut root_store = rustls::RootCertStore::empty();
    
    // Add webpki roots
    root_store.extend(
        webpki_roots::TLS_SERVER_ROOTS.iter().cloned()
    );
    
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
    tls_stream.write_all(request.as_bytes())
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
                if let Some(cl_line) = headers.lines().find(|l| l.to_lowercase().starts_with("content-length:")) {
                    if let Ok(content_length) = cl_line[15..].trim().parse::<usize>() {
                        let body_received = response_data.len() - body_start - 4;
                        if body_received >= content_length {
                            break;
                        }
                    }
                } else if !headers.to_lowercase().contains("transfer-encoding: chunked") {
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
    let method = pending.call.headers.iter()
        .find(|(k, _)| k == ":method")
        .map(|(_, v)| v.as_str())
        .unwrap_or("GET");
    
    let path = pending.call.headers.iter()
        .find(|(k, _)| k == ":path")
        .map(|(_, v)| v.as_str())
        .unwrap_or("/");
    
    // Request line
    request.push_str(&format!("{} {} HTTP/1.1\r\n", method, path));
    
    // Host header
    request.push_str(&format!("Host: {}\r\n", host));
    
    // Add other headers (skip pseudo-headers)
    for (key, value) in &pending.call.headers {
        if !key.starts_with(':') {
            request.push_str(&format!("{}: {}\r\n", key, value));
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
    let header_end = response_str.find("\r\n\r\n")
        .ok_or("Invalid response: no header/body boundary")?;
    
    let header_section = &response_str[..header_end];
    let body_start = header_end + 4;
    
    // Parse status line
    let status_line = header_section.lines().next()
        .ok_or("Invalid response: no status line")?;
    
    let status_parts: Vec<&str> = status_line.splitn(3, ' ').collect();
    if status_parts.len() < 2 {
        return Err("Invalid status line".to_string());
    }
    
    let status_code: u16 = status_parts[1].parse()
        .map_err(|_| "Invalid status code")?;
    
    // Parse headers
    let mut headers = Vec::new();
    for line in header_section.lines().skip(1) {
        if let Some(colon_pos) = line.find(':') {
            let key = line[..colon_pos].trim().to_string();
            let value = line[colon_pos + 1..].trim().to_string();
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
                headers: vec![("x-wasm-error".to_string(), "http_call_failed".to_string())],
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
                    (":method".to_string(), "GET".to_string()),
                    (":path".to_string(), "/api/test".to_string()),
                    ("user-agent".to_string(), "wasm-client".to_string()),
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
        assert!(response.headers.iter().any(|(k, v)| k == "Content-Type" && v == "application/json"));
        assert!(response.body.starts_with(b"{\"ok\":"));
    }
}
