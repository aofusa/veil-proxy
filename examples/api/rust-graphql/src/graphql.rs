//! async-graphql のスキーマを veil の API ライブラリに載せる橋渡し（F-207）。
//!
//! veil は GraphQL のライブラリに依存しない。この 1 ファイルを自分のプロジェクトにコピーして使う
//! （juniper 等の別の実装でも、`Request` から問い合わせを読み、結果を `Response` で返す同じ形で書ける）。
//!
//! - `POST`（`application/json`、バッチも可）と `GET`（クエリ文字列）: GraphQL over HTTP。応答の Content-Type は
//!   `Accept` に `application/graphql-response+json` があればそれ、無ければ `application/json`。
//! - ブラウザからの `GET`（`Accept: text/html`、クエリ無し）: GraphiQL。
//! - `Accept: text/event-stream` の `POST` / `GET`: GraphQL over SSE（distinct connections モード）。
//!   サブスクリプションは `event: next` を結果ごとに送り、最後に `event: complete`。問い合わせ・変更も 1 件送って終わる。

use async_graphql::{ObjectType, Schema, SubscriptionType};
use bytes::Bytes;
use futures::StreamExt;
use std::sync::Arc;
use veil::api::{App, BodyChunk, HandlerFn, OperationMeta, Request, Response};

/// GraphQL 応答の Content-Type（GraphQL over HTTP 仕様）。
const GRAPHQL_RESPONSE: &str = "application/graphql-response+json";

/// `path` に GraphQL エンドポイントを公開する（GET・POST）。
pub fn graphql<Q, M, S>(app: App, path: &str, schema: Schema<Q, M, S>) -> App
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let endpoint = path.to_string();
    let func: HandlerFn = Arc::new(move |req: Request| {
        let schema = schema.clone();
        let endpoint = endpoint.clone();
        Box::pin(async move { execute(&schema, &endpoint, req).await })
    });
    let name = format!("graphql{}", path.replace(['/', '{', '}', '*'], "_"));
    let meta = OperationMeta {
        operation_id: name.clone(),
        summary: Some("GraphQL endpoint".into()),
        tags: vec!["graphql".into()],
        ..Default::default()
    };
    app.route_fn(&name, &["GET", "POST"], path, func, meta)
}

fn accepts(req: &Request, media: &str) -> bool {
    req.header("accept")
        .map(|a| a.contains(media))
        .unwrap_or(false)
}

async fn execute<Q, M, S>(schema: &Schema<Q, M, S>, endpoint: &str, req: Request) -> Response
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let gql_req = match req.method() {
        "GET" | "HEAD" => {
            if req.query().is_empty() && accepts(&req, "text/html") {
                let html = async_graphql::http::GraphiQLSource::build()
                    .endpoint(endpoint)
                    .subscription_endpoint(endpoint)
                    .finish();
                return Response::html(html);
            }
            match async_graphql::http::parse_query_string(req.query()) {
                Ok(r) => async_graphql::BatchRequest::Single(r),
                Err(e) => return Response::bad_request(&e.to_string()),
            }
        }
        "POST" => match serde_json::from_slice::<async_graphql::BatchRequest>(req.body()) {
            Ok(r) => r,
            Err(e) => return Response::bad_request(&format!("invalid GraphQL request: {}", e)),
        },
        _ => return Response::error(405, "Method Not Allowed"),
    };

    if accepts(&req, "text/event-stream") {
        let single = match gql_req.into_single() {
            Ok(r) => r,
            Err(e) => return Response::bad_request(&e.to_string()),
        };
        return sse(schema.execute_stream(single));
    }

    let resp = schema.execute_batch(gql_req).await;
    let ct = if accepts(&req, GRAPHQL_RESPONSE) {
        GRAPHQL_RESPONSE
    } else {
        "application/json"
    };
    match serde_json::to_vec(&resp) {
        Ok(body) => Response::new(200)
            .with_header("content-type", ct)
            .with_body(body),
        Err(e) => Response::error(500, &e.to_string()),
    }
}

/// 結果の列を GraphQL over SSE（`event: next` … `event: complete`）で送る。
fn sse<St>(results: St) -> Response
where
    St: futures::Stream<Item = async_graphql::Response> + Send + 'static,
{
    let events = results
        .map(|r| {
            let json = serde_json::to_string(&r).unwrap_or_else(|_| "{}".into());
            BodyChunk::Data(Bytes::from(format!("event: next\ndata: {}\n\n", json)))
        })
        .chain(futures::stream::once(async {
            BodyChunk::Data(Bytes::from_static(b"event: complete\ndata:\n\n"))
        }));
    Response::stream(200, events)
        .with_header("content-type", "text/event-stream")
        .with_header("cache-control", "no-cache")
}
