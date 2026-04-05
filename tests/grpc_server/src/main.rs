use tonic::{transport::Server, Request, Response, Status};
use futures_core::Stream;
use std::pin::Pin;

pub mod test_service {
    tonic::include_proto!("grpc.test.v1");
}

use test_service::test_service_server::{TestService, TestServiceServer};
use test_service::{SimpleRequest, SimpleResponse};

#[derive(Debug, Default)]
pub struct MyTestService {}

#[tonic::async_trait]
impl TestService for MyTestService {
    async fn test(&self, request: Request<SimpleRequest>) -> Result<Response<SimpleResponse>, Status> {
        let req = request.into_inner();
        let mut resp = Response::new(SimpleResponse { message: req.message });
        resp.metadata_mut().insert("x-server-id", "grpc-server".parse().unwrap());
        Ok(resp)
    }

    async fn unary_call(&self, request: Request<SimpleRequest>) -> Result<Response<SimpleResponse>, Status> {
        let req = request.into_inner();
        let mut resp = Response::new(SimpleResponse { message: req.message });
        resp.metadata_mut().insert("x-server-id", "grpc-server".parse().unwrap());
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
        resp.metadata_mut().insert("x-server-id", "grpc-server".parse().unwrap());
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
        resp.metadata_mut().insert("x-server-id", "grpc-server".parse().unwrap());
        Ok(resp)
    }

    type BidirectionalStreamingStream = Pin<Box<dyn Stream<Item = Result<SimpleResponse, Status>> + Send>>;

    async fn bidirectional_streaming(&self, mut request: Request<tonic::Streaming<SimpleRequest>>) -> Result<Response<Self::BidirectionalStreamingStream>, Status> {
        let mut stream = request.into_inner();
        let output = async_stream::try_stream! {
            while let Some(msg) = stream.message().await? {
                yield SimpleResponse { message: msg.message };
            }
        };
        let mut resp = Response::new(Box::pin(output) as Self::BidirectionalStreamingStream);
        resp.metadata_mut().insert("x-server-id", "grpc-server".parse().unwrap());
        Ok(resp)
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let addr = "127.0.0.1:9004".parse()?;
    let test_service = MyTestService::default();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    println!("gRPC Test Server listening on {}", addr);

    Server::builder()
        .add_service(TestServiceServer::new(test_service))
        .serve(addr)
        .await?;

    Ok(())
}
