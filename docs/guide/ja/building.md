# ビルドとパッケージ

[← ドキュメント目次](README.md) · [English](../building.md)

## ビルド

### 依存ライブラリ

有効にするフィーチャーに応じて、以下のシステムライブラリが必要です：

| 依存ライブラリ | 必要なフィーチャー | 備考 |
|--------------|----------------|------|
| `cmake` | `http3` フィーチャー（aws-lc-sys libssl、quiche と共有） | ビルド前にインストールが必要 |
| `nasm` | `aws-lc-rs`（TLS、常時必要） | 暗号処理のアセンブリ最適化 |

Debian/Ubuntu の場合：

```bash
apt-get install -y cmake nasm
```

### ローカルビルド

```bash
# デフォルトビルド — kTLS + HTTP/2 + mimalloc（推奨）
cargo build --release

# 全機能ビルド — 全オプションフィーチャー有効
cargo build --release --features full

# 最小ビルド — オプションフィーチャーなし
cargo build --release --no-default-features
```

ビルド後のバイナリは `target/release/veil` に生成されます。

### 配布用ビルド（glibc 2.28、Docker 使用）

古い Linux ディストリビューション（glibc ≥ 2.28）向けのバイナリを生成するには、
Zig ツールチェーンで最小 glibc バージョンにリンクする [`messense/cargo-zigbuild`](https://github.com/messense/cargo-zigbuild) を使用します。

```bash
# 全機能ビルド
docker run --rm -it -v $(pwd):/io -w /io messense/cargo-zigbuild bash -c \
  "apt-get update -y && apt-get install -y cmake nasm && \
   cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28 --features full"

# デフォルトビルド（cmake/nasm 不要）
docker run --rm -it -v $(pwd):/io -w /io messense/cargo-zigbuild \
  cargo zigbuild --release --target x86_64-unknown-linux-gnu.2.28
```

ビルド後のバイナリは `target/x86_64-unknown-linux-gnu/release/veil` に生成されます。

### Linux パッケージ（.deb / .rpm）とバイナリ tar.gz

Debian/Ubuntu（`.deb`）と Amazon Linux 2023（`.rpm`）向けのインストールパッケージ、および glibc / musl スタンドアロンバイナリの tar.gz を、全機能（`--features full`）で一括ビルドします。

```bash
./packaging/scripts/build.sh
```

成果物:

```
packaging/output/veil_<version>_<arch>.deb
packaging/output/veil-<version>-1.<arch>.rpm
packaging/output/veil-<version>-x86_64-unknown-linux-gnu.tar.gz
packaging/output/veil-<version>-x86_64-unknown-linux-musl.tar.gz
```

Docker でのビルド（[docker/Dockerfile.glibc](../../../docker/Dockerfile.glibc) の `messense/cargo-zigbuild` による glibc 2.28 互換バイナリ、および [docker/Dockerfile.musl](../../../docker/Dockerfile.musl) による musl バイナリ）:

```bash
# バイナリとパッケージ生成をすべて Docker 内で実行
./packaging/scripts/build.sh --docker
```

インストール手順:

```bash
# Debian/Ubuntu
sudo dpkg -i packaging/output/veil_0.7.0_amd64.deb
sudo apt-get install -f
sudo systemctl enable --now veil

# Amazon Linux 2023
sudo dnf install -y packaging/output/veil-0.7.0-1.x86_64.rpm
sudo systemctl enable --now veil
```

Docker コンテナでのインストール・起動・curl 動作確認（両パッケージ）:

```bash
./packaging/scripts/test-install.sh
```

詳細は [packaging/README.md](../../../packaging/README.md) を参照してください。

> **注意**: `--features full` でビルドする場合、`http3` フィーチャーが aws-lc-sys の `libssl` をビルドするため cmake が、`aws-lc-rs` がアセンブリ最適化を使用するため `nasm` が、それぞれコンテナ内にインストールされている必要があります。`http3` を含まないデフォルトビルドでは cmake は不要です。
>
> **`AWS_LC_SYS_NO_PREFIX`（ターゲット別、B-47）**: `http3` / `full` ビルドでのこの値は [`.cargo/config.toml`](../../../.cargo/config.toml) の `[env]` テーブル**のみ**で設定します。aws-lc-sys がターゲット別に優先して読む変数名（`AWS_LC_SYS_NO_PREFIX_<トリプルの - を _ にしたもの>`）を列挙する方式です。
> - **Linux → `1`**: quiche が rustls と同じ**非プレフィックス**の AWS-LC シンボルへリンクする（`aws-lc-sys` を 1 つ共有）。
> - **FreeBSD / Windows / macOS / OpenBSD → `0`**: quiche は外部 `boringssl-boring-crate`（`boring`）を使うため、`aws-lc-sys` 側はプレフィックスを維持して共存させる。FreeBSD は F-136 で capsicum capability mode 下の証明書ホットリロード（quiche の in-memory `SSL_CTX` API が `boringssl-boring-crate` 限定）のためここへ移った。
>
> cargo には**ターゲット別の環境変数設定が存在せず**（`[target.<triple>.env]` は警告もなく無視される）、`build.rs` から依存クレートのビルドスクリプトへ環境変数を渡すこともできません（依存側が先に別プロセスで実行されるため）。この変数を Dockerfile や packaging スクリプトで設定してはいけません（設定箇所は `.cargo/config.toml` 1 箇所）。

> **Cargo フィーチャー**: 利用可能なフィーチャーフラグの一覧は [`Cargo.toml` の `[features]` セクション](../../../Cargo.toml) を参照してください。
> 主な注意点：
> - **デフォルトフィーチャー**: `ktls`、`http2`、`mimalloc`
> - **`full`**: 全フィーチャーを有効化（`ktls`、`http2`、`http3`、`grpc-full`、`wasm`、`compression`、`cache`、`metrics`、`websocket`、`rate-limit`、`buffering`、`mimalloc`）
> - **`full-freebsd` / `full-openbsd` / `full-netbsd`**: `full` と機能セットは同一でアロケータのみ異なる BSD 向けセット。`full-freebsd` は **jemalloc**（`aio` は B-63 により既定から除外）、`full-openbsd`/`full-netbsd` は**システムアロケータ**を使う。3 者とも `wasm` を含み、`full-openbsd`/`full-netbsd` は wasmtime の **Pulley インタープリタ**経由で動作する（B-52/B-55）。TLS はいずれも同梱構成（FreeBSD は rustls+aws_lc_rs、OpenBSD/NetBSD は rustls+ring。quiche は 3 者とも同梱 BoringSSL）。これらの feature セットは**アーキテクチャ非依存**である（旧 `full-freebsd-aarch64`/`full-openbsd-aarch64`/`full-netbsd-aarch64` は基底セットと完全同一だったため廃止した。x86_64・aarch64 とも `--features full-freebsd` 等をそのまま使う）。NetBSD（全アーキ）・FreeBSD aarch64・OpenBSD aarch64 では `wasm` が crates.io wasmtime の代わりに vendoring 版 `third_party/wasmtime`（Pulley 専用、詳細は `third_party/wasmtime/README.veil.md`）を使う（crates.io wasmtime 40 がこの 3 ターゲットのシグナルハンドリングに対応していないため）。それ以外のプラットフォームは無影響で crates.io wasmtime のまま。cargo にターゲット別 default features が無いため、packaging のスクリプトが `--no-default-features` と併せて明示指定する（`packaging/scripts/build-cross.sh --target freebsd`、`tools/qemu/bsd-vm.sh <os> <arch> build|e2e`）。素の `--features full` の挙動は従来どおり変わらない。
> - **`full-container`**（F-144）: `full` と機能セットは同一で、加えて `epoll` を有効化する。デフォルトの io_uring 完了ベースランタイム（`veil_rt_uring`）の代わりに、明示的に epoll ベースの readiness ランタイム（`veil_rt_reactor`、Linux 専用）を選ぶ。コンテナ・オーケストレーション環境（Docker/Kubernetes/gVisor 等）では seccomp プロファイルやコンテナランタイムのシステムコールエミュレーションにより io_uring がしばしばブロック・制限されるための対応。`default` は不変。`cargo build --features full-container`（アロケータも差し替えたい場合は `--no-default-features --features full-container` を併用）でビルドする。
> - **`alloc-stats`**（F-165、診断専用・既定オフ）: 選択中のアロケータ（mimalloc/jemalloc/システム）を
>   カウンティングアロケータで包み、`veil_alloc_allocs_total` / `veil_alloc_deallocs_total` /
>   `veil_alloc_reallocs_total` / `veil_alloc_bytes_total` を Prometheus ゲージとして公開する
>   （`metrics` feature と `[prometheus] enabled = true` が必要）。**1 リクエストあたりの
>   ヒープ確保回数**を負荷中に実測するための feature で、ハーネスは `tools/perf/alloc_measure.sh`。
>   確保ごとにアトミック加算が乗るため、本番ビルドでは有効にしないこと。
> - **アロケータフィーチャー**（`mimalloc`、`jemalloc`、`system-allocator`）は排他的 — 複数同時有効化不可
> - HTTP/3 は UDP ベースのため kTLS と併用不可



### ビルドプロファイル

- **`cargo build --release`** は cargo 既定（`lto = false` / `codegen-units = 16`）。
  Linux x86_64 と FreeBSD aarch64 の両方で実測した結果、veil のワークロードでは
  **LTO によるスループット差は誤差範囲**だった（ホットパスは syscall・TLS 暗号処理・
  コンテキストスイッチが支配的で、クレート跨ぎ呼び出しのコストは相対的に小さい）。
  そのため開発・E2E・perf 計測のビルドを遅くしてまで有効にしない
  （fat LTO は FreeBSD の増分ビルドで実測 26 秒 → 4 分 25 秒）。release はシンボルを
  残すので、DTrace のスタックや panic のバックトレースを関数名で読める。
- **`cargo build --profile dist`** が配布用（`inherits = "release"` +
  `lto = "fat"` + `codegen-units = 1` + `strip = "symbols"`）。実測 36.8MB → 25.3MB
  （**-31%**）。packaging のみが使う（`docker/Dockerfile.*`、
  `packaging/scripts/build.sh`、`tools/qemu/bsd-vm.sh` は `CARGO_PROFILE=dist`）。
- **`panic = "abort"` は意図的に使わない。** `src/system.rs` がコネクションのタスクを
  `catch_unwind` で包み、1 リクエストの panic をログに記録してワーカーを生かす設計。
  abort にすると 1 本の不正リクエストでプロキシ全体が落ち、`ConnectionGuard` の
  `Drop`（接続数の計上）も走らない。
