//! veil のビルドスクリプト。
//!
//! `http3` フィーチャー有効時、quiche が要求する BoringSSL 互換（非プレフィックス）
//! シンボルを rustls と共有するため `AWS_LC_SYS_NO_PREFIX=1` を自動適用する。
//! libssl / libcrypto のリンクは aws-lc-sys（`http3` で `ssl` フィーチャー有効）が担う。
//!
//! F-120: クロスプラットフォーム対応（Phase 1）向けに、target_os / feature の
//! 組み合わせから `veil_rt_uring` / `veil_rt_reactor` / `veil_poller_epoll` /
//! `veil_poller_kqueue` / `veil_ktls` の cfg エイリアスを発行する。判定が
//! 各所に散らばるのを防ぎ、`src/runtime/` 等はこれらのエイリアスのみを見ればよい。

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_HTTP3");
    println!("cargo:rerun-if-env-changed=AWS_LC_SYS_NO_PREFIX");
    println!("cargo:rerun-if-env-changed=VEIL_SSL_NO_PREFIX_BOOTSTRAP");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EPOLL");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_KTLS");

    emit_runtime_backend_cfg();

    if !feature_enabled("HTTP3") {
        return;
    }

    ensure_aws_lc_no_prefix();
}

fn feature_enabled(name: &str) -> bool {
    std::env::var(format!("CARGO_FEATURE_{name}")).is_ok()
}

/// ランタイムバックエンド選択用の cfg エイリアスを発行する（F-120 Phase 1）。
///
/// | cfg | 条件 | 意味 |
/// |-----|------|------|
/// | `veil_rt_uring` | `target_os = "linux"` かつ `not(feature = "epoll")` | io_uring 完了ベースランタイム |
/// | `veil_rt_reactor` | 上記以外（linux+epoll、freebsd、openbsd、macos、windows） | readiness ベースランタイム |
/// | `veil_poller_epoll` | `target_os = "linux"` かつ `feature = "epoll"` | reactor の poller = epoll |
/// | `veil_poller_kqueue` | `target_os = "freebsd"`、`"openbsd"`、`"macos"` | reactor の poller = kqueue |
/// | `veil_poller_wsapoll` | `target_os = "windows"` | reactor の poller = WSAPoll（F-125、cfg 発行のみ。実装は別作業） |
/// | `veil_ktls` | `feature = "ktls"` かつ (`target_os = "linux"` または `"freebsd"`) | kTLS カーネルオフロード経路（F-126: FreeBSD 対応追加。OpenBSD は非対応のまま） |
///
/// `cargo::rustc-check-cfg` も併せて発行し、`unexpected_cfgs` 警告を防ぐ。
fn emit_runtime_backend_cfg() {
    // 値なしフラグ cfg として宣言（unexpected_cfgs lint 対策）。
    println!("cargo::rustc-check-cfg=cfg(veil_rt_uring)");
    println!("cargo::rustc-check-cfg=cfg(veil_rt_reactor)");
    println!("cargo::rustc-check-cfg=cfg(veil_poller_epoll)");
    println!("cargo::rustc-check-cfg=cfg(veil_poller_kqueue)");
    println!("cargo::rustc-check-cfg=cfg(veil_poller_wsapoll)");
    println!("cargo::rustc-check-cfg=cfg(veil_ktls)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let epoll = feature_enabled("EPOLL");
    let ktls = feature_enabled("KTLS");

    match target_os.as_str() {
        "linux" => {
            if epoll {
                println!("cargo::rustc-cfg=veil_rt_reactor");
                println!("cargo::rustc-cfg=veil_poller_epoll");
            } else {
                println!("cargo::rustc-cfg=veil_rt_uring");
            }
        }
        "freebsd" | "openbsd" | "macos" => {
            if epoll {
                panic!(
                    "veil build.rs: --features epoll is only meaningful on Linux \
                     (target_os = \"linux\"); on target_os = \"{target_os}\" the kqueue \
                     reactor is selected automatically. Remove the epoll feature for \
                     this target."
                );
            }
            println!("cargo::rustc-cfg=veil_rt_reactor");
            println!("cargo::rustc-cfg=veil_poller_kqueue");
        }
        "windows" => {
            if epoll {
                panic!(
                    "veil build.rs: --features epoll is only meaningful on Linux \
                     (target_os = \"linux\"); on target_os = \"windows\" the WSAPoll \
                     reactor is selected automatically. Remove the epoll feature for \
                     this target."
                );
            }
            // F-125: cfg 発行のみ。WSAPoll reactor 本体（reactor/wsapoll.rs 等）は
            // 別作業で実装する（現時点では veil_rt_reactor 選択時に他 OS 向け reactor
            // コードがそのまま Windows 向けにコンパイルされるわけではなく、reactor
            // 内部の Unix 専用コードは Windows では別途 cfg 分岐が必要になる。今回の
            // 変更はビルドバックエンド選択の cfg 発行のみで、Windows 実装は含まない）。
            println!("cargo::rustc-cfg=veil_rt_reactor");
            println!("cargo::rustc-cfg=veil_poller_wsapoll");
        }
        other => {
            panic!(
                "veil build.rs: unsupported target_os \"{other}\" — veil currently \
                 supports target_os = \"linux\", \"freebsd\", \"openbsd\", \"macos\", \
                 \"windows\" only (F-120/F-125)"
            );
        }
    }

    if ktls && (target_os == "linux" || target_os == "freebsd") {
        println!("cargo::rustc-cfg=veil_ktls");
    }
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

    eprintln!("veil build.rs: applying AWS_LC_SYS_NO_PREFIX=1 for quiche/rustls AWS-LC sharing");

    let cargo = std::env::var("CARGO").unwrap_or_else(|_| "cargo".into());
    let status = std::process::Command::new(cargo)
        .env("AWS_LC_SYS_NO_PREFIX", "1")
        .env("VEIL_SSL_NO_PREFIX_BOOTSTRAP", "1")
        .args(std::env::args().skip(1))
        .status()
        .expect("failed to re-run cargo with AWS_LC_SYS_NO_PREFIX=1");

    std::process::exit(status.code().unwrap_or(1));
}
