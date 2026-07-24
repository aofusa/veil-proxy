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
│   ├── build-cross.sh           # macOS universal2 tar.gz（Docker クロスビルド、F-125）
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

FreeBSD/OpenBSD は Rust Tier 2/3 かつクロスビルドが困難なため、バイナリは
**QEMU VM 内でネイティブビルド**したものを取り出し、専用スクリプトで
rc.d サービススクリプト・設定リファレンス・（FreeBSD は）jail.conf サンプルを
同梱した tar.gz を生成する（deb/rpm は Linux 専用のため BSD は tar.gz のみ）。

```bash
# VM でビルドしたバイナリを host へ持ち出してから（--os-version でビルド OS バージョンを明記）:
./packaging/scripts/build-bsd.sh --os freebsd --arch x86_64 --binary ./veil-freebsd-amd64 --os-version 14.3-RELEASE
./packaging/scripts/build-bsd.sh --os openbsd --arch x86_64 --binary ./veil-openbsd-amd64 --os-version 7.6
# aarch64 も --arch aarch64 で対応（VM 内 aarch64 ネイティブビルドが前提）
# 対象 OS の VM 内で直接実行する場合は --os-version 省略で uname -r から自動検出される。
```

tar.gz には `veil` バイナリ・`rc.d/veil`（サービススクリプト）・`config.toml.default`・
`www/index.html`・`INSTALL.txt`・`BUILD_INFO.txt`（+ FreeBSD は `jail.conf.sample`）を
同梱する。`BUILD_INFO.txt` / `INSTALL.txt` には **ビルドした OS のバージョン**
（例 FreeBSD 14.3-RELEASE / OpenBSD 7.6）・ビルド日時・rustc バージョンを明記する
（ABI 互換の目安。大きく異なる OS バージョンでは再ビルド推奨）。
FreeBSD は capsicum（`[security] enable_capsicum`）・jail と、OpenBSD は
pledge/unveil（`[security] enable_pledge` / `enable_unveil`）と併用できる。
OpenBSD の TLS は rustls の ring プロバイダを使用し（F-122）、`full` フィーチャー（HTTP/3 + WASM 含む全機能）でのビルドに対応している。静的配信/プロキシとも HTTPS 200 で動作する（pledge+unveil 有効のまま）。

### macOS 向けパッケージ（F-125、Docker クロスビルド）

macOS は Docker（`messense/cargo-zigbuild`）で **universal2（x86_64 + aarch64 の
fat binary）をクロスビルド**できる。FreeBSD/OpenBSD と異なり VM ネイティブビルドは
不要。QEMU 実行・実機検証は行っていない（クロスビルドが通ることのみを合格基準と
する。設計は `docs/artifacts/f125_windows_macos_design.md`）。
デフォルトで `--features full`（HTTP/3 + WASM 含む全機能）でビルドされる。

```bash
./packaging/scripts/build-cross.sh --target macos
```

内部では `messense/cargo-zigbuild` イメージ内で `cmake` + `nasm` を導入し、
`cargo zigbuild --release --target universal2-apple-darwin --features full` を実行する。macOS は
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

内部では `messense/cargo-xwin` イメージ内で `cmake` + `nasm` を自動セットアップし、
`cargo xwin build --release --target <target> --features full` を x86_64/aarch64 両方に
対して実行し、それぞれ zip を出力する。rustls の暗号プロバイダは **x86_64 / aarch64 ともに aws_lc_rs**
を使用し（`Cargo.toml` の target 別依存、F-131）、`http3` (quiche) には BoringSSL (`boring-sys`) を、
UDP ソケットには Windows Winsock 互換（`QuicUdpSocket`）が適用されている。`l4-proxy` も対応済みである。

zip には `veil.exe`・`config.toml.default`・`www/index.html`・`INSTALL.txt` を
同梱する。Windows ネイティブのセキュリティは Job Object（best-effort、
`[security] enable_job_object_windows`）。`CreateJobObjectW` +
`SetInformationJobObject` でプロセスに最小限のリソース制限
（`ACTIVE_PROCESS=1`、`KILL_ON_JOB_CLOSE`）を適用するのみで、seccomp/Landlock
相当のシステムコールフィルタではない。

**注意**: Docker ビルドはリポジトリを `/io` としてマウントし `target/` を共有する
ため、ホスト側の他の `cargo build` と同時に実行しないこと（target 競合）。

### 成果物

```
packaging/output/veil_<version>_<deb_arch>.deb          # deb_arch: amd64 / arm64
packaging/output/veil-<version>-1.<rpm_arch>.rpm        # rpm_arch: x86_64 / aarch64
packaging/output/veil-<version>-x86_64-unknown-linux-gnu.tar.gz
packaging/output/veil-<version>-x86_64-unknown-linux-musl.tar.gz
packaging/output/veil-<version>-<arch>-unknown-freebsd.tar.gz   # build-bsd.sh
packaging/output/veil-<version>-<arch>-unknown-openbsd.tar.gz   # build-bsd.sh
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
| [examples/config.toml](../examples/config.toml) | 設定リファレンス |