//! veil のビルドスクリプト。
//!
//! ## HTTP/3
//! `http3` フィーチャー有効時、バックエンドを選択して cfg を発行する:
//! - `veil_http3_ngtcp2`: `http3` のみ（依存: ngtcp2-sys + nghttp3-sys。**quiche はビルドしない**）
//! - `veil_http3_quiche`: `http3-quiche` feature 指定時（依存に quiche を追加）
//!
//! OpenBSD / Windows では quiche バックエンドを推奨する（`--features http3,http3-quiche`）。
//! `http3` のみでも ngtcp2 経路の cfg になるが、当該 OS では quiche 利用を案内する。
//!
//! quiche 利用時は BoringSSL 互換（非プレフィックス）シンボルを rustls と共有するため
//! `AWS_LC_SYS_NO_PREFIX=1` を自動適用する。ngtcp2 の crypto_boringssl も AWS-LC を
//! 共有するため、HTTP/3 有効時は常に NO_PREFIX を適用する。
//! libssl / libcrypto のリンクは aws-lc-sys（`http3` で `ssl` フィーチャー有効）が担う。
//!
//! ## ランタイム（F-120）
//! target_os / feature の組み合わせから `veil_rt_uring` / `veil_rt_reactor` /
//! `veil_poller_epoll` / `veil_poller_kqueue` / `veil_ktls` の cfg エイリアスを発行する。

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_HTTP3");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_HTTP3_QUICHE");
    println!("cargo:rerun-if-env-changed=AWS_LC_SYS_NO_PREFIX");
    println!("cargo:rerun-if-env-changed=VEIL_SSL_NO_PREFIX_BOOTSTRAP");
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EPOLL");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_KTLS");

    emit_runtime_backend_cfg();
    emit_http3_backend_cfg();

    if feature_enabled("HTTP3") {
        ensure_aws_lc_no_prefix();
    }
}

fn feature_enabled(name: &str) -> bool {
    std::env::var(format!("CARGO_FEATURE_{name}")).is_ok()
}

/// HTTP/3 QUIC バックエンド選択用の cfg を発行する。
///
/// | cfg | 条件 |
/// |-----|------|
/// | `veil_http3_quiche` | `feature = "http3-quiche"`（依存グラフに quiche を含める） |
/// | `veil_http3_ngtcp2` | `feature = "http3"` かつ **http3-quiche なし** |
///
/// 依存: `http3` は ngtcp2/nghttp3 のみ。`http3-quiche` が quiche を追加する。
/// OpenBSD/Windows では quiche を推奨し、http3-quiche 未指定時は警告する。
fn emit_http3_backend_cfg() {
    println!("cargo::rustc-check-cfg=cfg(veil_http3_quiche)");
    println!("cargo::rustc-check-cfg=cfg(veil_http3_ngtcp2)");

    if !feature_enabled("HTTP3") {
        return;
    }

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let force_quiche = feature_enabled("HTTP3_QUICHE");
    let platform_prefers_quiche = matches!(target_os.as_str(), "openbsd" | "windows");

    if force_quiche {
        println!("cargo::rustc-cfg=veil_http3_quiche");
        eprintln!(
            "veil build.rs: HTTP/3 backend = quiche \
             (feature http3-quiche, target_os={target_os})"
        );
    } else {
        if platform_prefers_quiche {
            eprintln!(
                "veil build.rs: warning: target_os={target_os} prefers quiche; \
                 enable `--features http3-quiche` (plain `http3` builds ngtcp2/nghttp3 only, \
                 without quiche in the dependency graph)"
            );
        }
        println!("cargo::rustc-cfg=veil_http3_ngtcp2");
        eprintln!("veil build.rs: HTTP/3 backend = ngtcp2+nghttp3 (target_os={target_os})");
    }
}

/// ランタイムバックエンド選択用の cfg エイリアスを発行する（F-120 Phase 1）。
///
/// | cfg | 条件 | 意味 |
/// |-----|------|------|
/// | `veil_rt_uring` | `target_os = "linux"` かつ `not(feature = "epoll")` | io_uring 完了ベースランタイム |
/// | `veil_rt_reactor` | 上記以外（linux+epoll、freebsd、openbsd） | readiness ベースランタイム |
/// | `veil_poller_epoll` | `target_os = "linux"` かつ `feature = "epoll"` | reactor の poller = epoll |
/// | `veil_poller_kqueue` | `target_os = "freebsd"` または `"openbsd"` | reactor の poller = kqueue |
/// | `veil_ktls` | `feature = "ktls"` かつ `target_os = "linux"` | kTLS カーネルオフロード経路 |
///
/// `cargo::rustc-check-cfg` も併せて発行し、`unexpected_cfgs` 警告を防ぐ。
fn emit_runtime_backend_cfg() {
    // 値なしフラグ cfg として宣言（unexpected_cfgs lint 対策）。
    println!("cargo::rustc-check-cfg=cfg(veil_rt_uring)");
    println!("cargo::rustc-check-cfg=cfg(veil_rt_reactor)");
    println!("cargo::rustc-check-cfg=cfg(veil_poller_epoll)");
    println!("cargo::rustc-check-cfg=cfg(veil_poller_kqueue)");
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
        "freebsd" | "openbsd" => {
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
        // macOS: kqueue reactor（将来の F-120 拡張用。現状ランタイムは未完全対応でも
        // HTTP/3 バックエンド選択やクロスコンパイル時の cfg 解決に必要）
        "macos" => {
            if epoll {
                panic!(
                    "veil build.rs: --features epoll is only meaningful on Linux; \
                     on macOS the kqueue reactor is selected automatically."
                );
            }
            println!("cargo::rustc-cfg=veil_rt_reactor");
            println!("cargo::rustc-cfg=veil_poller_kqueue");
        }
        other => {
            panic!(
                "veil build.rs: unsupported target_os \"{other}\" — veil currently \
                 supports target_os = \"linux\", \"freebsd\", \"openbsd\", \"macos\" \
                 (F-120 + HTTP/3 backend selection)"
            );
        }
    }

    if ktls && target_os == "linux" {
        println!("cargo::rustc-cfg=veil_ktls");
    }
}

/// quiche / ngtcp2_crypto_boringssl は BoringSSL 互換の非プレフィックスシンボルを要求する。
/// 未設定時は cargo を `AWS_LC_SYS_NO_PREFIX=1` 付きで再実行し、
/// 呼び出し側での環境変数指定を不要にする。
fn ensure_aws_lc_no_prefix() {
    if std::env::var("AWS_LC_SYS_NO_PREFIX").ok().as_deref() == Some("1") {
        return;
    }

    if std::env::var("VEIL_SSL_NO_PREFIX_BOOTSTRAP").is_ok() {
        panic!(
            "http3 feature requires AWS_LC_SYS_NO_PREFIX=1 so quiche/ngtcp2 can share \
             aws-lc-sys with rustls, but bootstrap re-invocation did not apply it"
        );
    }

    eprintln!(
        "veil build.rs: applying AWS_LC_SYS_NO_PREFIX=1 for HTTP/3 (quiche/ngtcp2) \
         AWS-LC sharing with rustls"
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
