fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=proto/kit.proto");
    // サーバ（kit_server）とクライアント（kit_client）の両方を生成する（build_client は既定で true）
    veil_build::configure()
        .build_client(true)
        .compile_protos(&["proto/kit.proto"], &["proto"])
}
