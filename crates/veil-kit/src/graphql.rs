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
//! - `POST` (`application/json`, batches too) and `GET` (query string): GraphQL over HTTP. The response type is
//!   `application/graphql-response+json` when the client accepts it, `application/json` otherwise.
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
