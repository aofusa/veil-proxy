fn main() -> std::io::Result<()> {
    println!("cargo:rerun-if-changed=proto/demo.proto");
    veil_build::compile_protos(&["proto/demo.proto"], &["proto"])
}
