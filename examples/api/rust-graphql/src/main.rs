//! GraphQL（async-graphql）を veil の API ライブラリに載せるサンプル（F-207）。
//!
//! - 問い合わせ: `add`・`greeting`・`count`
//! - 変更: `increment`（共有のカウンタ）
//! - サブスクリプション: `countdown(from)`（GraphQL over SSE。`Accept: text/event-stream` の POST）
//!
//! ```sh
//! curl -X POST localhost:8080/graphql -H 'content-type: application/json' -d '{"query":"{ add(a:1,b:2) }"}'
//! curl -N -X POST localhost:8080/graphql -H 'accept: text/event-stream' -H 'content-type: application/json' \
//!      -d '{"query":"subscription { countdown(from: 3) }"}'
//! ```

mod graphql;

use async_graphql::{Context, Object, Schema, Subscription};
use futures::Stream;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;
use veil::api::prelude::*;

struct Counter(AtomicI64);

struct QueryRoot;

#[Object]
impl QueryRoot {
    /// 2 つの数の和
    async fn add(&self, a: i32, b: i32) -> i32 {
        a + b
    }
    async fn greeting(&self, name: String) -> String {
        format!("Hello, {}!", name)
    }
    async fn count(&self, ctx: &Context<'_>) -> i64 {
        ctx.data_unchecked::<Counter>().0.load(Ordering::Relaxed)
    }
}

struct MutationRoot;

#[Object]
impl MutationRoot {
    /// カウンタを `by` 増やして新しい値を返す
    async fn increment(&self, ctx: &Context<'_>, by: i64) -> i64 {
        ctx.data_unchecked::<Counter>()
            .0
            .fetch_add(by, Ordering::Relaxed)
            + by
    }
}

struct SubscriptionRoot;

#[Subscription]
impl SubscriptionRoot {
    /// `from` から 0 まで 10ms ごとに数える（veil のタイマーで待つ）
    async fn countdown(&self, from: i32) -> impl Stream<Item = i32> {
        futures::stream::unfold(from, |n| async move {
            if n < 0 {
                return None;
            }
            veil::tokio::time::sleep(Duration::from_millis(10)).await;
            Some((n, n - 1))
        })
    }
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    let mut builder = Config::load()?.builder().openapi_title("veil graphql");
    if let Ok(listen) = std::env::var("VEIL_LISTEN") {
        builder = builder.listen_cleartext(&listen);
    }
    let schema = Schema::build(QueryRoot, MutationRoot, SubscriptionRoot)
        .data(Counter(AtomicI64::new(0)))
        .finish();
    let app = App::new()
        .config(builder.build())
        .get("/health", |_req: Request| async { "ok" });
    graphql::graphql(app, "/graphql", schema).run()
}
