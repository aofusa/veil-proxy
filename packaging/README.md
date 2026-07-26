# Veil Linux パッケージ（.deb / .rpm）

Debian/Ubuntu 向け `.deb` と Amazon Linux 2023 向け `.rpm` を生成・検証するためのディレクトリです。

## 概要

| 項目 | 内容 |
|------|------|
| パッケージ形式 | `.deb`（Debian/Ubuntu）、`.rpm`（Amazon Linux 2023）、スタンドアロン `.tar.gz`（glibc / musl バイナリ） |
| ビルドスクリプト | `packaging/scripts/build.sh`（deb / rpm / 両バイナリ tar.gz を一括生成） |
| デフォルトビルド | `--features full`（全オプションフィーチャー有効） |
| 設定ファイル | `contrib/config/config.toml`（`examples/config.toml` ベース） |
| 設定配置先 | `/var/etc/veil/config.toml` |
| systemd ユニット | `contrib/systemd/veil.service` |
| 実行ユーザー | `veil:veil` |

## ディレクトリ構成

```
packaging/
├── README.md                    # 本ファイル
├── debian/DEBIAN/               # .deb メタデータ
├── rpm/veil.spec                # .rpm spec ファイル
├── scripts/
│   ├── build.sh                 # 統合ビルド（.deb + .rpm）
│   ├── build-bsd.sh             # FreeBSD/OpenBSD tar.gz（VM ネイティブビルド）
│   ├── build-cross.sh           # macOS / Windows / FreeBSD tar.gz・zip（Docker クロスビルド）
│   ├── test-install.sh          # 両パッケージを順に検証
│   ├── test-deb.sh              # .deb 検証
│   ├── test-rpm.sh              # .rpm 検証
│   ├── postinstall.sh           # deb/rpm 共通インストール後処理
│   ├── preuninstall.sh          # deb/rpm 共通アンインストール前処理
│   └── docker/
│       ├── Dockerfile.test-deb  # Ubuntu テスト用イメージ
│       └── Dockerfile.test-rpm  # Amazon Linux 2023 テスト用イメージ
├── build/                       # Docker ビルド中間成果物（.gitignore）
├── staging-deb/                 # .deb ステージング（.gitignore）
├── rpm/{BUILD,BUILDROOT,RPMS}/  # rpmbuild 作業ディレクトリ（.gitignore）
└── output/                      # 生成パッケージ（.gitignore）
```

## 前提条件

### ローカルビルド

| ツール | 用途 |
|--------|------|
| `cargo` | veil バイナリビルド |
| `cmake`, `nasm` | `full` フィーチャー（[README.md](../README.md) Build 節参照） |
| `dpkg-deb` | `.deb` 生成 |
| `rpmbuild` | `.rpm` 生成（`rpm` パッケージ） |

Debian/Ubuntu:

```bash
apt-get install -y cmake nasm dpkg-dev rpm
```

### Docker ビルド（推奨）

ホストに Rust ツールチェーンや dpkg-deb, rpmbuild がなくても、Docker だけでビルドできます。
`docker/Dockerfile.glibc` / `docker/Dockerfile.musl` で各バイナリをビルドし、コンテナ内で deb/rpm を作成、あわせてスタンドアロン tar.gz を `packaging/output/` へ出力します。

```bash
# パッケージ一式を Docker 内でビルド
./packaging/scripts/build.sh --docker
```

### テスト

- Docker
- ネットワーク（初回イメージ取得）

## ビルド

### 推奨: Docker でポータブルビルド（glibc 2.28 互換 + musl）

Debian/Ubuntu と Amazon Linux 2023 の両方で動作する glibc バイナリを生成します。
[docker/Dockerfile.glibc](../docker/Dockerfile.glibc)（`messense/cargo-zigbuild`）と
[docker/Dockerfile.musl](../docker/Dockerfile.musl)（`messense/rust-musl-cross`）を使用します。
deb/rpm には glibc バイナリを同梱し、両 libc 向けのスタンドアロン tar.gz も出力します。

```bash
./packaging/scripts/build.sh --docker
```

### ローカル（ネイティブ cargo / cargo zigbuild）

```bash
./packaging/scripts/build.sh
```

- **glibc**: `cargo zigbuild` が利用可能な場合は glibc 2.28 向けに自動ビルドします。
  利用できない場合はホスト glibc でビルドされ、Amazon Linux 2023 では動作しない可能性があります。
- **musl**: `cargo zigbuild --target <musl>`、または `cargo build --target <musl>`
  （`RUST_TARGET_MUSL`、既定は `x86_64-unknown-linux-musl`。要 musl ターゲット）。

### 既存バイナリからパッケージのみ生成

```bash
cargo build --release --features full
./packaging/scripts/build.sh --skip-build --binary target/release/veil
# musl の tar.gz も同時に出す場合:
# ./packaging/scripts/build.sh --skip-build \
#   --binary target/x86_64-unknown-linux-gnu/release/veil \
#   --binary-musl target/x86_64-unknown-linux-musl/release/veil
```

### aarch64（arm64）向けパッケージ（F-120 Phase 3/6）

`RUST_TARGET` に aarch64 ターゲットを指定すると、`ARCH` が自動的に `aarch64` へ
追従し deb は `arm64`・rpm は `aarch64` として出力される。Docker ビルドは
aarch64 専用 Dockerfile（`docker/Dockerfile.{glibc,musl}.aarch64`）と
`--platform linux/arm64` を自動選択する。

```bash
# aarch64 の .deb / .rpm / tar.gz（Docker クロスビルド）
RUST_TARGET=aarch64-unknown-linux-gnu ./packaging/scripts/build.sh --docker
```

### FreeBSD / OpenBSD 向けパッケージ（F-120 Phase 6）

FreeBSD/OpenBSD のバイナリは **QEMU VM 内でネイティブビルド**したものを取り出し、
専用スクリプトで rc.d サービススクリプト・設定リファレンス・（FreeBSD は）jail.conf
サンプルを同梱した tar.gz を生成する（deb/rpm は Linux 専用のため BSD は tar.gz のみ）。

VM の作成からビルド・E2E・バイナリ取得までは
[`tools/qemu/bsd-vm.sh`](../tools/qemu/README.md) が **FreeBSD/OpenBSD × x86_64/aarch64
の 4 通り**を同じインタフェースで面倒を見る（x86_64 ゲストはホストに `/dev/kvm` が
あれば KVM 加速される）。

```bash
# setup → provision → toolchain → build → e2e → fetch を一括
# （4 通り: freebsd|openbsd × x86_64|aarch64。すべて同じ形）
tools/qemu/bsd-vm.sh freebsd x86_64 all
tools/qemu/bsd-vm.sh freebsd aarch64 all
tools/qemu/bsd-vm.sh openbsd x86_64 all
tools/qemu/bsd-vm.sh openbsd aarch64 all

# 取り出したバイナリ（packaging/build/veil-<os>-<arch>）を tar.gz 化。
# --from-qemu は .os-version も自動で拾うので --binary / --os-version は不要。
./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 --from-qemu

# 取得済みのものをまとめて（存在する組み合わせだけ処理する）
./packaging/scripts/build-bsd.sh --all
```

段階を分けて実行することもできる（失敗時はその段階から再開できる）:

```bash
tools/qemu/bsd-vm.sh freebsd x86_64 setup      # イメージ取得 + シード/応答ファイル生成
tools/qemu/bsd-vm.sh freebsd x86_64 provision  # SSH 鍵注入まで
tools/qemu/bsd-vm.sh freebsd x86_64 toolchain  # rust / cmake / llvm / gmake / protobuf
tools/qemu/bsd-vm.sh freebsd x86_64 build      # --no-default-features --features full-freebsd
tools/qemu/bsd-vm.sh freebsd x86_64 e2e        # tests/e2e_setup.sh test
tools/qemu/bsd-vm.sh freebsd x86_64 fetch      # → packaging/build/veil-freebsd-x86_64
tools/qemu/bsd-vm.sh freebsd x86_64 reset      # 初期状態へ（再ダウンロード不要）
```

**所要時間の実測**（4 コア / KVM 有効ホスト、x86_64 ゲスト）:

| 対象 | in-VM リリースビルド |
|---|---|
| FreeBSD 14.3 amd64（`full-freebsd`） | **約 30 分** |
| OpenBSD 7.9 amd64（`full-openbsd`） | **約 72 分** |

aarch64 ゲストは x86_64 ホストでは TCG なので数倍〜十数倍かかる。
**OS ごとに固有の落とし穴がある**（OpenBSD は `/` が ~628M しかない、BoringSSL が
OpenBSD を想定しておらず cc ラッパ・libstdc++ 互換リンクが要る、など）。
`bsd-vm.sh` がすべて自動で処理するが、内容と検証状況は
[tools/qemu/README.md](../tools/qemu/README.md) にまとめてある。

対象 OS の VM 内で `build-bsd.sh` を直接実行する場合は `--os-version` 省略で
`uname -r` から自動検出される。

### BSD 向けの feature セット（`full-freebsd` / `full-openbsd`）

BSD 向けパッケージは `full` ではなく **BSD 専用の feature セット**でビルドする
（`Cargo.toml` の `[features]`）。機能セットは `full` と同一で、**アロケータと
FreeBSD 専用 I/O 経路だけが異なる**。cargo にはターゲット別の default features が
無いため、packaging のスクリプト側で `--no-default-features` と併せて明示指定する。

| セット | アロケータ | 追加 | 使う場所 |
|---|---|---|---|
| `full`（既定） | mimalloc | — | Linux / macOS / Windows |
| `full-freebsd` | **jemalloc** | **`aio`**（POSIX AIO 経路、F-127） | `build-cross.sh --target freebsd` / `bsd-vm.sh freebsd …` |
| `full-openbsd` | **システムアロケータ**（`global_allocator` を差し替えない） | — | `bsd-vm.sh openbsd …` |

通常の `cargo build --features full` の挙動は従来どおり（mimalloc・AIO 無効）で変わらない。
`CARGO_FEATURES` 環境変数で上書きもできる。

> **FreeBSD の Docker クロスビルドは現在通りません（B-49、未解決）**
>
> `docker/Dockerfile.freebsd` と `build-cross.sh --target freebsd` は用意してあり、
> Rust のコンパイルまでは通る（`cargo-zigbuild` は FreeBSD libc を同梱し
> `x86_64-unknown-freebsd` は Rust Tier 2）が、**リンク段で失敗する**。
> aws-lc-sys の s2n-bignum アセンブリが FreeBSD クロス構成で 1 つも組み立てられず、
> `undefined symbol: curve25519_x25519_byte` などが多数出る。
> cc ビルダ強制（libssl 非対応）・`CMAKE_SYSTEM_NAME` の調整・`AWS_LC_SYS_NO_ASM`
> （release では禁止）をいずれも試したが未解決で、aws-lc-sys 側の対応が要る。
> 詳細は [B-49](../docs/backlog/bugs/B-49-awslc-freebsd-cross-missing-s2n-bignum-asm.md)。
>
> **FreeBSD は x86_64 / aarch64 とも上記の QEMU VM 内ネイティブビルドを使うこと**
> （VM 内はネイティブ clang が `.S` を組み立てるため本問題の影響を受けない）。
> B-49 が解決すれば、x86_64 は Docker クロスビルド + VM で E2E だけ、という運用に
> 切り替えられる:
>
> ```bash
> ./packaging/scripts/build-cross.sh --target freebsd
> tools/qemu/bsd-vm.sh freebsd x86_64 e2e \
>   --prebuilt packaging/build/artifact-x86_64-unknown-freebsd/veil
> ```

`aarch64-unknown-freebsd` は Rust Tier 3（prebuilt std 無し）のため、B-49 の解決有無に
かかわらず VM 内ネイティブビルドのみ。

tar.gz には `veil` バイナリ・`rc.d/veil`（サービススクリプト）・`config.toml.default`・
`www/index.html`・`INSTALL.txt`・`BUILD_INFO.txt`（+ FreeBSD は `jail.conf.sample`）を
同梱する。`BUILD_INFO.txt` / `INSTALL.txt` には **ビルドした OS のバージョン**
（例 FreeBSD 14.3-RELEASE / OpenBSD 7.6）・ビルド日時・rustc バージョンを明記する
（ABI 互換の目安。大きく異なる OS バージョンでは再ビルド推奨）。
FreeBSD は capsicum（`[security] enable_capsicum`）・jail と、OpenBSD は
pledge/unveil（`[security] enable_pledge` / `enable_unveil`）と併用できる。
OpenBSD の TLS は rustls の ring プロバイダを使用し（F-122）、`full-openbsd`
（HTTP/3 + WASM 含む全機能。アロケータはシステム malloc）でのビルドに対応している。
静的配信/プロキシとも HTTPS 200 で動作する（pledge+unveil 有効のまま）。

> **FreeBSD の HTTP/3 に関する既知の制限**: FreeBSD では `http3_enabled = true` でも
> QUIC の UDP ポートが bind されず HTTP/3 が機能しない（**B-50**、未修正）。
> 配布物は `full-freebsd` でビルドされ http3 を含むが、実際には使えない点に注意。
> HTTP/1.1・HTTP/2・gRPC・WebSocket・L4 は動作する（VM 内 E2E で 416 件通過）。

### macOS 向けパッケージ（F-125、Docker クロスビルド）

macOS は Docker（`messense/cargo-zigbuild`）で **universal2（x86_64 + aarch64 の
fat binary）をクロスビルド**できる。FreeBSD/OpenBSD と異なり VM ネイティブビルドは
不要。QEMU 実行・実機検証は行っていない（クロスビルドが通ることのみを合格基準と
する。設計は `docs/artifacts/f125_windows_macos_design.md`）。
デフォルトで `--features full`（HTTP/3 + WASM 含む全機能）でビルドされる。

```bash
./packaging/scripts/build-cross.sh --target macos
```

内部では [`docker/Dockerfile.macos`](../docker/Dockerfile.macos)（`messense/cargo-zigbuild`
ベース、`Dockerfile.glibc` と同じ cacher/builder 2 段構成）を
`--target artifact --output type=local` でビルドし、`cargo zigbuild --release --target
universal2-apple-darwin --features full` の成果物を取り出す。2 段構成のため、ソース
変更だけの再ビルドでは `aws-lc-sys` / `boring-sys` の重い C ビルドがレイヤキャッシュ
から再利用される（B-47）。macOS は
rustls の暗号プロバイダに **aws_lc_rs** を使い（`Cargo.toml` の target 別依存、F-131）、
`http3` (quiche) は内蔵 BoringSSL (`boring-sys`) を独立して使用しシンボル分離されている。

tar.gz には `veil` バイナリ（universal2 fat binary）・`config.toml.default`・
`www/index.html`・`INSTALL.txt` を同梱する。macOS ネイティブのセキュリティは
`sandbox_init`（Seatbelt、`[security] enable_sandbox_macos`）。実機検証ができない
ため保守的な最小プロファイル（ネットワーク・ファイル読み取りは無条件許可、
書き込みのみログ/キャッシュディレクトリへ限定）を採用している
（`src/security.rs` の `macos_sandbox` モジュール参照）。

### Windows 向けパッケージ（F-125、v0.6.0、Docker クロスビルド）

Windows は Docker（`messense/cargo-xwin`）で **x86_64-pc-windows-msvc** と
**aarch64-pc-windows-msvc** を個別にクロスビルドできる（FreeBSD/OpenBSD と異なり
VM ネイティブビルドは不要）。QEMU 実行・実機検証は行っていない
（クロスビルドが通ることのみを合格基準とする。設計は
`docs/artifacts/f125_windows_macos_design.md`）。
デフォルトで `--features full`（HTTP/3 + WASM 含む全機能）でビルドされる。

```bash
./packaging/scripts/build-cross.sh --target windows
```

内部では [`docker/Dockerfile.windows`](../docker/Dockerfile.windows)（`messense/cargo-xwin`
ベース、`Dockerfile.glibc` と同じ cacher/builder 2 段構成）を
`--target artifact --output type=local` でビルドし、
`cargo xwin build --release --target <target> --features full` を x86_64/aarch64 両方に
対して実行して、それぞれ zip を出力する。2 段構成により `aws-lc-sys` / `boring-sys` の
C ビルドと xwin の Windows SDK 取得がレイヤキャッシュに残る（B-47）。rustls の暗号プロバイダは **x86_64 / aarch64 ともに aws_lc_rs**
を使用し（`Cargo.toml` の target 別依存、F-131）、`http3` (quiche) には BoringSSL (`boring-sys`) を、
UDP ソケットには Windows Winsock 互換（`QuicUdpSocket`）が適用されている。`l4-proxy` も対応済みである。

zip には `veil.exe`・`config.toml.default`・`www/index.html`・`INSTALL.txt` を
同梱する。Windows ネイティブのセキュリティは Job Object（best-effort、
`[security] enable_job_object_windows`）。`CreateJobObjectW` +
`SetInformationJobObject` でプロセスに最小限のリソース制限
（`ACTIVE_PROCESS=1`、`KILL_ON_JOB_CLOSE`）を適用するのみで、seccomp/Landlock
相当のシステムコールフィルタではない。

**`AWS_LC_SYS_NO_PREFIX` について（B-47）**: `http3` / `full` ビルドで
`aws-lc-sys` と `quiche` のシンボルをどう扱うかは
**`.cargo/config.toml` の `[env]`（ターゲット接尾辞付き変数）が唯一の設定箇所**である
（Linux/FreeBSD = `1`、Windows/macOS/OpenBSD = `0`）。cargo にターゲット別 env の仕組みが
無いため（`[target.<triple>.env]` は黙って無視される）、`aws-lc-sys` が優先して読む
`AWS_LC_SYS_NO_PREFIX_<triple_with_underscores>` を列挙している。
packaging のスクリプトや Dockerfile 側でこの変数を設定してはならない。

**注意**: 各クロスビルドは専用 Dockerfile のビルドコンテキスト内で完結し、
ホストの `target/` を共有しない（`--output type=local` で成果物だけを取り出す）。
そのためホスト側の `cargo build` と同時に実行しても競合しない。

### 成果物

```
packaging/output/veil_<version>_<deb_arch>.deb          # deb_arch: amd64 / arm64
packaging/output/veil-<version>-1.<rpm_arch>.rpm        # rpm_arch: x86_64 / aarch64
packaging/output/veil-<version>-x86_64-unknown-linux-gnu.tar.gz
packaging/output/veil-<version>-x86_64-unknown-linux-musl.tar.gz
packaging/output/veil-<version>-x86_64-unknown-freebsd.tar.gz    # build-bsd.sh（QEMU VM ビルド）
packaging/output/veil-<version>-aarch64-unknown-freebsd.tar.gz   # build-bsd.sh（QEMU VM ビルド）
packaging/output/veil-<version>-x86_64-unknown-openbsd.tar.gz    # build-bsd.sh（QEMU VM ビルド）
packaging/output/veil-<version>-aarch64-unknown-openbsd.tar.gz   # build-bsd.sh（QEMU VM ビルド）
packaging/output/veil-<version>-x86_64-unknown-freebsd.tar.gz   # build-cross.sh --target freebsd（B-49 により現在失敗）
packaging/output/veil-<version>-universal2-apple-darwin.tar.gz # build-cross.sh --target macos
packaging/output/veil-<version>-x86_64-pc-windows-msvc.zip      # build-cross.sh --target windows
packaging/output/veil-<version>-aarch64-pc-windows-msvc.zip     # build-cross.sh --target windows
```

`<version>` は **`Cargo.toml` の `[package] version` からビルド時に自動取得**され、
ファイル名・deb の `Version:` フィールド（`debian/DEBIAN/control` はプレースホルダ
`__VERSION__` を持つテンプレート）・rpm の `%{veil_version}` マクロへ反映されます。
リリース時に packaging/ 配下のファイルを手動更新する必要はありません。

tar.gz の中身は次の単一ディレクトリです（展開後 `veil` バイナリのみ）:

```
veil-<version>-<target>/
└── veil
```

例:

```
packaging/output/veil_0.6.0_amd64.deb
packaging/output/veil-0.6.0-1.x86_64.rpm
packaging/output/veil-0.6.0-x86_64-unknown-linux-gnu.tar.gz
packaging/output/veil-0.6.0-x86_64-unknown-linux-musl.tar.gz
```

### ビルド処理の流れ

1. glibc バイナリ生成（`cargo zigbuild` / ホスト cargo、または `Dockerfile.glibc`）
2. musl バイナリ生成（`cargo` musl ターゲット、または `Dockerfile.musl`）
3. スタンドアロン tar.gz を `packaging/output/` に出力（glibc / musl 各1）
4. 共通ルートファイルシステムをステージング（glibc バイナリを使用）
   - `/usr/bin/veil`
   - `/usr/share/veil/config.toml.default`
   - `/usr/share/veil/www/index.html`
   - `/usr/share/veil/scripts/{postinstall,preuninstall}.sh`
   - `/lib/systemd/system/veil.service`
5. `dpkg-deb` で `.deb` を生成
6. `rpmbuild` で `.rpm` を生成

## インストール

### Debian / Ubuntu

```bash
sudo dpkg -i packaging/output/veil_0.6.0_amd64.deb
sudo apt-get install -f
sudo systemctl enable --now veil
```

### Amazon Linux 2023

```bash
sudo dnf install -y packaging/output/veil-0.6.0-1.x86_64.rpm
sudo systemctl enable --now veil
```

### postinstall が行うこと

| 処理 | 詳細 |
|------|------|
| ユーザー作成 | `veil` ユーザー / グループ（未存在時のみ） |
| ディレクトリ作成 | `/var/www`, `/var/log/veil`, `/var/cache/veil`, `/var/tmp/veil`, `/var/etc/veil` |
| 設定配置 | `/var/etc/veil/config.toml`（既存がなければ `config.toml.default` をコピー） |
| サンプル HTML | `/var/www` が存在しなかった場合のみ `index.html` を配置 |
| TLS 証明書 | `/var/etc/veil/ssl/` に自己署名証明書を生成（未存在時のみ） |
| 権限設定 | ログ・キャッシュ・一時ディレクトリを `veil:veil`、設定を `root:veil` に |

## 動作確認

### 両パッケージを一括検証

```bash
./packaging/scripts/build.sh          # または Docker ビルド
./packaging/scripts/test-install.sh
```

### Debian/Ubuntu（.deb）のみ

```bash
./packaging/scripts/test-deb.sh
```

### Amazon Linux 2023（.rpm）のみ

```bash
./packaging/scripts/test-rpm.sh
```

検証内容（共通）:

1. systemd 入りコンテナ起動
2. パッケージのインストール
3. `systemctl enable` / `systemctl start veil`
4. コンテナ内 `curl` で HTTP リダイレクト（80）と HTTPS 応答（443）を確認

## systemd ユニットの設計上の注意

`contrib/systemd/veil.service` の方針:

- **SystemCallFilter なし** — `config.toml` の seccomp / Landlock と競合するため
- **MemoryDenyWriteExecute なし** — `full` ビルドの WASM 実行に必要
- **LogsDirectory / CacheDirectory** — `/var/log/veil`, `/var/cache/veil`
- **ReadOnlyPaths=/var/etc/veil** — 設定・証明書は読み取り専用
- **AmbientCapabilities=CAP_NET_BIND_SERVICE** — 特権ポート（80/443）を `veil` ユーザーでバインド

## トラブルシューティング

```bash
sudo journalctl -u veil --no-pager -n 50
sudo tail -50 /var/log/veil/veil.error-*.log
```

| 症状 | 原因 | 対処 |
|------|------|------|
| `Permission denied`（設定読込） | TLS 鍵の権限 | `key.pem` を `veil:veil` に変更 |
| `Landlock/seccomp failed` | systemd SystemCallFilter 併用 | ユニットから削除（現行版は対応済み） |
| `NAMESPACE` エラー | ReadWritePaths で存在しないパス | LogsDirectory/CacheDirectory を使用（現行版） |
| `rpmbuild: command not found` | rpm 未インストール | `apt install rpm` または Docker ビルドを使用 |

## 関連ファイル

| パス | 役割 |
|------|------|
| [contrib/config/config.toml](../contrib/config/config.toml) | パッケージ用デフォルト設定 |
| [contrib/systemd/veil.service](../contrib/systemd/veil.service) | systemd ユニット |
| [docker/Dockerfile.glibc](../docker/Dockerfile.glibc) | glibc 配布バイナリビルド |
| [docker/Dockerfile.musl](../docker/Dockerfile.musl) | musl 配布バイナリビルド |
| [docker/Dockerfile.macos](../docker/Dockerfile.macos) | macOS universal2 クロスビルド（キャッシュ有効） |
| [docker/Dockerfile.windows](../docker/Dockerfile.windows) | Windows x86_64/aarch64 クロスビルド（キャッシュ有効） |
| [docker/Dockerfile.freebsd](../docker/Dockerfile.freebsd) | FreeBSD x86_64 クロスビルド（キャッシュ有効） |
| [tools/qemu/bsd-vm.sh](../tools/qemu/README.md) | FreeBSD/OpenBSD × x86_64/aarch64 の VM ビルド・E2E・バイナリ取得（`<os> <arch> all` で一括） |
| [packaging/scripts/build-bsd.sh](scripts/build-bsd.sh) | 上記の取得物を tar.gz 化（`--from-qemu` / `--all`） |
| [examples/config.toml](../examples/config.toml) | 設定リファレンス |