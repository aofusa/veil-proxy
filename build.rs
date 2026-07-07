//! veil のビルドスクリプト。
//!
//! `http3` フィーチャー有効時、quiche が要求する BoringSSL 互換（非プレフィックス）
//! シンボルを rustls と共有するため `AWS_LC_SYS_NO_PREFIX=1` を自動適用する。
//! libssl / libcrypto のリンクは aws-lc-sys（`http3` で `ssl` フィーチャー有効）が担う。

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_HTTP3");
    println!("cargo:rerun-if-env-changed=AWS_LC_SYS_NO_PREFIX");
    println!("cargo:rerun-if-env-changed=VEIL_SSL_NO_PREFIX_BOOTSTRAP");

    if !feature_enabled("HTTP3") {
        return;
    }

    ensure_aws_lc_no_prefix();
}

fn feature_enabled(name: &str) -> bool {
    std::env::var(format!("CARGO_FEATURE_{name}")).is_ok()
}

/// quiche は BoringSSL 互換の非プレフィックスシンボルを要求する。
/// 未設定時は cargo を `AWS_LC_SYS_NO_PREFIX=1` 付きで再実行し、
/// 呼び出し側での環境変数指定を不要にする。
fn ensure_aws_lc_no_prefix() {
    if std::env::var("AWS_LC_SYS_NO_PREFIX").ok().as_deref() == Some("1") {
        return;
    }

    if std::env::var("VEIL_SSL_NO_PREFIX_BOOTSTRAP").is_ok() {
        panic!(
            "http3 feature requires AWS_LC_SYS_NO_PREFIX=1 so quiche can share \
             aws-lc-sys with rustls, but bootstrap re-invocation did not apply it"
        );
    }

    eprintln!(
        "veil build.rs: applying AWS_LC_SYS_NO_PREFIX=1 for quiche/rustls AWS-LC sharing"
    );

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .env("AWS_LC_SYS_NO_PREFIX", "1")
        .env("VEIL_SSL_NO_PREFIX_BOOTSTRAP", "1")
        .args(std::env::args().skip(1))
        .status()
        .expect("failed to re-run cargo with AWS_LC_SYS_NO_PREFIX=1");

    std::process::exit(status.code().unwrap_or(1));
}