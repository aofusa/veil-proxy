//! nghttp3 をソースから静的ビルドする。
//!
//! - 公式 https://github.com/ngtcp2/nghttp3 のタグ `NGHTTP3_VERSION` を使用
//! - 生成済み `src/bindings.rs` を使用（`overwrite` feature で再生成可）

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

const NGHTTP3_VERSION: &str = "1.17.0";
const NGHTTP3_GIT: &str = "https://github.com/ngtcp2/nghttp3.git";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/wrapper.h");
    println!("cargo:rerun-if-env-changed=NGHTTP3_SOURCE_DIR");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let src_dir = resolve_source_dir(&out_dir);

    // sfparse は nghttp3 の submodule
    run(
        Command::new("git").current_dir(&src_dir).args([
            "submodule",
            "update",
            "--init",
            "--depth",
            "1",
        ]),
        "git submodule update (sfparse)",
    );

    let dst = cmake::Config::new(&src_dir)
        .define("ENABLE_STATIC_LIB", "ON")
        .define("ENABLE_SHARED_LIB", "OFF")
        .define("ENABLE_LIB_ONLY", "ON")
        .define("BUILD_TESTING", "OFF")
        .build();

    let lib_dir = if dst.join("lib64").exists() {
        dst.join("lib64")
    } else {
        dst.join("lib")
    };

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=nghttp3");
    println!("cargo:include={}/include", dst.display());
    println!("cargo:root={}", dst.display());

    #[cfg(feature = "overwrite")]
    regenerate_bindings(&src_dir, &dst);
}

fn resolve_source_dir(out_dir: &Path) -> PathBuf {
    if let Ok(p) = env::var("NGHTTP3_SOURCE_DIR") {
        let p = PathBuf::from(p);
        assert!(
            p.join("CMakeLists.txt").exists(),
            "NGHTTP3_SOURCE_DIR invalid"
        );
        return p;
    }

    let dir = out_dir.join("nghttp3-src");
    let tag = format!("v{NGHTTP3_VERSION}");
    if !dir.join("CMakeLists.txt").exists() {
        run(
            Command::new("git").args([
                "clone",
                "--depth",
                "1",
                "--branch",
                &tag,
                NGHTTP3_GIT,
                dir.to_str().expect("utf8 path"),
            ]),
            "git clone nghttp3",
        );
    }
    dir
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd.status().unwrap_or_else(|e| panic!("{what}: {e}"));
    if !status.success() {
        panic!("{what} failed: {status}");
    }
}

#[cfg(feature = "overwrite")]
fn regenerate_bindings(src_dir: &Path, dst: &Path) {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let bindings = bindgen::Builder::default()
        .header(manifest.join("src/wrapper.h").to_str().unwrap())
        .clang_arg(format!("-I{}/include", dst.display()))
        .clang_arg(format!("-I{}/lib/includes", src_dir.display()))
        .allowlist_function("nghttp3_.*")
        .allowlist_type("nghttp3_.*")
        .allowlist_var("NGHTTP3_.*")
        .generate()
        .expect("bindgen nghttp3");

    bindings
        .write_to_file(manifest.join("src/bindings.rs"))
        .expect("write bindings.rs");
    println!("cargo:warning=nghttp3-sys: regenerated src/bindings.rs");
}
