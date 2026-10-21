// F-188
//! Common code through which the HTTP/1.1, HTTP/2 and HTTP/3 paths call `Backend::Handler`.

use super::handler::HandlerEntry;
use super::request::{split_target, HttpVersion, Request};
use super::response::Response;
use bytes::Bytes;
use futures::FutureExt;
use std::net::IpAddr;

/// Builds a request (`target` is the `:path` or request line, including the query).
pub(crate) fn build_request(
    method: Bytes,
    target: Bytes,
    headers: Vec<(Bytes, Bytes)>,
    body: Bytes,
    client_ip: &str,
    version: HttpVersion,
) -> Request {
    let (path, query) = split_target(&target);
    Request {
        method,
        path,
        query,
        headers,
        body,
        params: Vec::new(),
        client_ip: client_ip
            .parse::<IpAddr>()
            .unwrap_or(IpAddr::from([0, 0, 0, 0])),
        version,
        body_stream: None,
    }
}

// F-202: 今は tokio 互換レイヤーの包んだ Waker だけ。
/// State kept per connection and reused across requests.
#[derive(Default)]
pub(crate) struct CallCache {
    #[cfg(feature = "tokio-compat")]
    waker: crate::tokio::waker::SafeWakerCache,
}

/// Calls the handler. A body over the limit gives 413 and a handler panic gives 500; for HEAD the body is dropped
/// and `content-length` carries the original length.
pub(crate) async fn call(entry: &HandlerEntry, req: Request) -> Response {
    call_with(entry, req, &mut CallCache::default()).await
}

/// Like [`call`], but uses the per-connection [`CallCache`] (reduces per-request allocation on HTTP/1.1 keep-alive).
pub(crate) async fn call_with(
    entry: &HandlerEntry,
    req: Request,
    cache: &mut CallCache,
) -> Response {
    if req.body.len() > super::app::max_body_size() {
        return Response::error(413, "Payload Too Large");
    }
    let head = &req.method[..] == b"HEAD";
    // tokio 互換レイヤー（F-198）: ハンドラの中で使う同期プリミティブ・チャネルが別スレッドから
    // 起こしても、所有スレッドで再開するように Waker を包む
    #[cfg(feature = "tokio-compat")]
    let fut = crate::tokio::waker::WithSafeWakerIn::new(entry.call(req), &mut cache.waker);
    #[cfg(not(feature = "tokio-compat"))]
    let fut = {
        let _ = cache;
        entry.call(req)
    };
    let mut resp = match std::panic::AssertUnwindSafe(fut).catch_unwind().await {
        Ok(r) => r,
        Err(_) => {
            ftlog::error!("[api] handler '{}' panicked", entry.name);
            Response::error(500, "Internal Server Error")
        }
    };
    if head {
        // HEAD はストリームを送らない（drop でハンドラ側の生成を止める）。長さは不明なので付けない。
        if resp.stream.take().is_some() {
            // HTTP/1.1 の HEAD でも GET と同じく chunked と示す（HTTP/2・HTTP/3 では取り除かれる）
            resp.headers.push((
                Bytes::from_static(b"transfer-encoding"),
                Bytes::from_static(b"chunked"),
            ));
            return resp;
        }
        let len = resp.body.len();
        resp.body = Bytes::new();
        if !resp
            .headers
            .iter()
            .any(|(n, _)| n.eq_ignore_ascii_case(b"content-length"))
        {
            let mut buf = itoa::Buffer::new();
            resp.headers.push((
                Bytes::from_static(b"content-length"),
                Bytes::copy_from_slice(buf.format(len).as_bytes()),
            ));
        }
    }
    resp
}

/// Lowercases header names for HTTP/2 and HTTP/3 and removes connection-specific headers (RFC 9113 section 8.2.2).
pub(crate) fn h2_h3_headers(resp: &mut Response) {
    resp.headers.retain(|(n, _)| {
        !(n.eq_ignore_ascii_case(b"connection")
            || n.eq_ignore_ascii_case(b"keep-alive")
            || n.eq_ignore_ascii_case(b"transfer-encoding")
            || n.eq_ignore_ascii_case(b"upgrade")
            || n.eq_ignore_ascii_case(b"proxy-connection"))
    });
    lowercase_names(&mut resp.headers);
}

/// Lowercases field names (HTTP/2 and HTTP/3 headers and trailers). Nothing is allocated if they are already lowercase.
pub(crate) fn lowercase_names(fields: &mut [(Bytes, Bytes)]) {
    for (n, _) in fields.iter_mut() {
        if n.iter().any(|b| b.is_ascii_uppercase()) {
            *n = Bytes::from(n.to_ascii_lowercase());
        }
    }
}

// F-197: ストリーミング応答は chunked。
/// Builds the HTTP/1.1 response head (status line through the blank line). `Content-Length` is set from the body
/// length (the handler's own value is used if present; for HEAD, `call` sets it). A streaming response uses
/// `Transfer-Encoding: chunked`.
pub(crate) fn http1_head(resp: &Response, keep_alive: bool) -> Vec<u8> {
    let mut out = Vec::with_capacity(128 + resp.headers.len() * 32);
    write_http1_head(&mut out, resp, keep_alive);
    out
}

// F-202
/// Writes [`http1_head`] to the end of `out` (the caller supplies a pooled buffer).
/// Whether the response is chunked is decided by `resp.stream`, so call this before taking the stream out.
pub(crate) fn write_http1_head(out: &mut Vec<u8>, resp: &Response, keep_alive: bool) {
    let chunked = resp.stream.is_some()
        || resp.headers.iter().any(|(n, v)| {
            n.eq_ignore_ascii_case(b"transfer-encoding") && v.eq_ignore_ascii_case(b"chunked")
        });
    out.extend_from_slice(b"HTTP/1.1 ");
    let mut buf = itoa::Buffer::new();
    out.extend_from_slice(buf.format(resp.status).as_bytes());
    out.push(b' ');
    out.extend_from_slice(reason_phrase(resp.status).as_bytes());
    out.extend_from_slice(b"\r\n");
    let mut has_len = false;
    for (n, v) in &resp.headers {
        if n.eq_ignore_ascii_case(b"connection") || n.eq_ignore_ascii_case(b"transfer-encoding") {
            continue;
        }
        if n.eq_ignore_ascii_case(b"content-length") {
            if chunked {
                continue;
            }
            has_len = true;
        }
        out.extend_from_slice(n);
        out.extend_from_slice(b": ");
        out.extend_from_slice(v);
        out.extend_from_slice(b"\r\n");
    }
    if chunked && !(resp.status == 204 || resp.status == 304 || resp.status < 200) {
        out.extend_from_slice(b"Transfer-Encoding: chunked\r\n");
    } else if !has_len && !(resp.status == 204 || resp.status == 304 || resp.status < 200) {
        out.extend_from_slice(b"Content-Length: ");
        out.extend_from_slice(buf.format(resp.body.len()).as_bytes());
        out.extend_from_slice(b"\r\n");
    }
    if keep_alive {
        out.extend_from_slice(b"Connection: keep-alive\r\n\r\n");
    } else {
        out.extend_from_slice(b"Connection: close\r\n\r\n");
    }
}

/// Reason phrases (adds the ones commonly used by APIs to the main table).
fn reason_phrase(status: u16) -> &'static str {
    match status {
        202 => "Accepted",
        203 => "Non-Authoritative Information",
        303 => "See Other",
        406 => "Not Acceptable",
        409 => "Conflict",
        410 => "Gone",
        411 => "Length Required",
        412 => "Precondition Failed",
        415 => "Unsupported Media Type",
        418 => "I'm a teapot",
        422 => "Unprocessable Content",
        423 => "Locked",
        428 => "Precondition Required",
        501 => "Not Implemented",
        _ => crate::http_utils::status_reason_phrase(status),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http1_head_serialization() {
        let r = Response::text("hi").with_header("X-A", "1");
        let h = String::from_utf8(http1_head(&r, true)).unwrap();
        assert!(h.starts_with("HTTP/1.1 200 OK\r\n"), "{}", h);
        assert!(h.contains("content-type: text/plain; charset=utf-8\r\n"));
        assert!(h.contains("X-A: 1\r\n"));
        assert!(h.contains("Content-Length: 2\r\n"));
        assert!(h.ends_with("Connection: keep-alive\r\n\r\n"));
        let h = String::from_utf8(http1_head(&Response::no_content(), false)).unwrap();
        assert!(!h.contains("Content-Length"), "{}", h);
    }

    #[test]
    fn http1_head_streaming_is_chunked() {
        let r = Response::stream(200, futures::stream::empty()).with_header("content-length", "5");
        let h = String::from_utf8(http1_head(&r, true)).unwrap();
        assert!(h.contains("Transfer-Encoding: chunked\r\n"), "{}", h);
        assert!(!h.to_ascii_lowercase().contains("content-length"), "{}", h);
    }

    #[test]
    fn head_drops_stream() {
        let entry = crate::api::handler::HandlerEntry::new(
            "s",
            "/",
            &["GET", "HEAD"],
            std::sync::Arc::new(|_req: Request| -> crate::api::handler::HandlerFuture {
                Box::pin(async {
                    Response::stream(
                        200,
                        futures::stream::iter(vec![crate::api::stream::BodyChunk::Data(
                            Bytes::from_static(b"x"),
                        )]),
                    )
                })
            }),
            Default::default(),
        )
        .unwrap();
        let req = build_request(
            Bytes::from_static(b"HEAD"),
            Bytes::from_static(b"/"),
            Vec::new(),
            Bytes::new(),
            "127.0.0.1",
            HttpVersion::Http11,
        );
        let resp = futures::executor::block_on(call(&entry, req));
        assert!(!resp.is_streaming());
        let h = String::from_utf8(http1_head(&resp, true)).unwrap();
        assert!(h.contains("Transfer-Encoding: chunked\r\n"), "{}", h);
    }

    #[test]
    fn h2_header_normalization() {
        let mut r = Response::text("x")
            .with_header("X-Upper", "1")
            .with_header("Connection", "close");
        h2_h3_headers(&mut r);
        assert!(r
            .headers
            .iter()
            .all(|(n, _)| n.iter().all(|b| !b.is_ascii_uppercase())));
        assert!(r.header("connection").is_none());
    }
}
