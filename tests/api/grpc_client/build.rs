fn main() -> Result<(), Box<dyn std::error::Error>> {
    // サーバのサンプルと同じ .proto（クライアント側だけ生成する）
    tonic_build::configure()
        .build_server(false)
        .compile_protos(
            &["../../../examples/api/rust-grpc/proto/demo.proto"],
            &["../../../examples/api/rust-grpc/proto"],
        )?;
    Ok(())
}
