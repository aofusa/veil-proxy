//! The API server the benchmarks of `tools/perf/` drive (F-295): the same handler shapes as the HttpArena `veil-api`
//! entry (`/baseline11`, `/json/{count}`, unary gRPC), in one process, on the veil API library.
//!
//! `veil-api-app [addr] [threads]`; `BENCH_COMPRESSION=0` turns `[api] compression` off (serialization only).
use serde::Serialize;
use veil::api::grpc::{Request as GrpcRequest, Response as GrpcResponse, Status};
use veil::prelude::*;

pub mod benchmark {
    veil::include_proto!("benchmark");
}
use benchmark::benchmark_service_server::{add_benchmark_service, BenchmarkService};
use benchmark::{SumReply, SumRequest};

#[cfg(all(unix, not(feature = "sysalloc")))]
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;

#[derive(Serialize, Clone)]
struct Rating {
    score: i64,
    count: i64,
}

#[derive(Serialize, Clone)]
struct Item {
    id: i64,
    name: String,
    category: String,
    price: i64,
    quantity: i64,
    active: bool,
    tags: Vec<String>,
    rating: Rating,
}

#[derive(Serialize)]
struct PricedItem<'a> {
    #[serde(flatten)]
    item: &'a Item,
    total: i64,
}

#[derive(Serialize)]
struct Items<'a> {
    items: Vec<PricedItem<'a>>,
    count: usize,
}

static DATASET: std::sync::OnceLock<Vec<Item>> = std::sync::OnceLock::new();

// HttpArena の dataset.json と同じ形（50 件）を決まった擬似乱数で作る（圧縮の比率が同じくらいになるように）
fn make_dataset() -> Vec<Item> {
    const ADJ: [&str; 10] = ["Alpha", "Pro", "Ultra", "Nano", "Prime", "Core", "Flex", "Edge", "Swift", "Nova"];
    const NOUN: [&str; 10] = ["Widget", "Gadget", "Module", "Sensor", "Driver", "Relay", "Switch", "Valve", "Gear", "Link"];
    const CAT: [&str; 8] = ["electronics", "tools", "clothing", "food", "sports", "books", "toys", "health"];
    const TAG: [&str; 10] = ["fast", "new", "popular", "sale", "limited", "premium", "eco", "compact", "heavy-duty", "wireless"];
    let mut s: u64 = 0x2545_f491_4f6c_dd1d;
    let mut next = move |n: u64| {
        s ^= s << 13;
        s ^= s >> 7;
        s ^= s << 17;
        s % n
    };
    (0..50)
        .map(|i| {
            let tags = (0..1 + next(4)).map(|_| TAG[next(10) as usize].to_string()).collect();
            Item {
                id: i + 1,
                name: format!("{} {}", ADJ[next(10) as usize], NOUN[next(10) as usize]),
                category: CAT[next(8) as usize].to_string(),
                price: 1 + next(500) as i64,
                quantity: 1 + next(100) as i64,
                active: next(4) != 0,
                tags,
                rating: Rating {
                    score: 1 + next(50) as i64,
                    count: 1 + next(500) as i64,
                },
            }
        })
        .collect()
}

fn dataset() -> &'static [Item] {
    DATASET.get().map(Vec::as_slice).unwrap_or(&[])
}

#[veil::route(path = "/baseline11", method = "GET")]
async fn baseline11(a: i64, b: i64) -> String {
    (a + b).to_string()
}

#[veil::route(path = "/plaintext", method = "GET")]
async fn plaintext() -> &'static str {
    "Hello, World!"
}

#[veil::route(path = "/json/{count}", method = "GET")]
async fn json(count: usize, m: Option<i64>) -> Response {
    let m = m.unwrap_or(1);
    let items: Vec<PricedItem> = dataset()
        .iter()
        .take(count)
        .map(|item| PricedItem {
            item,
            total: item.price * item.quantity * m,
        })
        .collect();
    Response::json(&Items {
        count: items.len(),
        items,
    })
}

struct Benchmark;

impl BenchmarkService for Benchmark {
    async fn get_sum(&self, req: GrpcRequest<SumRequest>) -> Result<GrpcResponse<SumReply>, Status> {
        let r = req.into_inner();
        Ok(GrpcResponse::new(SumReply { result: r.a + r.b }))
    }
}

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    let _ = DATASET.set(make_dataset());
    let addr = std::env::args().nth(1).unwrap_or_else(|| "127.0.0.1:3000".to_string());
    let threads: usize = std::env::args().nth(2).and_then(|t| t.parse().ok()).unwrap_or(0);
    let compression = env_or("BENCH_COMPRESSION", "1") == "1";
    let mut config = Config::new()
        .builder()
        .threads(threads)
        .log_level("warn")
        .openapi_enabled(false)
        .listen_cleartext(&addr)
        .build();
    config.set_override(&format!("api.compression = {}", compression))?;
    config.set_override("access_log.enabled = false")?;
    add_benchmark_service(App::new().config(config), Benchmark).run()
}
