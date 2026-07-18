//! ngtcp2 をソースから静的ビルドし、aws-lc-sys（BoringSSL 互換）とリンクする。
//!
//! - 公式 https://github.com/ngtcp2/ngtcp2 のタグ `NGTCP2_VERSION` を使用
//! - `ENABLE_BORINGSSL=ON` + aws-lc-sys の include/lib を渡す
//! - 生成済み `src/bindings.rs` を使用（`overwrite` feature で再生成可）

use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

/// 固定バージョン（bindings.rs と同期すること）
const NGTCP2_VERSION: &str = "1.24.0";
const NGTCP2_GIT: &str = "https://github.com/ngtcp2/ngtcp2.git";

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=src/wrapper.h");
    println!("cargo:rerun-if-env-changed=NGTCP2_SOURCE_DIR");

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    let src_dir = resolve_source_dir(&out_dir);

    let (aws_include, ssl_lib, crypto_lib) = aws_lc_paths();

    // AWS_LC_SYS_NO_PREFIX=1 時、libcrypto は非プレフィックス記号を export するが
    // ヘッダの boringssl_prefix_symbols.h は既定で aws_lc_* プレフィックスにリネームする。
    // リネームを無効化し、ngtcp2_crypto_boringssl が unprefixed 記号を参照するようにする。
    let unprefix_flag = format!(
        "-I{} -DBORINGSSL_PREFIX_SYMBOLS_H -DBORINGSSL_PREFIX_SYMBOLS_ASM_H",
        aws_include
    );

    let dst = cmake::Config::new(&src_dir)
        .define("ENABLE_STATIC_LIB", "ON")
        .define("ENABLE_SHARED_LIB", "OFF")
        .define("ENABLE_LIB_ONLY", "ON")
        .define("BUILD_TESTING", "OFF")
        .define("ENABLE_OPENSSL", "OFF")
        .define("ENABLE_BORINGSSL", "ON")
        .define("BORINGSSL_INCLUDE_DIR", &aws_include)
        .define(
            "BORINGSSL_LIBRARIES",
            &format!(
                "{};{}",
                path_for_cmake(&ssl_lib),
                path_for_cmake(&crypto_lib)
            ),
        )
        .cflag(&unprefix_flag)
        .cxxflag(&unprefix_flag)
        .build();

    let lib_dir = if dst.join("lib64").exists() {
        dst.join("lib64")
    } else {
        dst.join("lib")
    };

    println!("cargo:rustc-link-search=native={}", lib_dir.display());
    println!("cargo:rustc-link-lib=static=ngtcp2");
    println!("cargo:rustc-link-lib=static=ngtcp2_crypto_boringssl");
    // aws-lc-sys 側が libssl / libcrypto をリンクする
    println!("cargo:include={}/include", dst.display());
    println!("cargo:root={}", dst.display());

    #[cfg(feature = "overwrite")]
    regenerate_bindings(&src_dir, &dst, &aws_include);
}

fn resolve_source_dir(out_dir: &Path) -> PathBuf {
    if let Ok(p) = env::var("NGTCP2_SOURCE_DIR") {
        let p = PathBuf::from(p);
        assert!(
            p.join("CMakeLists.txt").exists(),
            "NGTCP2_SOURCE_DIR invalid"
        );
        return p;
    }

    let dir = out_dir.join("ngtcp2-src");
    let tag = format!("v{NGTCP2_VERSION}");
    if !dir.join("CMakeLists.txt").exists() {
        run(
            Command::new("git").args([
                "clone",
                "--depth",
                "1",
                "--branch",
                &tag,
                NGTCP2_GIT,
                dir.to_str().expect("utf8 path"),
            ]),
            "git clone ngtcp2",
        );
    }
    dir
}

/// aws-lc-sys が export する DEP_*_INCLUDE から include / lib パスを得る。
///
/// `AWS_LC_SYS_NO_PREFIX=1` 時は `libssl.a` / `libcrypto.a`、
/// プレフィックス有効時は `libaws_lc_*_ssl.a` / `libaws_lc_*_crypto.a` になる。
fn aws_lc_paths() -> (String, PathBuf, PathBuf) {
    let mut include: Option<String> = None;
    let mut links_name: Option<String> = None;
    for (key, val) in env::vars() {
        if let Some(rest) = key.strip_prefix("DEP_") {
            if let Some(name) = rest.strip_suffix("_INCLUDE") {
                if name.starts_with("AWS_LC_") {
                    include = Some(val);
                    links_name = Some(name.to_lowercase());
                    break;
                }
            }
        }
    }
    let include = include.expect(
        "DEP_AWS_LC_*_INCLUDE not set — ngtcp2-sys requires aws-lc-sys with features=[\"ssl\"]",
    );
    let links = links_name.expect("links name");
    let out = PathBuf::from(&include)
        .parent()
        .expect("aws-lc include parent")
        .to_path_buf();
    let artifacts = out.join("build").join("artifacts");

    // NO_PREFIX=1 → libssl.a / libcrypto.a
    // プレフィックス付き → lib{links}_ssl.a / lib{links}_crypto.a
    let candidates_ssl = [
        artifacts.join("libssl.a"),
        artifacts.join(format!("lib{links}_ssl.a")),
    ];
    let candidates_crypto = [
        artifacts.join("libcrypto.a"),
        artifacts.join(format!("lib{links}_crypto.a")),
    ];
    let ssl = candidates_ssl
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "aws-lc ssl static lib not found under {}",
                artifacts.display()
            )
        });
    let crypto = candidates_crypto
        .into_iter()
        .find(|p| p.exists())
        .unwrap_or_else(|| {
            panic!(
                "aws-lc crypto static lib not found under {}",
                artifacts.display()
            )
        });
    (include, ssl, crypto)
}

fn path_for_cmake(p: &Path) -> String {
    p.to_str().expect("utf8").replace('\\', "/")
}

fn run(cmd: &mut Command, what: &str) {
    let status = cmd.status().unwrap_or_else(|e| panic!("{what}: {e}"));
    if !status.success() {
        panic!("{what} failed: {status}");
    }
}

#[cfg(feature = "overwrite")]
fn regenerate_bindings(src_dir: &Path, dst: &Path, aws_include: &str) {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let bindings = bindgen::Builder::default()
        .header(manifest.join("src/wrapper.h").to_str().unwrap())
        .clang_arg(format!("-I{}/include", dst.display()))
        .clang_arg(format!("-I{}/lib/includes", src_dir.display()))
        .clang_arg(format!("-I{}/crypto/includes", src_dir.display()))
        .clang_arg(format!("-I{aws_include}"))
        .allowlist_function("ngtcp2_.*")
        .allowlist_type("ngtcp2_.*")
        .allowlist_var("NGTCP2_.*")
        // SSL 型は aws-lc-sys と共有（ポインタ互換）
        .blocklist_type("SSL")
        .blocklist_type("SSL_CTX")
        .blocklist_type("ssl_st")
        .blocklist_type("ssl_ctx_st")
        .blocklist_type("ssl_encryption_level_t")
        .raw_line("/* generated by ngtcp2-sys build.rs --features overwrite */")
        .raw_line("pub use aws_lc_sys::{SSL, SSL_CTX};")
        // BoringSSL/aws-lc の encryption level 列挙（値は OpenSSL 互換）
        .raw_line("pub use aws_lc_sys::ssl_encryption_level_t;")
        .generate()
        .expect("bindgen ngtcp2");

    bindings
        .write_to_file(manifest.join("src/bindings.rs"))
        .expect("write bindings.rs");
    println!("cargo:warning=ngtcp2-sys: regenerated src/bindings.rs");
}
