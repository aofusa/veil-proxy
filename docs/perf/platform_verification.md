# v0.8.0 全プラットフォーム検証

リリース前に、対象のすべての OS・アーキテクチャで単体・統合・standalone・E2E・ライブラリの試験を回した結果
（2026-10-18〜19）。ソースは `34528afb` 以降変わっていない（その後のコミットは文書・試験のハーネス・VM のスクリプトだけ）。
Linux x86_64 以外は実機（macOS・Windows）または実カーネルの VM（`tools/qemu/`）上で、ネイティブビルドしたものを
試験している。

| OS | アーキ | 環境 | feature | 単体 | 統合 | standalone | E2E | ライブラリ | コミット |
|---|---|---|---|---|---|---|---|---|---|
| Linux | x86_64 | ホスト（io_uring） | `full` | 1085 | 60 | 23 | 569 | 230 | `7b23beba` |
| Linux | x86_64 | ホスト（epoll reactor） | `full,epoll` | — | — | — | 569 | — | `7b23beba` |
| Linux | aarch64 | QEMU + HVF（Ubuntu、io_uring） | `full` | 1083 | 60 | 23 | 569 | 230 | `7b23beba` |
| macOS | aarch64 | 実機（kqueue reactor） | `full-macos` | 1001 | 60 | 23 | 569 | 196 | `7b23beba` |
| Windows | x86_64 | 実機（WSAPoll reactor） | `full-windows` | 954 | 54 | 22 | スモーク※1 | 195 | `7b23beba` |
| FreeBSD | aarch64 | QEMU + HVF（14.3） | `full-freebsd` | 1022 | 60 | 23 | 569 | 169 | `7b23beba` |
| FreeBSD | x86_64 | QEMU + KVM（14.3） | `full-freebsd` | 1022 | 60 | 23 | 569 | 169 | `d438059b` |
| OpenBSD | aarch64 | QEMU + HVF（7.9） | `full-openbsd` | 999 | 60 | 22 | 568（無視 1） | 169 | `7b23beba` |
| OpenBSD | x86_64 | QEMU + KVM（7.9） | `full-openbsd` | 999 | 60 | 22 | 568（無視 1） | 169 | `d438059b` |
| NetBSD | aarch64 | QEMU + HVF（10.1） | `full-netbsd` | 999 | 60 | 22 | 569 | 169 | `7b23beba` |
| NetBSD | x86_64 | QEMU + KVM（10.1） | `full-netbsd` | 999 | 60 | 22 | 569 | 169 | `d438059b` |

すべて失敗 0。件数の違いは feature とプラットフォームの `cfg` による（Windows は Unix 専用の試験が、BSD は
`full-*` に含まれない機能の試験が対象外）。ライブラリは `tests/api/library_e2e.sh all` の確認の数（Rust・GraphQL・
gRPC・C・Python。OS によって載らない機能がある）。

BSD の 3 OS は、それぞれの OS 固有のサンドボックス（FreeBSD の capsicum・OpenBSD の pledge/unveil・NetBSD の chroot +
特権降格）の下での E2E（`bsd-vm.sh <os> <arch> security-e2e`）も通った。

※1 Windows は bash の E2E ハーネスの構成が UDS のバックエンドなど Windows で使えない機能を含むため、配布物（`--profile dist`）で
前段（静的配信 + プロキシ）と後段の veil を 2 つ起動して疎通を確かめた（下の「Windows のスモーク」）。

## ビルド・静的解析

| 対象 | 結果 |
|---|---|
| Linux x86_64: `default` / `--no-default-features` / `full` / `full,epoll` / `full-container` / `full,api` / メッシュ、各 feature の単独（`--no-default-features --features <f>`）と既定 + 1 feature（`docs/artifacts/feature-check.sh`） | 92 通りすべて clippy `-D warnings` で指摘 0 |
| macOS: `cargo clippy --no-default-features --features full-macos --all-targets -D warnings` | 指摘 0 |
| Windows: `cargo clippy --no-default-features --features full-windows --all-targets -D warnings` | 指摘 0（v0.8.0 で初めて通した。windows-sys の定数への同じ型へのキャスト 20 件など） |
| 各プラットフォームの試験のビルド | 警告 0 |
| `cargo fmt --check` | 差分なし |
| `#[allow(dead_code)]` | 追加なし（Windows の試験だけが警告を出していた unix 専用の補助関数は `cfg(unix)` にした） |

Rust のツールチェーンは全プラットフォームで 1.99。Windows（1.95）と macOS（1.94）は古い clippy だけが出す指摘
（`nonminimal_bool`・`collapsible_match`）があったので直してから更新した。

## container_security（Linux x86_64、`veil:glibc`）

`SKIP_LIBFUZZER=0 SKIP_CARGO_AUDIT=0 SKIP_CARGO_DENY=0 SKIP_ZAP=0 SKIP_BAD_BACKEND=0 SKIP_PUMBA=0 SKIP_RESOURCE_EXHAUSTION=0
SKIP_SYSCALL_CHAOS=0 SKIP_DIFFERENTIAL=0 RUN_E2E_ASAN=1` で全フェーズを回し、**32 合格・失敗 0**（libFuzzer の ASAN/TSAN 版と
カーネル能力のフェーズは既定で無効）。syscall 障害注入・ASAN の E2E カオスも検出 0。

## 検証の過程で見つけて直した不具合

| チケット | 見つかった環境 | 内容 |
|---|---|---|
| [B-154](../backlog/bugs/B-154-static-first-request-404-when-openat2-blocked.md) | Linux（container_security） | openat2 が seccomp で拒否される環境で、起動後最初の静的配信の要求が 404 |
| [B-155](../backlog/bugs/B-155-dependency-advisories-v080.md) | Linux（container_security の cargo-audit） | 依存の RUSTSEC 勧告（rustls・quinn-proto・pyo3・wasmtime） |
| F-240（quiche の `send_body_zc`） | Linux（perf の h2load） | 本文のゼロコピーで偽の STREAM_DATA_BLOCKED を送り、ngtcp2 のクライアントが FINAL_SIZE_ERROR で閉じる（E2E の quinn は無視） |
| B-122（追記） | Linux（standalone の並列実行） | 平文リスナーで判別に足りない断片の続きが 200ms に間に合わず RST |
| — | Windows・macOS | clippy の指摘（上の表）・quiche の build.rs が macOS のビルドのたびに出す警告 |
| — | 試験のハーネス | Linux aarch64 の VM へ `tools/vds` を送っていない・library の再起動の確認が同じポート（SO_REUSEPORT で前のサーバに繋がる）・シグナルの試験が他の試験のポートと衝突・container_security のハーネスが F-200 後の平文ポートを見ていない |

## Windows のスモーク

配布物 `veil-0.8.0-x86_64-pc-windows-msvc.zip`（`build-cross.sh --target windows`、`--profile dist`）の `veil.exe` で、
前段（`listen_cleartext`、静的配信と `/proxy/` の中継）と後段（静的配信）を起動して確かめた。

| 確認 | 結果 |
|---|---|
| `veil.exe --version` | `veil 0.8.0` |
| 前段の静的配信（HTTP/1.1） | 200 |
| 前段の静的配信（h2c prior knowledge） | 200（HTTP/2） |
| `/proxy/` → 後段 | 200（後段の本文） |
| 後段を止めた後の `/proxy/` | 502（接続エラーをログに出す） |

## 配布物（`--profile dist`、2026-10-19）

`packaging/README.md` の手順で、ビルドは直列に回した（Linux・Windows・macOS は Docker、BSD x86_64 はこのホストの QEMU + KVM、
BSD aarch64 は Apple Silicon の QEMU + HVF。VM のメモリは 7〜8GB で、fat LTO のまま通った）。

| 成果物 | 作り方 |
|---|---|
| `veil_0.8.0_amd64.deb`・`veil-0.8.0-1.x86_64.rpm`・`veil-0.8.0-x86_64-unknown-linux-{gnu,musl}.tar.gz` | `build.sh --docker` |
| `veil_0.8.0_arm64.deb`・`veil-0.8.0-1.aarch64.rpm`・`veil-0.8.0-aarch64-unknown-linux-{gnu,musl}.tar.gz` | `RUST_TARGET=aarch64-unknown-linux-gnu build.sh --docker` |
| `veil-0.8.0-universal2-apple-darwin.tar.gz` | `build-cross.sh --target macos` |
| `veil-0.8.0-{x86_64,aarch64}-pc-windows-msvc.zip` | `build-cross.sh --target windows` |
| `veil-0.8.0-{x86_64,aarch64}-unknown-{freebsd,openbsd,netbsd}.tar.gz` | `bsd-vm.sh <os> <arch> build`（`CARGO_PROFILE=dist`）→ `fetch` → `build-bsd.sh --all` |
| `lib/veil-lib-0.8.0-x86_64-linux-gnu.zip`（C ライブラリ・Python wheel・Rust ソース） | `build-lib.sh --docker` |

`verify-artifacts.sh rustls-0.23.46 discover_pmtu` で 15 件すべて OK（rpm の 2 件は `rpm2cpio` を別に使って同じ確認をした）。
ライブラリの zip の `libveil.so`・`libveil.a`・wheel の拡張モジュールも同じ文字列を含む（wheel は pyo3 0.29.3）。

