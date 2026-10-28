// F-267: async-graphql を veil のハンドラに載せる（examples/api/rust-graphql の橋渡し（F-207）を取り込んだもの）。
// WebSocket のサブスクリプションは F-262 の WebSocket の上の graphql-transport-ws / graphql-ws。
//! GraphQL with [async-graphql](https://docs.rs/async-graphql) 7.
//!
//! ```no_run
//! use async_graphql::{EmptyMutation, EmptySubscription, Object, Schema};
//! use veil::prelude::*;
//!
//! struct Query;
//! #[Object]
//! impl Query {
//!     async fn add(&self, a: i32, b: i32) -> i32 { a + b }
//! }
//!
//! let schema = Schema::new(Query, EmptyMutation, EmptySubscription);
//! App::new().with(veil_kit::graphql::mount("/graphql", schema)).run().unwrap();
//! ```
//!
//! One path serves:
//!
//! - `POST` (`Content-Type: application/json`, batches too; another type gives 415) and `GET` (query string; queries
//!   and subscriptions only, a mutation gives 405 so that a cross-site link cannot run one): GraphQL over HTTP. The
//!   response type is `application/graphql-response+json` when the client accepts it, `application/json` otherwise.
//! - `Accept: text/event-stream`: GraphQL over SSE (distinct connections mode): `event: next` per result, then
//!   `event: complete`.
//! - A WebSocket upgrade with `Sec-WebSocket-Protocol: graphql-transport-ws` (or the older `graphql-ws`):
//!   subscriptions over WebSocket (HTTP/1.1).
//! - A browser `GET` (`Accept: text/html`, no query): GraphiQL (`graphiql` feature). The page loads its scripts from
//!   a CDN.

use async_graphql::http::{WebSocketProtocols, WsMessage};
use async_graphql::{ObjectType, Schema, SubscriptionType};
use bytes::Bytes;
use futures::StreamExt;
use std::sync::Arc;
use veil::api::ws::{Message, WebSocketUpgrade};
use veil::api::{App, BodyChunk, FromRequest, HandlerFn, OperationMeta, Request, Response};

/// The GraphQL over HTTP media type.
const GRAPHQL_RESPONSE: &str = "application/graphql-response+json";

/// A plugin that serves `schema` at `path` (`GET` and `POST`).
pub fn mount<Q, M, S>(path: &str, schema: Schema<Q, M, S>) -> impl FnOnce(App) -> App
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let path = path.to_string();
    move |app: App| {
        let endpoint: Arc<str> = Arc::from(path.as_str());
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
        app.route_fn(&name, &["GET", "POST"], &path, func, meta)
    }
}

/// Whether the `Content-Type` is JSON (`application/json` or the GraphQL media types, with any parameters).
fn json_content_type(ct: Option<&str>) -> bool {
    let Some(ct) = ct else { return false };
    let media = ct.split(';').next().unwrap_or("").trim();
    [
        "application/json",
        GRAPHQL_RESPONSE,
        "application/graphql+json",
    ]
    .iter()
    .any(|m| media.eq_ignore_ascii_case(m))
}

/// Whether the operation a `GET` request would run is a mutation (a document that does not parse is left to the
/// executor, which reports the error).
fn is_mutation(r: &mut async_graphql::Request) -> bool {
    use async_graphql::parser::types::{DocumentOperations, OperationType};
    let name = r.operation_name.clone();
    let Ok(doc) = r.parsed_query() else {
        return false;
    };
    match &doc.operations {
        DocumentOperations::Single(op) => op.node.ty == OperationType::Mutation,
        DocumentOperations::Multiple(ops) => ops
            .iter()
            .filter(|(n, _)| name.as_deref().is_none_or(|want| n.as_str() == want))
            .any(|(_, op)| op.node.ty == OperationType::Mutation),
    }
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
    if req
        .upgrade_protocol()
        .is_some_and(|p| p.eq_ignore_ascii_case(b"websocket"))
    {
        return websocket(schema.clone(), &req);
    }
    let gql_req = match req.method() {
        "GET" | "HEAD" => {
            if req.query().is_empty() && accepts(&req, "text/html") {
                return graphiql(endpoint);
            }
            match async_graphql::http::parse_query_string(req.query()) {
                Ok(mut r) => {
                    // B-196: GET で mutation を実行しない（Lax の Cookie はトップレベルの GET 遷移に付く）
                    if is_mutation(&mut r) {
                        return Response::error(405, "mutations require POST")
                            .with_header("allow", "POST");
                    }
                    async_graphql::BatchRequest::Single(r)
                }
                Err(e) => return Response::bad_request(&e.to_string()),
            }
        }
        "POST" => {
            // B-196: JSON 以外の Content-Type（text/plain のフォーム等、CORS の事前確認の要らない要求）は受けない
            if !json_content_type(req.header("content-type")) {
                return Response::error(415, "GraphQL requests must be application/json");
            }
            match serde_json::from_slice::<async_graphql::BatchRequest>(req.body()) {
                Ok(r) => r,
                Err(e) => return Response::bad_request(&format!("invalid GraphQL request: {}", e)),
            }
        }
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

#[cfg(feature = "graphiql")]
fn graphiql(endpoint: &str) -> Response {
    let html = async_graphql::http::GraphiQLSource::build()
        .endpoint(endpoint)
        .subscription_endpoint(endpoint)
        .finish();
    Response::html(html)
}

#[cfg(not(feature = "graphiql"))]
fn graphiql(_endpoint: &str) -> Response {
    Response::error(
        404,
        "GraphiQL is not enabled (veil-proxy-kit feature \"graphiql\")",
    )
}

/// Sends a stream of results as GraphQL over SSE.
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

/// GraphQL subscriptions over WebSocket.
fn websocket<Q, M, S>(schema: Schema<Q, M, S>, req: &Request) -> Response
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let up = match WebSocketUpgrade::from_request(req) {
        Ok(u) => u.protocols(async_graphql::http::ALL_WEBSOCKET_PROTOCOLS),
        Err(r) => return r,
    };
    let protocol: WebSocketProtocols = match up.selected_protocol().map(str::parse) {
        Some(Ok(p)) => p,
        _ => {
            return Response::bad_request(
                "GraphQL over WebSocket needs Sec-WebSocket-Protocol graphql-transport-ws or graphql-ws",
            )
        }
    };
    up.on_upgrade(move |ws| async move {
        let (tx, rx) = ws.split();
        let incoming = futures::stream::unfold(rx, |mut rx| async move {
            loop {
                match rx.recv().await {
                    Some(Ok(Message::Text(t))) => return Some((t.into_bytes(), rx)),
                    Some(Ok(Message::Binary(b))) => return Some((b, rx)),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => return None,
                    Some(Ok(_)) => continue,
                }
            }
        });
        let mut out = std::pin::pin!(async_graphql::http::WebSocket::new(
            schema,
            Box::pin(incoming),
            protocol
        ));
        while let Some(m) = out.next().await {
            let r = match m {
                WsMessage::Text(t) => tx.send(Message::text(t)),
                WsMessage::Close(code, reason) => {
                    let _ = tx.send(Message::Close(Some(veil::api::ws::CloseFrame {
                        code,
                        reason: reason.into(),
                    })));
                    break;
                }
            };
            if r.is_err() {
                break;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_graphql::{EmptySubscription, Object};

    struct Query;
    #[Object]
    impl Query {
        async fn add(&self, a: i32, b: i32) -> i32 {
            a + b
        }
    }

    struct Mutation;
    #[Object]
    impl Mutation {
        async fn delete_account(&self) -> bool {
            true
        }
    }

    fn run(method: &str, target: &str, ct: Option<&str>, body: &str) -> Response {
        let schema = Schema::new(Query, Mutation, EmptySubscription);
        let headers = ct
            .map(|c| {
                vec![(
                    Bytes::from_static(b"content-type"),
                    Bytes::from(c.to_string()),
                )]
            })
            .unwrap_or_default();
        let req = Request::new(
            method.to_string(),
            target.to_string(),
            headers,
            body.to_string(),
        );
        futures::executor::block_on(execute(&schema, "/graphql", req))
    }

    // B-196: GET は query だけ、POST は JSON の Content-Type だけ
    #[test]
    fn get_runs_queries_only() {
        let r = run("GET", "/graphql?query=%7Badd(a%3A1%2Cb%3A2)%7D", None, "");
        assert_eq!(r.status_code(), 200);
        assert!(String::from_utf8_lossy(r.body()).contains("3"));
        let r = run(
            "GET",
            "/graphql?query=mutation%7BdeleteAccount%7D",
            None,
            "",
        );
        assert_eq!(r.status_code(), 405);
        assert_eq!(r.header("allow"), Some(&b"POST"[..]));
        // 名前付きの複数の操作: 選んだものの種類で決める（async-graphql の GET の解析は `operation_name` を読む。
        // `operationName` は読まれず、選ばないと mutation を含む文書は 405）
        let doc = "query%20Q%7Badd(a%3A1%2Cb%3A1)%7D%20mutation%20M%7BdeleteAccount%7D";
        let r = run(
            "GET",
            &format!("/graphql?query={doc}&operation_name=Q"),
            None,
            "",
        );
        assert_eq!(
            r.status_code(),
            200,
            "{}",
            String::from_utf8_lossy(r.body())
        );
        let r = run(
            "GET",
            &format!("/graphql?query={doc}&operation_name=M"),
            None,
            "",
        );
        assert_eq!(r.status_code(), 405);
        let r = run("GET", &format!("/graphql?query={doc}"), None, "");
        assert_eq!(r.status_code(), 405);
        let r = run(
            "GET",
            &format!("/graphql?query={doc}&operationName=Q"),
            None,
            "",
        );
        assert_eq!(r.status_code(), 405);
    }

    #[test]
    fn post_requires_json() {
        let body = r#"{"query":"mutation{deleteAccount}"}"#;
        assert_eq!(
            run("POST", "/graphql", Some("application/json"), body).status_code(),
            200
        );
        assert_eq!(
            run(
                "POST",
                "/graphql",
                Some("Application/JSON; charset=utf-8"),
                body
            )
            .status_code(),
            200
        );
        assert_eq!(
            run("POST", "/graphql", Some("text/plain"), body).status_code(),
            415
        );
        assert_eq!(
            run(
                "POST",
                "/graphql",
                Some("application/x-www-form-urlencoded"),
                body
            )
            .status_code(),
            415
        );
        assert_eq!(run("POST", "/graphql", None, body).status_code(), 415);
    }
}
