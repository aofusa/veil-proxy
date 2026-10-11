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
    println!("cargo:rerun-if-env-changed=CARGO_CFG_TARGET_ARCH");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_EPOLL");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_KTLS");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_AIO");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_VEIL_ALLOC");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_MIMALLOC");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_MIMALLOC_FORCE");

    emit_runtime_backend_cfg();
    emit_wasm_nosignals_cfg();
    emit_allocator_cfg();
}

/// F-183: veil 専用アロケータ（`veil-alloc` feature）を実際に使うかの cfg エイリアス。
///
/// | cfg | 条件 | 意味 |
/// |-----|------|------|
/// | `veil_alloc_active` | `feature = "veil-alloc"` かつ `target_family = "unix"` かつ macOS 以外 | `src/veil_alloc/` をコンパイルし `#[global_allocator]` にする。Windows（pthread キー・mmap 前提のため）と macOS（F-184）では feature を無視する |
/// | `veil_mimalloc_active` | `veil_alloc_active` でなく、`feature = "mimalloc"` かつ Windows・macOS 以外、または `feature = "mimalloc-force"` | mimalloc を `#[global_allocator]` にする |
///
/// F-184: Windows と macOS は既定（`default` / `full`）でシステムアロケータ（HeapAlloc /
/// libmalloc）を使う。cargo にはターゲット別の default features が無いため、feature は
/// そのままにして build.rs で無効化する（同じクレートを別名でターゲット別に依存させることは
/// cargo が許さないため、依存はビルドされるが参照しないのでリンクされない）。Windows・macOS で
/// mimalloc を使うときは `mimalloc-force` を明示する。
fn emit_allocator_cfg() {
    println!("cargo::rustc-check-cfg=cfg(veil_alloc_active)");
    println!("cargo::rustc-check-cfg=cfg(veil_mimalloc_active)");
    let unix = std::env::var("CARGO_CFG_TARGET_FAMILY")
        .map(|f| f.split(',').any(|x| x == "unix"))
        .unwrap_or(false);
    let os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let native_alloc_os = os == "windows" || os == "macos";
    let veil_alloc = feature_enabled("VEIL_ALLOC") && unix && !native_alloc_os;
    if veil_alloc {
        println!("cargo::rustc-cfg=veil_alloc_active");
    }
    let mimalloc =
        (feature_enabled("MIMALLOC") && !native_alloc_os) || feature_enabled("MIMALLOC_FORCE");
    if mimalloc && !veil_alloc {
        println!("cargo::rustc-cfg=veil_mimalloc_active");
    }
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
/// | `veil_poller_kqueue` | `target_os = "freebsd"`、`"openbsd"`、`"macos"`、`"netbsd"`（F-140） | reactor の poller = kqueue |
/// | `veil_poller_wsapoll` | `target_os = "windows"` | reactor の poller = WSAPoll（F-125、cfg 発行のみ。実装は別作業） |
/// | `veil_ktls` | `feature = "ktls"` かつ (`target_os = "linux"` または `"freebsd"`) | kTLS カーネルオフロード経路（F-126: FreeBSD 対応追加。OpenBSD は非対応のまま） |
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
        println!("cargo::rustc-cfg=veil_ktls");
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

/// B-55: wasm feature がターゲット別依存（`wasmtime` / `veil-wasmtime`）のどちらを
/// 選んでいるかを `src/wasm/registry.rs` から判定するための cfg エイリアスを発行する。
///
/// | cfg | 条件 | 意味 |
/// |-----|------|------|
/// | `veil_wasm_nosignals` | netbsd 全アーキ ∪ (openbsd, aarch64) ∪ (freebsd, aarch64) | crates.io wasmtime のシグナルベーストラップが使えず `third_party/wasmtime`（Pulley 専用）に切り替わるターゲット。`Cargo.toml` の `[target.'cfg(<BSD_NOSIG>)'.dependencies]` と同一条件（B-55、詳細は third_party/wasmtime/README.veil.md） |
///
/// `cargo::rustc-check-cfg` も併せて発行し、`unexpected_cfgs` 警告を防ぐ。
fn emit_wasm_nosignals_cfg() {
    println!("cargo::rustc-check-cfg=cfg(veil_wasm_nosignals)");

    let target_os = std::env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    let target_arch = std::env::var("CARGO_CFG_TARGET_ARCH").unwrap_or_default();

    let nosignals = matches!(
        (target_os.as_str(), target_arch.as_str()),
        ("netbsd", _) | ("openbsd", "aarch64") | ("freebsd", "aarch64")
    );

    if nosignals {
        println!("cargo::rustc-cfg=veil_wasm_nosignals");
    }
}
