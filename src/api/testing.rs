// F-291: ソケットを開かずにアプリのルートを呼ぶ（C の veil_test_*・veil_kit_test_*）。サーバと同じ規則でルートを選び
// （パスが合うもののうち、メソッドが合うものの中で最も具体的なもの。パスだけ合えば 405、無ければ 404）、ストリーミングの
// 本文は集めてから返す。応答後の処理（after_send）は本文を集め終えた後に走らせる。
//! In-process calls of an [`App`](super::App)'s routes, without a server (see
//! [`App::into_test_client`](super::App::into_test_client)).

use super::handler::HandlerEntry;
use super::request::{HttpVersion, Request};
use super::stream::BodyChunk;
use bytes::{Bytes, BytesMut};
use futures::StreamExt;

/// Calls the routes of an app in-process.
pub struct TestClient {
    entries: Vec<HandlerEntry>,
}

/// The response of a [`TestClient`] call (the body is complete; a streamed body was collected).
#[derive(Debug, Clone)]
pub struct TestResponse {
    /// The status code.
    pub status: u16,
    /// The response headers, in order.
    pub headers: Vec<(Bytes, Bytes)>,
    /// The whole body.
    pub body: Bytes,
    /// The trailers of a streamed body.
    pub trailers: Vec<(Bytes, Bytes)>,
    /// Whether a streamed body was aborted midway.
    pub aborted: bool,
}

impl TestResponse {
    /// The first value of the header `name` (case-insensitive).
    pub fn header(&self, name: &str) -> Option<&[u8]> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name.as_bytes()))
            .map(|(_, v)| &v[..])
    }

    /// The body as text (invalid UTF-8 replaced).
    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }
}

impl TestClient {
    pub(crate) fn new(entries: Vec<HandlerEntry>) -> TestClient {
        TestClient { entries }
    }

    /// The route for `method` and `path`: among the routes whose path matches, the most specific one that accepts
    /// the method (`HEAD` also matches `GET`). `Err(405)` when only the path matches, `Err(404)` otherwise.
    fn select(&self, method: &[u8], path: &[u8]) -> Result<&HandlerEntry, u16> {
        let mut path_matched = false;
        let mut best: Option<&HandlerEntry> = None;
        for e in &self.entries {
            if !e.pattern.matches(path) {
                continue;
            }
            path_matched = true;
            let ok = e.methods.is_empty()
                || e.methods.iter().any(|m| m.as_bytes() == method)
                || (method == b"HEAD" && e.methods.iter().any(|m| m == "GET"));
            if ok && best.is_none_or(|b| e.pattern.specificity() > b.pattern.specificity()) {
                best = Some(e);
            }
        }
        match best {
            Some(e) => Ok(e),
            None if path_matched => Err(405),
            None => Err(404),
        }
    }

    /// Dispatches `req` and waits for the whole response. Call it from a test thread (not from a server worker).
    pub fn call(&self, req: Request) -> TestResponse {
        futures::executor::block_on(self.call_async(req))
    }

    /// The asynchronous form of [`TestClient::call`].
    pub async fn call_async(&self, req: Request) -> TestResponse {
        let head = req.method.as_ref() == b"HEAD";
        let mut resp = match self.select(&req.method, &req.path) {
            Ok(e) => e.call(req).await,
            Err(404) => super::response::Response::not_found(),
            Err(code) => super::response::Response::error(code, "Method Not Allowed"),
        };
        let mut out = TestResponse {
            status: resp.status,
            headers: std::mem::take(&mut resp.headers),
            body: std::mem::take(&mut resp.body),
            trailers: Vec::new(),
            aborted: false,
        };
        if let Some(mut stream) = resp.stream.take() {
            let mut buf = BytesMut::new();
            while let Some(chunk) = stream.next().await {
                match chunk {
                    BodyChunk::Data(d) => buf.extend_from_slice(&d),
                    BodyChunk::Trailers(t) => {
                        out.trailers = t;
                        break;
                    }
                    BodyChunk::Abort(_) => {
                        out.aborted = true;
                        break;
                    }
                }
            }
            out.body = buf.freeze();
        }
        if head {
            out.body = Bytes::new();
        }
        // 応答後の処理は「送り終えた」ここで走らせる（AfterSend の Drop）
        drop(resp.take_after_send());
        out
    }

    /// The number of routes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether there are no routes.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl Request {
    // F-291: 試験・アダプタ用（C の veil_test_request の version）
    /// A copy with the HTTP version replaced (for tests and adapters).
    pub fn with_version(mut self, version: HttpVersion) -> Request {
        self.version = version;
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{App, Response};

    #[test]
    fn dispatches_like_the_server() {
        let app = App::new()
            .without_macro_routes()
            .get("/users/{id}", |req: Request| async move {
                format!("user {}", req.param("id").unwrap_or_default())
            })
            .get("/users/me", |_req: Request| async { "me" })
            .post("/users/{id}", |_req: Request| async { "posted" })
            .wrap_on("/users", |inner| {
                std::sync::Arc::new(move |req| {
                    let inner = inner.clone();
                    Box::pin(async move {
                        let r: Response = inner(req).await;
                        r.with_header("x-mw", "1").after_send(|| {})
                    })
                })
            });
        let t = app.into_test_client().unwrap();
        assert_eq!(t.len(), 3);
        let r = t.call(Request::new("GET", "/users/42", vec![], ""));
        assert_eq!((r.status, r.text()), (200, "user 42".to_string()));
        assert_eq!(r.header("x-mw"), Some(&b"1"[..]));
        // より具体的なルート・メソッドの選択・405・404・HEAD
        assert_eq!(
            t.call(Request::new("GET", "/users/me", vec![], "")).text(),
            "me"
        );
        assert_eq!(
            t.call(Request::new("POST", "/users/1", vec![], "")).text(),
            "posted"
        );
        assert_eq!(
            t.call(Request::new("DELETE", "/users/1", vec![], ""))
                .status,
            405
        );
        assert_eq!(t.call(Request::new("GET", "/nope", vec![], "")).status, 404);
        let h = t.call(Request::new("HEAD", "/users/me", vec![], ""));
        assert_eq!((h.status, h.body.len()), (200, 0));
    }
}
