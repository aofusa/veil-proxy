//! ハンドラの応答（F-188）と、さまざまな戻り値型からの変換（`IntoResponse`）。

use bytes::Bytes;

/// ハンドラが返す応答。本文は `Bytes`（ゼロコピーで各プロトコルの送信経路へ渡る）か、
/// ストリーム（`Response::stream`。断片とトレーラ、F-197）。
pub struct Response {
    pub(crate) status: u16,
    pub(crate) headers: Vec<(Bytes, Bytes)>,
    pub(crate) body: Bytes,
    /// ストリーミング本文（あれば `body` の代わりに送る）
    pub(crate) stream: Option<super::stream::BodyStream>,
}

impl std::fmt::Debug for Response {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .field("headers", &self.headers)
            .field("body_len", &self.body.len())
            .field("streaming", &self.stream.is_some())
            .finish()
    }
}

const CT_TEXT: &[u8] = b"text/plain; charset=utf-8";
const CT_HTML: &[u8] = b"text/html; charset=utf-8";
const CT_JSON: &[u8] = b"application/json";
const CT_OCTET: &[u8] = b"application/octet-stream";

impl Response {
    /// 空の本文・ヘッダ無しの応答。
    pub fn new(status: u16) -> Response {
        Response {
            status,
            headers: Vec::new(),
            body: Bytes::new(),
            stream: None,
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
        }
    }

    /// `text/plain; charset=utf-8`（200）。
    pub fn text(body: impl Into<String>) -> Response {
        Self::with_type(200, CT_TEXT, Bytes::from(body.into()))
    }

    /// `text/html; charset=utf-8`（200）。
    pub fn html(body: impl Into<String>) -> Response {
        Self::with_type(200, CT_HTML, Bytes::from(body.into()))
    }

    /// `application/json`（200）。シリアライズに失敗したら 500。
    pub fn json<T: serde::Serialize + ?Sized>(value: &T) -> Response {
        match serde_json::to_vec(value) {
            Ok(v) => Self::with_type(200, CT_JSON, Bytes::from(v)),
            Err(e) => Response::error(500, &format!("failed to serialize response: {}", e)),
        }
    }

    /// 既にシリアライズ済みの JSON（200）。
    pub fn json_bytes(body: impl Into<Bytes>) -> Response {
        Self::with_type(200, CT_JSON, body.into())
    }

    /// `application/octet-stream`（200）。
    pub fn bytes(body: impl Into<Bytes>) -> Response {
        Self::with_type(200, CT_OCTET, body.into())
    }

    /// ストリーミング応答（本文を断片で送り、最後にトレーラを送れる。F-197）。
    /// `content-type` は `with_header` で付ける。
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
        }
    }

    /// ストリーミング応答か。
    pub fn is_streaming(&self) -> bool {
        self.stream.is_some()
    }

    /// ストリームを取り出す（アダプタ・テスト用）。
    pub fn take_stream(&mut self) -> Option<super::stream::BodyStream> {
        self.stream.take()
    }

    /// 204 No Content。
    pub fn no_content() -> Response {
        Response::new(204)
    }

    /// JSON のエラー応答 `{"detail": "<message>"}`（FastAPI と同じ形）。
    pub fn error(status: u16, message: &str) -> Response {
        let body = serde_json::json!({ "detail": message });
        let mut r = Response::json(&body);
        r.status = status;
        r
    }

    /// 404 Not Found（JSON）。
    pub fn not_found() -> Response {
        Response::error(404, "Not Found")
    }

    /// 400 Bad Request（JSON）。
    pub fn bad_request(message: &str) -> Response {
        Response::error(400, message)
    }

    /// ステータスを変える。
    pub fn with_status(mut self, status: u16) -> Response {
        self.status = status;
        self
    }

    /// 設計書の記法（`Response::json(&x).status(201)`）に合わせた別名。
    pub fn status(self, status: u16) -> Response {
        self.with_status(status)
    }

    /// ヘッダを追加する（同名があれば置き換えない＝追加）。
    pub fn with_header(mut self, name: impl Into<Bytes>, value: impl Into<Bytes>) -> Response {
        self.headers.push((name.into(), value.into()));
        self
    }

    /// 同名のヘッダを置き換える（大文字小文字を区別しない）。
    pub fn set_header(&mut self, name: &str, value: impl Into<Bytes>) {
        self.headers
            .retain(|(n, _)| !n.eq_ignore_ascii_case(name.as_bytes()));
        self.headers
            .push((Bytes::copy_from_slice(name.as_bytes()), value.into()));
    }

    /// 本文を差し替える。
    pub fn with_body(mut self, body: impl Into<Bytes>) -> Response {
        self.body = body.into();
        self
    }

    pub fn status_code(&self) -> u16 {
        self.status
    }

    pub fn headers(&self) -> &[(Bytes, Bytes)] {
        &self.headers
    }

    pub fn body(&self) -> &Bytes {
        &self.body
    }

    /// ヘッダ（大文字小文字を区別しない）。
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| v.as_ref())
    }
}

/// ハンドラの戻り値から `Response` を作る。
pub trait IntoResponse {
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

/// JSON 本文（抽出子・戻り値の両方に使う）。戻り値では `application/json` で返す。
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

/// HTML 本文の戻り値。
#[derive(Debug, Clone)]
pub struct Html<T>(pub T);

impl<T: Into<String>> IntoResponse for Html<T> {
    fn into_response(self) -> Response {
        Response::html(self.0)
    }
}

/// ハンドラが `?` で返せる汎用エラー（ステータス + メッセージ、本文は `{"detail": ...}`）。
#[derive(Debug, Clone)]
pub struct ApiError {
    pub status: u16,
    pub message: String,
}

impl ApiError {
    pub fn new(status: u16, message: impl Into<String>) -> ApiError {
        ApiError {
            status,
            message: message.into(),
        }
    }
    pub fn bad_request(message: impl Into<String>) -> ApiError {
        ApiError::new(400, message)
    }
    pub fn not_found(message: impl Into<String>) -> ApiError {
        ApiError::new(404, message)
    }
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
