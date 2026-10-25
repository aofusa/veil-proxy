// F-188
//! Handler responses and the conversion from various return types (`IntoResponse`).

use bytes::Bytes;

// F-197: ストリームは断片とトレーラ。
/// A response returned by a handler. The body is either `Bytes` (passed zero-copy to each protocol's send path)
/// or a stream (`Response::stream`).
pub struct Response {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(Bytes, Bytes)>,
    pub(crate) body: Bytes,
    // F-197
    /// Streaming body (sent in place of `body` when present).
    pub(crate) stream: Option<super::stream::BodyStream>,
    // F-261
    /// Protocol upgrade (`101 Switching Protocols`): the protocol name and the function that takes the connection.
    // Box: 応答の大きさを増やさない（ハンドラの戻り値・抽出子の `Err` として毎要求ムーブされる）
    pub(crate) upgrade: Option<Box<super::upgrade::UpgradeHook>>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .field("streaming", &self.stream.is_some())
            .field("upgrade", &self.upgrade.is_some())
            .finish()
    }
}

const CT_TEXT: &[u8] = b"text/plain; charset=utf-8";
const CT_HTML: &[u8] = b"text/html; charset=utf-8";
const CT_JSON: &[u8] = b"application/json";
const CT_OCTET: &[u8] = b"application/octet-stream";

impl Response {
    /// A response with an empty body and no headers.
    pub fn new(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: Bytes::new(),
            stream: None,
            upgrade: None,
        }
    }

    fn with_type(status: u16, content_type: &'static [u8], body: Bytes) -> Response {
        Response {
            status,
            headers: vec![(
                Bytes::from_static(b"content-type"),
                Bytes::from_static(content_type),
            )],
            body,
            stream: None,
            upgrade: None,
        }
    }

    /// `text/plain; charset=utf-8` (200).
    pub fn text(body: impl Into<String>) -> Response {
        Self::with_type(200, CT_TEXT, Bytes::from(body.into()))
    }

    /// `text/html; charset=utf-8` (200).
    pub fn html(body: impl Into<String>) -> Response {
        Self::with_type(200, CT_HTML, Bytes::from(body.into()))
    }

    /// `application/json` (200). A serialization failure gives 500.
    pub fn json<T: serde::Serialize + ?Sized>(value: &T) -> Response {
        match serde_json::to_vec(value) {
            Ok(v) => Self::with_type(200, CT_JSON, Bytes::from(v)),
            Err(e) => Response::error(500, &format!("failed to serialize response: {}", e)),
        }
    }

    /// Already serialized JSON (200).
    pub fn json_bytes(body: impl Into<Bytes>) -> Response {
        Self::with_type(200, CT_JSON, body.into())
    }

    /// `application/octet-stream` (200).
    pub fn bytes(body: impl Into<Bytes>) -> Response {
        Self::with_type(200, CT_OCTET, body.into())
    }

    // F-197
    /// A streaming response: the body is sent in chunks and trailers can follow at the end.
    /// Set `content-type` with `with_header`.
    pub fn stream<S>(status: u16, body: S) -> Response
    where
        S: futures::Stream<Item = super::stream::BodyChunk> + Send + 'static,
    {
        Response {
            status,
            headers: Vec::new(),
            body: Bytes::new(),
            // fuse: 送出側は終端の後にもう一度 poll することがある（`stream::coalesce`）
            stream: Some(Box::pin(futures::StreamExt::fuse(body))),
            upgrade: None,
        }
    }

    // F-261
    /// `101 Switching Protocols` to `protocol` (HTTP/1.1 only). After the response head is written, the connection
    /// (with any bytes the client already sent after the request) is passed to `on_upgrade`, which runs on the
    /// worker thread until it returns; the connection closes when the [`Upgraded`](super::upgrade::Upgraded) is dropped.
    ///
    /// The request must carry `Connection: upgrade` and `Upgrade`, be HTTP/1.1 and have no body; otherwise the
    /// response is replaced with 400 (HTTP/2 and HTTP/3 have no upgrades). Headers added with `with_header` are sent
    /// with the 101. `h2c` is reserved (500).
    pub fn switching_protocols<F, Fut>(protocol: &str, on_upgrade: F) -> Response
    where
        F: FnOnce(super::upgrade::Upgraded) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = ()> + 'static,
    {
        let mut r = Response::new(101);
        r.upgrade = Some(Box::new(super::upgrade::UpgradeHook::new(
            protocol, on_upgrade,
        )));
        r
    }

    /// Whether this is a protocol upgrade response ([`Response::switching_protocols`]).
    pub fn is_upgrade(&self) -> bool {
        self.upgrade.is_some()
    }

    /// Whether this is a streaming response.
    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// Takes the stream out (for adapters and tests).
    pub fn take_stream(&mut self) -> Option<super::stream::BodyStream> {
        self.stream.take()
    }

    /// 204 No Content。
    pub fn no_content() -> Response {
        Response::new(204)
    }

    /// A JSON error response `{"detail": "<message>"}` (the same shape as FastAPI).
    pub fn error(status: u16, message: &str) -> Response {
        let body = serde_json::json!({ "detail": message });
        let mut r = Response::json(&body);
        r.status = status;
        r
    }

    /// 404 Not Found (JSON).
    pub fn not_found() -> Response {
        Response::error(404, "Not Found")
    }

    /// 400 Bad Request (JSON).
    pub fn bad_request(message: &str) -> Response {
        Response::error(400, message)
    }

    /// Changes the status.
    pub fn with_status(mut self, status: u16) -> Response {
        self.status = status;
        self
    }

    /// Alias of `with_status`, so that `Response::json(&x).status(201)` works.
    pub fn status(self, status: u16) -> Response {
        self.with_status(status)
    }

    /// Adds a header (an existing header of the same name is not replaced; this appends).
    pub fn with_header(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Response {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// Replaces the header of the same name (case-insensitive).
    pub fn set_header(&mut self, name: &str, value: impl Into<Bytes>) {
        self.headers
            .retain(|(n, _)| !n.eq_ignore_ascii_case(name.as_bytes()));
        self.headers
            .push((Bytes::copy_from_slice(name.as_bytes()), value.into()));
    }

    /// Replaces the body.
    pub fn with_body(mut self, body: impl Into<Bytes>) -> Response {
        self.body = body.into();
        self
    }

    /// The HTTP status code.
    pub fn status_code(&self) -> u16 {
        self.status
    }

    /// The list of response headers (name, value).
    pub fn headers(&self) -> &[(Bytes, Bytes)] {
        &self.headers
    }

    /// The response body.
    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// A header (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| v.as_ref())
    }
}

/// Builds a `Response` from a handler's return value.
pub trait IntoResponse {
    /// Converts into a response.
    fn into_response(self) -> Response;
}

impl IntoResponse for Response {
    fn into_response(self) -> Response {
        self
    }
}

impl IntoResponse for &'static str {
    fn into_response(self) -> Response {
        Response::with_type(200, CT_TEXT, Bytes::from_static(self.as_bytes()))
    }
}

impl IntoResponse for String {
    fn into_response(self) -> Response {
        Response::text(self)
    }
}

impl IntoResponse for Bytes {
    fn into_response(self) -> Response {
        Response::bytes(self)
    }
}

impl IntoResponse for Vec<u8> {
    fn into_response(self) -> Response {
        Response::bytes(self)
    }
}

impl IntoResponse for () {
    fn into_response(self) -> Response {
        Response::no_content()
    }
}

impl IntoResponse for serde_json::Value {
    fn into_response(self) -> Response {
        Response::json(&self)
    }
}

impl<T: IntoResponse> IntoResponse for (u16, T) {
    fn into_response(self) -> Response {
        self.1.into_response().with_status(self.0)
    }
}

impl<T: IntoResponse> IntoResponse for Option<T> {
    fn into_response(self) -> Response {
        match self {
            Some(v) => v.into_response(),
            None => Response::not_found(),
        }
    }
}

impl<T: IntoResponse, E: IntoResponse> IntoResponse for Result<T, E> {
    fn into_response(self) -> Response {
        match self {
            Ok(v) => v.into_response(),
            Err(e) => e.into_response(),
        }
    }
}

/// A JSON body (usable both as an extractor and as a return value). As a return value it is sent as `application/json`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Json<T>(pub T);

impl<T> std::ops::Deref for Json<T> {
    type Target = T;
    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T> std::ops::DerefMut for Json<T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.0
    }
}

impl<T: serde::Serialize> IntoResponse for Json<T> {
    fn into_response(self) -> Response {
        Response::json(&self.0)
    }
}

/// An HTML body as a return value.
#[derive(Debug, Clone)]
pub struct Html<T>(pub T);

impl<T: Into<String>> IntoResponse for Html<T> {
    fn into_response(self) -> Response {
        Response::html(self.0)
    }
}

/// A general-purpose error a handler can return with `?` (a status plus a message; the body is `{"detail": ...}`).
#[derive(Debug, Clone)]
pub struct ApiError {
    /// The HTTP status code.
    pub status: u16,
    /// The message returned as `{"detail": ...}`.
    pub message: String,
}

impl ApiError {
    /// Creates an error from a status code and a message.
    pub fn new(status: u16, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
        }
    }
    /// 400 Bad Request。
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(400, message)
    }
    /// 404 Not Found。
    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new(404, message)
    }
    /// 500 Internal Server Error。
    pub fn internal(message: impl Into<String>) -> ApiError {
        ApiError::new(500, message)
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.message)
    }
}

impl std::error::Error for ApiError {}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        Response::error(self.status, &self.message)
    }
}

impl From<serde_json::Error> for ApiError {
    fn from(e: serde_json::Error) -> ApiError {
        ApiError::bad_request(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions() {
        assert_eq!("hi".into_response().status_code(), 200);
        assert_eq!(().into_response().status_code(), 204);
        let r = (201u16, Json(serde_json::json!({"a": 1}))).into_response();
        assert_eq!(r.status_code(), 201);
        assert_eq!(r.header("content-type"), Some(&b"application/json"[..]));
        assert_eq!(&r.body()[..], br#"{"a":1}"#);
        let r: Response = Option::<String>::None.into_response();
        assert_eq!(r.status_code(), 404);
        let r = Result::<String, ApiError>::Err(ApiError::bad_request("x")).into_response();
        assert_eq!(r.status_code(), 400);
        assert_eq!(&r.body()[..], br#"{"detail":"x"}"#);
        let r = Response::json(&serde_json::json!([1])).status(202);
        assert_eq!(r.status_code(), 202);
    }
}
