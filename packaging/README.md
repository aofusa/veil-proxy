# Veil Linux パッケージ（.deb / .rpm）

Debian/Ubuntu 向け `.deb` と Amazon Linux 2023 向け `.rpm` を生成・検証するためのディレクトリです。

## 概要

| 項目 | 内容 |
|------|------|
| パッケージ形式 | `.deb`（Debian/Ubuntu）、`.rpm`（Amazon Linux 2023） |
| ビルドスクリプト | `packaging/scripts/build.sh`（両方を一括生成） |
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
│   ├── test-install.sh          # 両パッケージを順に検証
│   ├── test-deb.sh              # .deb 検証
│   ├── test-rpm.sh              # .rpm 検証
│   ├── postinstall.sh           # deb/rpm 共通インストール後処理
│   ├── preuninstall.sh          # deb/rpm 共通アンインストール前処理
│   └── docker/
│       ├── Dockerfile.build     # バイナリのみ Docker ビルド
│       ├── Dockerfile.package   # Docker 内でパッケージまで一括ビルド
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

ホストに Rust ツールチェーンがなくても、Docker だけでビルドできます。
[docker/Dockerfile.glibc](../docker/Dockerfile.glibc) と同様に `messense/cargo-zigbuild` を使用し、glibc 2.28 互換バイナリを生成します。

```bash
# パッケージ一式を Docker 内でビルド
docker build -f packaging/scripts/docker/Dockerfile.package -t veil-package:local .

# 成果物をホストへコピー
cid=$(docker create veil-package:local)
docker cp "$cid:/app/packaging/output/." packaging/output/
docker rm "$cid"
```

バイナリのみ Docker でビルドし、パッケージングはホストで行う場合:

```bash
./packaging/scripts/build.sh --docker
```

### テスト

- Docker
- ネットワーク（初回イメージ取得）

## ビルド

### 推奨: Docker でポータブルビルド（glibc 2.28 互換）

Debian/Ubuntu と Amazon Linux 2023 の両方で動作するバイナリを生成します。
[docker/Dockerfile.glibc](../docker/Dockerfile.glibc) と同じ `messense/cargo-zigbuild` を使用します。

```bash
./packaging/scripts/build.sh --docker
```

### ローカル（ネイティブ cargo / cargo zigbuild）

```bash
./packaging/scripts/build.sh
```

`cargo zigbuild` が利用可能な場合は glibc 2.28 向けに自動ビルドします。
利用できない場合はホスト glibc でビルドされ、Amazon Linux 2023 では動作しない可能性があります。

### 既存バイナリからパッケージのみ生成

```bash
cargo build --release --features full
./packaging/scripts/build.sh --skip-build --binary target/release/veil
```

### 成果物

```
packaging/output/veil_<version>_<deb_arch>.deb
packaging/output/veil-<version>-1.<rpm_arch>.rpm
```

`<version>` は **`Cargo.toml` の `[package] version` からビルド時に自動取得**され、
ファイル名・deb の `Version:` フィールド（`debian/DEBIAN/control` はプレースホルダ
`__VERSION__` を持つテンプレート）・rpm の `%{veil_version}` マクロへ反映されます。
リリース時に packaging/ 配下のファイルを手動更新する必要はありません。

例:

```
packaging/output/veil_0.5.0_amd64.deb
packaging/output/veil-0.5.0-1.x86_64.rpm
```

### ビルド処理の流れ

1. `cargo build --release --features full`（または Docker zigbuild）でバイナリ生成
2. 共通ルートファイルシステムをステージング
   - `/usr/bin/veil`
   - `/usr/share/veil/config.toml.default`
   - `/usr/share/veil/www/index.html`
   - `/usr/share/veil/scripts/{postinstall,preuninstall}.sh`
   - `/lib/systemd/system/veil.service`
3. `dpkg-deb` で `.deb` を生成
4. `rpmbuild` で `.rpm` を生成

## インストール

### Debian / Ubuntu

```bash
sudo dpkg -i packaging/output/veil_0.5.0_amd64.deb
sudo apt-get install -f
sudo systemctl enable --now veil
```

### Amazon Linux 2023

```bash
sudo dnf install -y packaging/output/veil-0.5.0-1.x86_64.rpm
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
5. ホストポートマッピング確認（deb: 18080/18443、rpm: 28080/28443）

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
| [docker/Dockerfile.glibc](../docker/Dockerfile.glibc) | 配布用バイナリビルドの参考実装 |
| [examples/config.toml](../examples/config.toml) | 設定リファレンス |