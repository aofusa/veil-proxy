# veil テストガイド

veil のテスト構成と実行手順。機能・ビルドの正典は [../README.md](../README.md) /
[../docs/readme/README.ja.md](../docs/readme/README.ja.md)、feature 定義は
[../Cargo.toml](../Cargo.toml)。

## テストの種類

| 種類 | 場所 | 実行 |
|------|------|------|
| ユニット | `src/**`（`#[cfg(test)]`） | `cargo test --lib --features full` |
| 統合 | `tests/integration_tests.rs` | `cargo test --test integration_tests --features full` |
| プロパティ（proptest） | `tests/{config,routing}_proptest.rs` | `cargo test --test config_proptest --features full` 等 |
| ランタイム cancel | `tests/runtime_cancellation_test.rs` | `cargo test --test runtime_cancellation_test --features full` |
| E2E（バックエンド実起動） | `tests/e2e_tests.rs` + `tests/e2e_setup.sh` | **`./tests/e2e_setup.sh test`**（下記） |

補助: `tests/fixtures/`（証明書・config 雛形）、`tests/common/`（共通ヘルパ）、
`tests/grpc_server/`・`tests/test_backends/`（E2E 用バックエンド）、`tests/wasm/`
（Proxy-Wasm モジュール）、`tests/proto/`（gRPC proto）、`tests/load/`（負荷）。

## E2E テスト（必ず e2e_setup.sh 経由で）

E2E は veil + gRPC サーバ + テストバックエンドを実起動する。**手動で直接
`cargo test --test e2e_tests` を叩くとバックエンド未起動でタイムアウトする**ため、
必ずセットアップ・実行・クリーンアップを自動化する `e2e_setup.sh` を使う。

```bash
# セットアップ + 全 E2E 実行 + クリーンアップ（features=full 既定）
./tests/e2e_setup.sh test

# feature セットを変える（例: epoll バックエンドで検証）
VEIL_E2E_FEATURES="full,epoll" ./tests/e2e_setup.sh test

# 個別サブコマンド
./tests/e2e_setup.sh start      # バックエンド + veil を起動したまま
./tests/e2e_setup.sh health     # 稼働確認
./tests/e2e_setup.sh stop       # 停止
./tests/e2e_setup.sh clean      # 生成物含め掃除
```

- ポート競合エラー時は残存プロセスを終了してから再実行する:
  `pkill -x veil`（`pkill -f veil` は cwd にパスが含まれると自シェルを巻き込むため使わない）。
- E2E は 4 コア + 同居負荷の環境で稀にフレーキー（stress/431/oversized-header 系は
  環境要因でコード非依存）。再調査前に `uptime` の loadavg を確認する。

## プラットフォーム別の実行

### Linux（既定: io_uring / `--features epoll`）
上記そのまま。`VEIL_E2E_FEATURES="full,epoll"` で epoll reactor 経路も検証できる。

### FreeBSD / OpenBSD（kqueue reactor）
対象 OS の VM 内でネイティブビルドして実行する（Rust Tier 2/3・クロス不可）。
- FreeBSD kTLS（F-126）: `sysctl kern.ipc.tls.enable=1` 後、`--features ktls` で
  HTTPS 静的配信/プロキシが 200・kTLS TX/RX 有効化を確認（無効時は rustls フォールバック）。
- FreeBSD AIO（F-127）: `--features aio` でビルドし、プロキシのバックエンドレッグで
  `aio_read`/`aio_write` が発行されることを dtrace で確認できる（`vfs.aio.*` sysctl）。

### aarch64（io_uring、full-system QEMU）
QEMU user-mode は io_uring 非対応のため、実カーネルでの io_uring E2E は
`tools/qemu/`（[../tools/qemu/README.md](../tools/qemu/README.md)）を使う:

```bash
tools/qemu/aarch64-vm.sh setup && tools/qemu/aarch64-vm.sh up
tools/qemu/run-e2e-aarch64.sh   # ホストで aarch64 クロスビルド → VM で io_uring 起動 → HTTPS スモーク
```

epoll バックエンドの aarch64 検証は QEMU user-mode（`docker/Dockerfile.{glibc,musl}.aarch64`）でも可能。

### macOS / Windows（Docker クロスビルドのみ）
実機/QEMU テストは行わず、**クロスビルドが通ること**を合格基準とする
（`packaging/scripts/build-cross.sh --target {macos|windows}`）。

## ベンチマーク

`tools/perf/`（wrk / h2load / QUIC h2load / k6 による Docker A/B ベンチ）を使う。
実行手順は [../tools/perf/README.md](../tools/perf/README.md) 参照。
`tests/run_bench.sh` はマイクロベンチ（`benches/`）のラッパ。
