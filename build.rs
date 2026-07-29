//! veil のビルドスクリプト。
//!
//! `http3` フィーチャー有効時の `AWS_LC_SYS_NO_PREFIX` は **`.cargo/config.toml` の
//! `[env]`（ターゲット接尾辞付きの変数名）が唯一の設定箇所**であり、ここでは扱わない。
//! ビルドスクリプトの実行順序上、依存クレート（aws-lc-sys）のビルドスクリプトは
//! 本スクリプトより**先に別プロセスとして**実行されるため、ここで
//! `std::env::set_var` してもこれらへは一切伝播しない（B-47）。
//! ターゲット別の値と理由は `.cargo/config.toml` のコメントを参照。
//!
//! F-120: クロスプラットフォーム対応（Phase 1）向けに、target_os / feature の
//! 組み合わせから `veil_rt_uring` / `veil_rt_reactor` / `veil_poller_epoll` /
//! `veil_poller_kqueue` / `veil_ktls` / `veil_aio`(F-127) の cfg エイリアスを発行する。判定が
//! 各所に散らばるのを防ぎ、`src/runtime/` 等はこれらのエイリアスのみを見ればよい。

fn main() {
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_OS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EPOLL");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_KTLS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_AIO");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_SYSTEM_TLS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_VENDORED_TLS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_HTTP3");

    check_tls_backend_exclusivity();
    check_system_tls_quic_capability();
    emit_runtime_backend_cfg();
}

fn feature_enabled(name: &str) -> bool {
    std::env::var(format!("CARGO_FEATURE_{name}")).is_ok()
}

/// `vendored-tls`（同梱 aws-lc-rs/aws-lc-sys、F-142 で feature 化）と `system-tls`
/// （システムの OpenSSL/LibreSSL への動的リンク）は排他（F-137/F-142）。
///
/// 両方を有効にすると、aws-lc-sys のビルド成果物に含まれる `libssl.a`/`libcrypto.a`
/// （`AWS_LC_SYS_NO_PREFIX` の値に関わらずこの名前で生成される）が最終リンクコマンドの
/// `-L` 探索順でシステムの動的 libssl/libcrypto より先に来てしまい、`system-tls`
/// プロバイダ（`src/tls_provider/libressl/`）/ quiche が要求するシンボルの一部
/// （aws-lc-sys に無い新しめの OpenSSL 3.0 系 API）が undefined symbol になってリンクが
/// 壊れる（`http3` を外した最小構成でも再現し、quiche の feature 選択とは無関係の問題
/// であることを確認済み。`docs/backlog/features/F-137-system-tls-feature.md` 参照）。
/// 意味不明なリンクエラーで悩ませないよう、依存関係の解決前にビルドを止めて
/// 原因と対処法を明示する。
fn check_tls_backend_exclusivity() {
    // F-142: `system-tls` は Linux/FreeBSD/OpenBSD/NetBSD で使用可能（macOS/Windows は
    // 未対応）。Linux/FreeBSD は F-142 で aws-lc-rs/aws-lc-sys を `vendored-tls` feature
    // 配下の optional 依存にしたことで排他切替できるようになった。
    const SYSTEM_TLS_ALLOWED_TARGET_OSES: &[&str] = &["openbsd", "netbsd", "linux", "freebsd"];

    if !feature_enabled("SYSTEM_TLS") {
        return;
    }
    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    if !SYSTEM_TLS_ALLOWED_TARGET_OSES.contains(&target_os.as_str()) {
        panic!(
            "veil build.rs: `system-tls` is only supported on {allowed:?} (current target_os \
             = `{target_os}`). On other targets aws-lc-rs/aws-lc-sys are unconditional \
             dependencies whose build artifacts contain files literally named \
             libssl.a/libcrypto.a; those shadow the system libssl/libcrypto in the linker's \
             -L search order and the system-tls provider fails with undefined-symbol errors \
             (reproducible even with http3 disabled, so it is unrelated to the quiche feature \
             selection). Drop `system-tls` on this target. \
             See docs/backlog/features/F-137-system-tls-feature.md.",
            allowed = SYSTEM_TLS_ALLOWED_TARGET_OSES,
            target_os = target_os,
        );
    }

    // F-142: `vendored-tls` と `system-tls` の同時指定は、両方が aws-lc-sys の静的
    // ライブラリ（`vendored-tls`）とシステム libssl への動的リンク（`system-tls`）を
    // 同一バイナリに持ち込もうとするため、上記と同じリンク衝突を起こす。
    if feature_enabled("VENDORED_TLS") {
        panic!(
            "veil build.rs: `vendored-tls` and `system-tls` cannot be enabled together. \
             `vendored-tls` pulls in aws-lc-rs/aws-lc-sys (static libssl.a/libcrypto.a \
             build artifacts) while `system-tls` dynamically links the system \
             libssl/libcrypto; combining them causes the same linker shadowing/undefined- \
             symbol failures described for the target_os check above. Pick exactly one: \
             drop `vendored-tls` for a system-tls build, or drop `system-tls` for the \
             default vendored build. See docs/backlog/features/F-137-system-tls-feature.md."
        );
    }
}

/// `system-tls` + `http3` の組み合わせが要求する QUIC API を、システムの libssl が
/// 実装しているかを pkg-config 経由で検査する（F-142）。
///
/// quiche は BoringSSL 系 QUIC API（`SSL_set_quic_method`/`SSL_provide_quic_data` 等）を
/// 要求するが、バニラ OpenSSL 3.x にはこれが無い（NetBSD 実機で
/// `undefined reference to SSL_set_quic_method` を確認済み）。LibreSSL 3.6+ / quictls には
/// 存在する。リンク時の不可解な undefined reference で悩ませないよう、依存解決前に
/// `openssl/ssl.h` を pkg-config の include path から探して `SSL_set_quic_method` の
/// 宣言があるかを grep で確認し、無ければビルドを止めて明確なエラーを出す。
///
/// `http3` を含まない `system-tls` 単体（rustls のみ）はこのチェックをスキップする
/// （OpenSSL 3.x でも LibreSSL でも rustls 側は動くため）。
fn check_system_tls_quic_capability() {
    if !(feature_enabled("SYSTEM_TLS") && feature_enabled("HTTP3")) {
        return;
    }

    let Some(include_dir) = pkg_config_variable("libssl", "includedir") else {
        // pkg-config 自体が無い/libssl.pc が見つからない場合は、リンク時に quiche 側の
        // ビルドスクリプトがより具体的なエラーを出す（ここでは検出できないだけで諦める）。
        println!(
            "cargo:warning=veil: could not locate libssl via pkg-config to verify QUIC API \
             support for `system-tls` + `http3`; proceeding, but the build may fail later \
             with an undefined-symbol link error if the system libssl lacks \
             SSL_set_quic_method (LibreSSL 3.6+ / quictls required)."
        );
        return;
    };

    let header = std::path::Path::new(&include_dir).join("openssl/ssl.h");
    // build.rs はコールドパス（ビルド時に一度だけ実行）であり、AGENTS.md のホットパス
    // 同期 I/O 禁止規則の対象外。
    #[allow(clippy::disallowed_methods)]
    let has_quic_api = std::fs::read_to_string(&header)
        .map(|contents| contents.contains("SSL_set_quic_method"))
        .unwrap_or(false);

    if !has_quic_api {
        panic!(
            "veil build.rs: `system-tls` + `http3` requires the system libssl to implement \
             the BoringSSL-derived QUIC API (LibreSSL 3.6+ / quictls), specifically \
             `SSL_set_quic_method`. It was not found in {header}. Either drop `http3` (rustls \
             alone works fine with vanilla OpenSSL 3.x / LibreSSL under system-tls), or use \
             `vendored-tls` instead, or install a QUIC-capable libssl (e.g. LibreSSL 3.6+ on \
             OpenBSD/NetBSD base, or pkgsrc libressl on NetBSD, or quictls on Linux/FreeBSD).",
            header = header.display(),
        );
    }
}

/// `pkg-config --variable=<var> <pkg>` を実行し、成功した場合の標準出力（trim 済み）を返す。
/// `pkg-config` コマンド自体が無い、または対象パッケージが見つからない場合は `None`。
fn pkg_config_variable(pkg: &str, var: &str) -> Option<String> {
    let output = std::process::Command::new("pkg-config")
        .arg(format!("--variable={var}"))
        .arg(pkg)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let value = String::from_utf8(output.stdout).ok()?;
    let value = value.trim();
    if value.is_empty() {
        None
    } else {
        Some(value.to_string())
    }
}

/// ランタイムバックエンド選択用の cfg エイリアスを発行する（F-120 Phase 1）。
///
/// | cfg | 条件 | 意味 |
/// |-----|------|------|
/// | `veil_rt_uring` | `target_os = "linux"` かつ `not(feature = "epoll")` | io_uring 完了ベースランタイム |
/// | `veil_rt_reactor` | 上記以外（linux+epoll、freebsd、openbsd、macos、windows） | readiness ベースランタイム |
/// | `veil_poller_epoll` | `target_os = "linux"` かつ `feature = "epoll"` | reactor の poller = epoll |
/// | `veil_poller_kqueue` | `target_os = "freebsd"`、`"openbsd"`、`"macos"`、`"netbsd"`（F-140） | reactor の poller = kqueue |
/// | `veil_poller_wsapoll` | `target_os = "windows"` | reactor の poller = WSAPoll（F-125、cfg 発行のみ。実装は別作業） |
/// | `veil_ktls` | `feature = "ktls"` かつ (`target_os = "linux"` または `"freebsd"`) かつ `not(feature = "system-tls")` かつ `feature = "vendored-tls"` | kTLS カーネルオフロード経路（F-126: FreeBSD 対応追加。OpenBSD は非対応のまま。F-137: `system-tls` と同時指定時、F-142: `vendored-tls` 未指定時は自動的に無効化し `cargo:warning` を出す） |
/// | `veil_aio` | `feature = "aio"` かつ `target_os = "freebsd"` | POSIX AIO（`aio_read`/`aio_write` + `EVFILT_AIO`）による TCP read/write 経路（F-127。FreeBSD 専用、既定オフ） |
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
    println!("cargo::rustc-check-cfg=cfg(veil_aio)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let epoll = feature_enabled("EPOLL");
    let ktls = feature_enabled("KTLS");
    let aio = feature_enabled("AIO");
    let system_tls = feature_enabled("SYSTEM_TLS");
    let vendored_tls = feature_enabled("VENDORED_TLS");

    match target_os.as_str() {
        "linux" => {
            if epoll {
                println!("cargo::rustc-cfg=veil_rt_reactor");
                println!("cargo::rustc-cfg=veil_poller_epoll");
            } else {
                println!("cargo::rustc-cfg=veil_rt_uring");
            }
        }
        "freebsd" | "openbsd" | "macos" | "netbsd" => {
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
                 \"netbsd\", \"windows\" only (F-120/F-125/F-140)"
            );
        }
    }

    if ktls && (target_os == "linux" || target_os == "freebsd") {
        if system_tls {
            // F-137: veil_ktls は aws_lc_rs 固有の cipher_suite 定数を直接参照する
            // （src/ktls_rustls.rs 参照）ため、`system-tls`とは併用できない。
            // cfg を立てず kTLS を自動的に無効化する。
            println!(
                "cargo:warning=veil: `ktls` feature is disabled automatically because \
                 `system-tls` is also enabled (kTLS relies on aws_lc_rs-specific cipher \
                 suite constants that are unavailable under system-tls; F-137). Remove \
                 `ktls` from the feature list to silence this warning."
            );
        } else if !vendored_tls {
            // F-142: aws-lc-rs/aws-lc-sys が `vendored-tls` feature 配下の optional 依存に
            // なったため、`rustls::crypto::aws_lc_rs` を直接参照する kTLS 経路
            // （src/ktls_rustls.rs）は `vendored-tls` も必要とする。
            println!(
                "cargo:warning=veil: `ktls` feature is disabled automatically because \
                 `vendored-tls` is not enabled (kTLS relies on the aws_lc_rs cipher suite \
                 constants bundled by rustls's `aws_lc_rs` cargo feature, which `vendored-tls` \
                 turns on; F-142). Add `vendored-tls` to the feature list, or remove `ktls` \
                 to silence this warning."
            );
        } else {
            println!("cargo::rustc-cfg=veil_ktls");
        }
    }

    // F-127: POSIX AIO(FreeBSD の aio_read/aio_write + EVFILT_AIO)は FreeBSD 専用。
    // epoll/wsapoll と同様、対象外ターゲットで指定された場合は明確な panic とする
    // (`--features aio` の誤用をビルド時に検出する)。
    if aio {
        if target_os == "freebsd" {
            println!("cargo::rustc-cfg=veil_aio");
        } else {
            panic!(
                "veil build.rs: --features aio is only meaningful on FreeBSD \
                 (target_os = \"freebsd\"); on target_os = \"{target_os}\" POSIX AIO is not \
                 available. Remove the aio feature for this target."
            );
        }
    }
}
