//! Tower の `Service` / `Layer` 互換（F-190、`tower-compat` feature。`api-core` に含む）。
//!
//! - `App::layer(L)` / `App::layer_on("/admin", L)`: ハンドラを tower のサービスとして包み、層を掛ける。
//!   veil のワーカー上でそのまま実行する（tokio の時計・I/O を使わない層向け: 認証・ヘッダ付与・CORS 等）。
//! - `App::tokio_service(path, svc)`: tower のサービス（axum の Router 等）を tokio 互換ランタイムで
//!   実行してマウントする（tokio に依存する層・ライブラリ向け）。
//!
//! リクエスト・応答は `http` クレートの型（本文は `http_body_util::Full<Bytes>`）で受け渡す。

use super::app::App;
use super::handler::HandlerFn;
use super::request::Request;
use super::response::Response;
use bytes::Bytes;
use http_body_util::BodyExt;
use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use tower_layer::Layer;
use tower_service::Service;

/// `tower::ServiceExt::oneshot` と同じ（準備を待ってから 1 回呼ぶ）。tower クレートに依存しないため
/// （tower の tokio を使う機能が有効だと、tokio を veil の互換レイヤーに置き換えたとき依存が循環する。F-198）。
async fn oneshot<S, R>(mut svc: S, req: R) -> Result<S::Response, S::Error>
where
    S: Service<R>,
{
    std::future::poll_fn(|cx| svc.poll_ready(cx)).await?;
    svc.call(req).await
}

/// tower 側に渡す本文の型。
pub type VeilBody = http_body_util::Full<Bytes>;

/// veil の `Request` を `http::Request` にする（元の `Request` は extensions にも入れる。
/// パスパラメータ等を内側のハンドラへ引き継ぐため）。
pub fn to_http_request(req: Request) -> http::Request<VeilBody> {
    let mut uri = String::with_capacity(req.path.len() + req.query.len() + 1);
    uri.push_str(req.path());
    if !req.query.is_empty() {
        uri.push('?');
        uri.push_str(req.query());
    }
    let mut b = http::Request::builder()
        .method(req.method())
        .uri(uri)
        .version(match req.version {
            super::request::HttpVersion::Http11 => http::Version::HTTP_11,
            super::request::HttpVersion::Http2 => http::Version::HTTP_2,
            super::request::HttpVersion::Http3 => http::Version::HTTP_3,
        });
    for (n, v) in req.headers.iter() {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(n),
            http::HeaderValue::from_bytes(v),
        ) {
            b = b.header(n, v);
        }
    }
    let body = VeilBody::new(req.body.clone());
    let mut out = b
        .body(body)
        .unwrap_or_else(|_| http::Request::new(VeilBody::new(Bytes::new())));
    out.extensions_mut().insert(req);
    out
}

/// `http::Request` を veil の `Request` に戻す（層が書き換えたメソッド・URI・ヘッダ・本文を反映する）。
pub async fn from_http_request(req: http::Request<VeilBody>) -> Request {
    let (parts, body) = req.into_parts();
    let body = body
        .collect()
        .await
        .map(|c| c.to_bytes())
        .unwrap_or_default();
    let orig = parts.extensions.get::<Request>().cloned();
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let headers: Vec<(Bytes, Bytes)> = parts
        .headers
        .iter()
        .map(|(n, v)| {
            (
                Bytes::copy_from_slice(n.as_str().as_bytes()),
                Bytes::copy_from_slice(v.as_bytes()),
            )
        })
        .collect();
    let mut req = Request::new(parts.method.as_str().to_string(), target, headers, body);
    if let Some(o) = orig {
        req.params = o.params;
        req.client_ip = o.client_ip;
        req.version = o.version;
        req.body_stream = o.body_stream;
    }
    req
}

/// veil の `Response` を `http::Response` にする。
pub fn to_http_response(resp: Response) -> http::Response<VeilBody> {
    let mut b = http::Response::builder().status(resp.status);
    for (n, v) in &resp.headers {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(n),
            http::HeaderValue::from_bytes(v),
        ) {
            b = b.header(n, v);
        }
    }
    let mut out = b
        .body(VeilBody::new(resp.body))
        .unwrap_or_else(|_| http::Response::new(VeilBody::new(Bytes::new())));
    // ストリーミング応答（F-197）は層を素通りさせる（extensions に入れて `from_http_response` で戻す）
    if let Some(stream) = resp.stream {
        out.extensions_mut()
            .insert(StreamCarrier(Arc::new(Mutex::new(Some(stream)))));
    }
    out
}

/// 層を通す間ストリーミング本文を預ける入れ物（http の extensions は `Clone + Sync` を要求する）。
#[derive(Clone)]
struct StreamCarrier(Arc<Mutex<Option<super::stream::BodyStream>>>);

/// `http::Response` を veil の `Response` にする（本文はまとめて読む）。
pub async fn from_http_response<B>(resp: http::Response<B>) -> Response
where
    B: http_body::Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    let (parts, body) = resp.into_parts();
    let carried = parts
        .extensions
        .get::<StreamCarrier>()
        .and_then(|c| c.0.lock().ok().and_then(|mut g| g.take()));
    if let Some(stream) = carried {
        let mut r = Response::stream(parts.status.as_u16(), stream);
        for (n, v) in parts.headers.iter() {
            r.headers.push((
                Bytes::copy_from_slice(n.as_str().as_bytes()),
                Bytes::copy_from_slice(v.as_bytes()),
            ));
        }
        return r;
    }
    let body = match body.collect().await {
        Ok(c) => c.to_bytes(),
        Err(e) => return Response::error(502, &format!("response body error: {}", e)),
    };
    let mut r = Response::new(parts.status.as_u16()).with_body(body);
    for (n, v) in parts.headers.iter() {
        r.headers.push((
            Bytes::copy_from_slice(n.as_str().as_bytes()),
            Bytes::copy_from_slice(v.as_bytes()),
        ));
    }
    r
}

/// veil のハンドラを tower の `Service` として見せる（層の内側）。
#[derive(Clone)]
pub struct HandlerService {
    func: HandlerFn,
}

impl Service<http::Request<VeilBody>> for HandlerService {
    type Response = http::Response<VeilBody>;
    type Error = Infallible;
    type Future = Pin<Box<dyn Future<Output = Result<Self::Response, Infallible>>>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Infallible>> {
        Poll::Ready(Ok(()))
    }

    fn call(&mut self, req: http::Request<VeilBody>) -> Self::Future {
        let func = self.func.clone();
        Box::pin(async move {
            let req = from_http_request(req).await;
            Ok(to_http_response(func(req).await))
        })
    }
}

impl App {
    /// 全ルートに tower の層を掛ける（veil のワーカー上で実行）。
    pub fn layer<L, B>(self, layer: L) -> App
    where
        L: Layer<HandlerService> + Clone + Send + Sync + 'static,
        L::Service: Service<http::Request<VeilBody>, Response = http::Response<B>>
            + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as Service<http::Request<VeilBody>>>::Error: std::fmt::Display,
        <L::Service as Service<http::Request<VeilBody>>>::Future: 'static,
        B: http_body::Body<Data = Bytes> + 'static,
        B::Error: std::fmt::Display,
    {
        self.layer_on("/", layer)
    }

    /// `prefix`（セグメント境界の前方一致）以下のルートだけに tower の層を掛ける。
    pub fn layer_on<L, B>(self, prefix: &str, layer: L) -> App
    where
        L: Layer<HandlerService> + Clone + Send + Sync + 'static,
        L::Service: Service<http::Request<VeilBody>, Response = http::Response<B>>
            + Clone
            + Send
            + Sync
            + 'static,
        <L::Service as Service<http::Request<VeilBody>>>::Error: std::fmt::Display,
        <L::Service as Service<http::Request<VeilBody>>>::Future: 'static,
        B: http_body::Body<Data = Bytes> + 'static,
        B::Error: std::fmt::Display,
    {
        self.wrap_on(prefix, move |inner: HandlerFn| -> HandlerFn {
            // 層はルートごとに 1 回だけ組み立て、リクエストごとに clone する（tower の慣習）。
            let svc = layer.layer(HandlerService { func: inner });
            Arc::new(move |req: Request| {
                let svc = svc.clone();
                Box::pin(async move {
                    match oneshot(svc, to_http_request(req)).await {
                        Ok(resp) => from_http_response(resp).await,
                        Err(e) => Response::error(500, &e.to_string()),
                    }
                })
            })
        })
    }

    /// tower のサービス（axum の `Router` 等）を tokio 互換ランタイムで実行して `path` にマウントする。
    #[cfg(any(feature = "tokio-compat", feature = "tokio-compat-bridge"))]
    /// `path` は `{*rest}` で下位パスをまとめて渡せる（例: `"/legacy/{*rest}"`）。
    pub fn tokio_service<S, B>(self, methods: &[&str], path: &str, svc: S) -> App
    where
        S: Service<http::Request<VeilBody>, Response = http::Response<B>>
            + Clone
            + Send
            + Sync
            + 'static,
        S::Future: Send + 'static,
        S::Error: std::fmt::Display + Send + 'static,
        B: http_body::Body<Data = Bytes> + Send + 'static,
        B::Error: std::fmt::Display,
    {
        let name = format!("tokio_service{}", path.replace(['/', '{', '}', '*'], "_"));
        let func: HandlerFn = Arc::new(move |req: Request| {
            let svc = svc.clone();
            Box::pin(async move {
                let http_req = to_http_request(req);
                let res = super::tokio_compat::try_run(async move {
                    match oneshot(svc, http_req).await {
                        Ok(resp) => Ok(from_http_response(resp).await),
                        Err(e) => Err(e.to_string()),
                    }
                })
                .await;
                match res {
                    Ok(Ok(r)) => r,
                    Ok(Err(e)) => Response::error(500, &e),
                    Err(e) => Response::error(500, &e.to_string()),
                }
            })
        });
        let meta = super::handler::OperationMeta {
            operation_id: name.clone(),
            ..Default::default()
        };
        self.route_fn(&name, methods, path, func, meta)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::stream::BodyChunk;
    use futures::StreamExt;

    #[test]
    fn streaming_response_survives_tower_conversion() {
        let resp = Response::stream(
            201,
            futures::stream::iter(vec![
                BodyChunk::Data(Bytes::from_static(b"a")),
                BodyChunk::Trailers(vec![(Bytes::from_static(b"t"), Bytes::from_static(b"1"))]),
            ]),
        )
        .with_header("x-h", "v");
        let http_resp = to_http_response(resp);
        let mut back = futures::executor::block_on(from_http_response(http_resp));
        assert_eq!(back.status_code(), 201);
        assert_eq!(back.header("x-h"), Some(&b"v"[..]));
        let items: Vec<BodyChunk> =
            futures::executor::block_on(back.take_stream().expect("stream kept").collect());
        assert!(matches!(&items[0], BodyChunk::Data(b) if &b[..] == b"a"));
        assert!(matches!(&items[1], BodyChunk::Trailers(t) if t.len() == 1));
    }
}
