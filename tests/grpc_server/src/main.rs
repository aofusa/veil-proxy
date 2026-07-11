use tonic::{transport::Server, Request, Response, Status};
use futures_core::Stream;
use std::pin::Pin;

pub mod test_service {
    tonic::include_proto!("grpc.test.v1");
}

use test_service::test_service_server::{TestService, TestServiceServer};
use test_service::{SimpleRequest, SimpleResponse};

#[derive(Debug, Clone)]
pub struct MyTestService {
    /// レスポンスメタデータ `x-server-id`（Consistent Hash E2E 用）
    server_id: String,
}

impl MyTestService {
    fn new(server_id: String) -> Self {
        Self { server_id }
    }

    fn apply_server_id<T>(&self, resp: &mut Response<T>) {
        if let Ok(v) = self.server_id.parse() {
            resp.metadata_mut().insert("x-server-id", v);
        }
    }
}

#[tonic::async_trait]
impl TestService for MyTestService {
    async fn test(&self, request: Request<SimpleRequest>) -> Result<Response<SimpleResponse>, Status> {
        let req = request.into_inner();
        let mut resp = Response::new(SimpleResponse { message: req.message });
        self.apply_server_id(&mut resp);
        Ok(resp)
    }

    async fn unary_call(&self, request: Request<SimpleRequest>) -> Result<Response<SimpleResponse>, Status> {
        let req = request.into_inner();
        let mut resp = Response::new(SimpleResponse { message: req.message });
        self.apply_server_id(&mut resp);
        Ok(resp)
    }

    async fn stream_reset(&self, _request: Request<SimpleRequest>) -> Result<Response<SimpleResponse>, Status> {
        Err(Status::internal("Explicit reset for testing"))
    }

    type ServerStreamingStream = Pin<Box<dyn Stream<Item = Result<SimpleResponse, Status>> + Send>>;

    async fn server_streaming(&self, request: Request<SimpleRequest>) -> Result<Response<Self::ServerStreamingStream>, Status> {
        let req = request.into_inner();
        let output = async_stream::try_stream! {
            for i in 0..5 {
                yield SimpleResponse { message: format!("{}-{}", req.message, i) };
            }
        };
        let mut resp = Response::new(Box::pin(output) as Self::ServerStreamingStream);
        self.apply_server_id(&mut resp);
        Ok(resp)
    }

    async fn client_streaming(&self, mut request: Request<tonic::Streaming<SimpleRequest>>) -> Result<Response<SimpleResponse>, Status> {
        let mut count = 0;
        let mut last_msg = String::new();
        while let Some(msg) = request.get_mut().message().await? {
            count += 1;
            last_msg = msg.message;
        }
        let mut resp = Response::new(SimpleResponse { message: format!("Received {} messages, last: {}", count, last_msg) });
        self.apply_server_id(&mut resp);
        Ok(resp)
    }

    type BidirectionalStreamingStream = Pin<Box<dyn Stream<Item = Result<SimpleResponse, Status>> + Send>>;

    async fn bidirectional_streaming(&self, request: Request<tonic::Streaming<SimpleRequest>>) -> Result<Response<Self::BidirectionalStreamingStream>, Status> {
        let mut stream = request.into_inner();
        let output = async_stream::try_stream! {
            while let Some(msg) = stream.message().await? {
                yield SimpleResponse { message: msg.message };
            }
        };
        let mut resp = Response::new(Box::pin(output) as Self::BidirectionalStreamingStream);
        self.apply_server_id(&mut resp);
        Ok(resp)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // F-97: GRPC_LISTEN_ADDR / GRPC_SERVER_ID で複数インスタンス起動可能
    let addr_str =
        std::env::var("GRPC_LISTEN_ADDR").unwrap_or_else(|_| "127.0.0.1:9004".to_string());
    let server_id =
        std::env::var("GRPC_SERVER_ID").unwrap_or_else(|_| "grpc-server".to_string());
    let addr: std::net::SocketAddr = addr_str.parse()?;
    let test_service = MyTestService::new(server_id.clone());

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // gRPC Health Checking Protocol（E2E の grpc ヘルスチェック検証用）
    let (mut health_reporter, health_service) = tonic_health::server::health_reporter();
    // GRPC_HEALTH_NOT_SERVING=1 で NOT_SERVING を返す（フェイルオーバー E2E 用）
    if std::env::var("GRPC_HEALTH_NOT_SERVING").ok().as_deref() == Some("1") {
        health_reporter
            .set_not_serving::<TestServiceServer<MyTestService>>()
            .await;
    } else {
        health_reporter
            .set_serving::<TestServiceServer<MyTestService>>()
            .await;
    }

    println!(
        "gRPC Test Server listening on {} (server_id={})",
        addr, server_id
    );

    Server::builder()
        .add_service(health_service)
        .add_service(TestServiceServer::new(test_service))
        .serve(addr)
        .await?;

    Ok(())
}
