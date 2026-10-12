fn main() -> Result<(), Box<dyn std::error::Error>> {
    tonic_build::configure()
        .build_client(false)
        .compile_protos(&["../../../proto/veil/vds/v1/vds.proto"], &["../../../proto"])?;
    Ok(())
}
