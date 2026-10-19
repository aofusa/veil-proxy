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
    let mut out = build_http_request(&req);
    out.extensions_mut().insert(req);
    out
}

/// `http::Request` を作る（extensions には何も入れない）。F-235: 要求のバイト列を複製しない。
/// - URI はクエリが無ければパスの `Bytes` をそのまま使い、あれば 1 回だけ連結する。
/// - ヘッダの値は `HeaderValue::from_maybe_shared` で受信バッファの `Bytes` を共有する
///   （名前は標準のものなら静的。`HeaderMap` は数で容量を取る）。
fn build_http_request(req: &Request) -> http::Request<VeilBody> {
    let mut out = http::Request::new(VeilBody::new(req.body.clone()));
    if let Ok(m) = http::Method::from_bytes(&req.method) {
        *out.method_mut() = m;
    }
    let target = if req.query.is_empty() {
        req.path.clone()
    } else {
        let mut t = bytes::BytesMut::with_capacity(req.path.len() + 1 + req.query.len());
        t.extend_from_slice(&req.path);
        t.extend_from_slice(b"?");
        t.extend_from_slice(&req.query);
        t.freeze()
    };
    if let Ok(u) = http::Uri::from_maybe_shared(target) {
        *out.uri_mut() = u;
    }
    *out.version_mut() = match req.version {
        super::request::HttpVersion::Http11 => http::Version::HTTP_11,
        super::request::HttpVersion::Http2 => http::Version::HTTP_2,
        super::request::HttpVersion::Http3 => http::Version::HTTP_3,
    };
    let headers = out.headers_mut();
    headers.reserve(req.headers.len());
    for (n, v) in req.headers.iter() {
        if let (Ok(n), Ok(v)) = (
            http::HeaderName::from_bytes(n),
            http::HeaderValue::from_maybe_shared(v.clone()),
        ) {
            headers.append(n, v);
        }
    }
    out
}

/// よく使うヘッダの名前は静的な `Bytes` にする（`http` 1.x には `HeaderName` を `Bytes` へ移す手段が無いため、
/// 表で引く。表に無い名前だけ複製する）。
fn header_name_bytes(n: &http::HeaderName) -> Bytes {
    use http::header::*;
    static COMMON: &[(HeaderName, &[u8])] = &[
        (CONTENT_TYPE, b"content-type"),
        (CONTENT_LENGTH, b"content-length"),
        (CONTENT_ENCODING, b"content-encoding"),
        (CACHE_CONTROL, b"cache-control"),
        (DATE, b"date"),
        (ETAG, b"etag"),
        (LAST_MODIFIED, b"last-modified"),
        (LOCATION, b"location"),
        (SET_COOKIE, b"set-cookie"),
        (VARY, b"vary"),
        (SERVER, b"server"),
        (ACCESS_CONTROL_ALLOW_ORIGIN, b"access-control-allow-origin"),
        (HOST, b"host"),
        (USER_AGENT, b"user-agent"),
        (ACCEPT, b"accept"),
        (ACCEPT_ENCODING, b"accept-encoding"),
        (AUTHORIZATION, b"authorization"),
        (COOKIE, b"cookie"),
        (CONNECTION, b"connection"),
    ];
    for (k, v) in COMMON {
        if k == n {
            return Bytes::from_static(v);
        }
    }
    Bytes::copy_from_slice(n.as_str().as_bytes())
}

/// `HeaderMap` を veil のヘッダの列にする（F-235: よく使う名前は複製しない。値は 1 回だけ複製する）。
fn headers_into_vec(map: http::HeaderMap, out: &mut Vec<(Bytes, Bytes)>) {
    out.reserve(map.len());
    let mut last: Option<Bytes> = None;
    for (n, v) in map {
        // 同じ名前が続くときは None（直前の名前を使う）
        let name = match n {
            Some(n) => {
                let b = header_name_bytes(&n);
                last = Some(b.clone());
                b
            }
            None => match &last {
                Some(b) => b.clone(),
                None => continue,
            },
        };
        out.push((name, Bytes::copy_from_slice(v.as_bytes())));
    }
}

/// 本文をまとめて読む。F-235: データのフレームが 1 つ（`Full` 等）ならその `Bytes` をそのまま返し、
/// 2 つ以上のときだけ連結する（`collect()` は 1 フレームでも内部のキューを確保していた）。
async fn body_bytes<B>(body: B) -> Result<Bytes, B::Error>
where
    B: http_body::Body<Data = Bytes>,
{
    let mut body = std::pin::pin!(body);
    let mut first: Option<Bytes> = None;
    let mut rest: Option<bytes::BytesMut> = None;
    while let Some(frame) = body.frame().await {
        let Ok(data) = frame?.into_data() else {
            continue; // トレイラーは捨てる（collect().to_bytes() と同じ）
        };
        if data.is_empty() {
            continue;
        }
        match (&mut first, &mut rest) {
            (None, _) => first = Some(data),
            (Some(f), None) => {
                let mut m = bytes::BytesMut::with_capacity(f.len() + data.len());
                m.extend_from_slice(f);
                m.extend_from_slice(&data);
                rest = Some(m);
            }
            (Some(_), Some(m)) => m.extend_from_slice(&data),
        }
    }
    Ok(match (first, rest) {
        (_, Some(m)) => m.freeze(),
        (Some(f), None) => f,
        (None, None) => Bytes::new(),
    })
}

/// `http::Request` を veil の `Request` に戻す（層が書き換えたメソッド・URI・ヘッダ・本文を反映する）。
pub async fn from_http_request(req: http::Request<VeilBody>) -> Request {
    let (parts, body) = req.into_parts();
    let body = body_bytes(body).await.unwrap_or_default();
    let orig = parts.extensions.get::<Request>().cloned();
    let target = parts
        .uri
        .path_and_query()
        .map(|p| p.as_str().to_string())
        .unwrap_or_else(|| "/".to_string());
    let mut headers = Vec::new();
    headers_into_vec(parts.headers, &mut headers);
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
        headers_into_vec(parts.headers, &mut r.headers);
        return r;
    }
    let body = match body_bytes(body).await {
        Ok(b) => b,
        Err(e) => return Response::error(502, &format!("response body error: {}", e)),
    };
    let mut r = Response::new(parts.status.as_u16()).with_body(body);
    headers_into_vec(parts.headers, &mut r.headers);
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
                // F-235: マウントしたサービス（axum 等）は veil の Request を読まないので extensions に入れない
                // （入れると Box と extensions の表で要求ごとに 3 回確保していた）
                let http_req = build_http_request(&req);
                drop(req);
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
