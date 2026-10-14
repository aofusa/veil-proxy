//! veil の gRPC サーバのサンプル（F-197）。`.proto` から `veil-build` が生成したトレイトを実装し、
//! `add_demo` で `App` に登録する。ヘルスチェック（`grpc.health.v1.Health`）も載せる。
use futures::StreamExt;
use veil::api::grpc::{BoxStream, Code, Request, Response, Status, Streaming};
use veil::prelude::*;

pub mod demo {
    veil::include_proto!("demo.v1");
}
use demo::demo_server::{add_demo, Demo};
use demo::*;

#[cfg(unix)]
#[global_allocator]
static GLOBAL: veil::veil_alloc::VeilAlloc = veil::veil_alloc::VeilAlloc;

struct DemoService;

impl Demo for DemoService {
    async fn say_hello(&self, req: Request<HelloRequest>) -> Result<Response<HelloReply>, Status> {
        let caller = req
            .metadata_value("x-caller")
            .map(|v| String::from_utf8_lossy(v).into_owned());
        let name = req.into_inner().name;
        let mut resp = Response::new(HelloReply {
            message: format!("Hello {}", name),
        });
        if let Some(c) = caller {
            resp = resp.with_metadata("x-caller-echo", c);
        }
        Ok(resp.with_trailer("x-served-by", "veil"))
    }

    async fn count_up(
        &self,
        req: Request<CountRequest>,
    ) -> Result<Response<BoxStream<CountReply>>, Status> {
        let count = req.into_inner().count;
        if count > 100_000 {
            return Err(Status::invalid_argument("count must be <= 100000"));
        }
        let s = futures::stream::iter((1..=count).map(|n| Ok(CountReply { n })));
        Ok(Response::new(Box::pin(s)))
    }

    async fn sum(&self, req: Request<Streaming<Number>>) -> Result<Response<SumReply>, Status> {
        let mut s = req.into_inner();
        let (mut total, mut count) = (0i64, 0u32);
        while let Some(n) = s.message().await? {
            total += n.value;
            count += 1;
        }
        Ok(Response::new(SumReply { total, count }))
    }

    async fn chat(
        &self,
        req: Request<Streaming<ChatMessage>>,
    ) -> Result<Response<BoxStream<ChatMessage>>, Status> {
        let s = req.into_inner().map(|m| {
            m.map(|m| ChatMessage {
                text: m.text.to_uppercase(),
            })
        });
        Ok(Response::new(Box::pin(s)))
    }

    async fn sleep(&self, req: Request<SleepRequest>) -> Result<Response<Empty>, Status> {
        let ms = req.into_inner().millis.min(60_000);
        veil::runtime::time::sleep(std::time::Duration::from_millis(ms as u64)).await;
        Ok(Response::new(Empty {}))
    }

    async fn fail(&self, req: Request<FailRequest>) -> Result<Response<Empty>, Status> {
        let r = req.into_inner();
        let code = Code::from_u8(r.code as u8).unwrap_or(Code::Unknown);
        Err(Status::new(code, r.message))
    }
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    // 既定は平文 0.0.0.0:50051（h2c）。VEIL_LISTEN で平文、VEIL_TLS_LISTEN + 証明書で TLS（HTTP/2・HTTP/3）。
    let mut builder = Config::load()?
        .builder()
        .listen_cleartext(
            &std::env::var("VEIL_LISTEN").unwrap_or_else(|_| "127.0.0.1:50051".into()),
        )
        .openapi_enabled(false)
        // 1KB 以上の応答メッセージを gzip で圧縮する（クライアントが grpc-accept-encoding で受けるときだけ。F-209）
        .grpc_response_compression("gzip");
    if let (Ok(listen), Ok(cert), Ok(key)) = (
        std::env::var("VEIL_TLS_LISTEN"),
        std::env::var("VEIL_TLS_CERT"),
        std::env::var("VEIL_TLS_KEY"),
    ) {
        builder = builder.listen(&listen).tls(&cert, &key).http3(&listen);
    }
    let app = App::new().config(builder.build()).grpc_health();
    add_demo(app, DemoService).run()
}
