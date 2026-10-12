//! GraphQL（async-graphql、F-191、`graphql` feature）。
//!
//! `App::graphql(path, schema)` で GraphQL over HTTP（POST: `application/json`、GET: クエリ文字列）を
//! 公開する。GET でブラウザ（`Accept: text/html`）なら GraphiQL を返す。async-graphql は実行時
//! ランタイムに依存しないので、veil のワーカー上で直接 await する。サブスクリプションは非対応。

use super::app::App;
use super::handler::{HandlerFn, OperationMeta};
use super::request::Request;
use super::response::Response;
use async_graphql::{ObjectType, Schema, SubscriptionType};
use std::sync::Arc;

/// GraphQL 応答の Content-Type（GraphQL over HTTP 仕様）。
const GRAPHQL_RESPONSE: &str = "application/graphql-response+json";

impl App {
    /// `path` に GraphQL エンドポイントを公開する（GET・POST）。
    pub fn graphql<Q, M, S>(self, path: &str, schema: Schema<Q, M, S>) -> App
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
        self.route_fn(&name, &["GET", "POST"], path, func, meta)
    }
}

async fn execute<Q, M, S>(schema: &Schema<Q, M, S>, endpoint: &str, req: Request) -> Response
where
    Q: ObjectType + 'static,
    M: ObjectType + 'static,
    S: SubscriptionType + 'static,
{
    let gql_req = match req.method() {
        "GET" | "HEAD" => {
            if req.query().is_empty()
                && req
                    .header("accept")
                    .map(|a| a.contains("text/html"))
                    .unwrap_or(false)
            {
                let html = async_graphql::http::GraphiQLSource::build()
                    .endpoint(endpoint)
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
    let resp = schema.execute_batch(gql_req).await;
    let ct = if req
        .header("accept")
        .map(|a| a.contains(GRAPHQL_RESPONSE))
        .unwrap_or(false)
    {
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
