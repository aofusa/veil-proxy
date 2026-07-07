//! veil のビルドスクリプト。
//!
//! `http3` フィーチャー有効時、quiche が要求する BoringSSL 互換（非プレフィックス）
//! シンボルを rustls と共有するため `AWS_LC_SYS_NO_PREFIX=1` を検証する。
//! libssl / libcrypto のリンクは aws-lc-sys（`http3` で `ssl` フィーチャー有効）が担う。

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_HTTP3");
    println!("cargo:rerun-if-env-changed=AWS_LC_SYS_NO_PREFIX");

    if !feature_enabled("HTTP3") {
        return;
    }

    if std::env::var("AWS_LC_SYS_NO_PREFIX").ok().as_deref() != Some("1") {
        panic!(
            "http3 feature requires AWS_LC_SYS_NO_PREFIX=1 so quiche can share \
             aws-lc-sys with rustls. Example: AWS_LC_SYS_NO_PREFIX=1 cargo build --features http3"
        );
    }
}

fn feature_enabled(name: &str) -> bool {
    std::env::var(format!("CARGO_FEATURE_{name}")).is_ok()
}