# Veil Linux パッケージ（.deb）

Debian/Ubuntu 向けの `.deb` インストールパッケージを生成・検証するためのディレクトリです。

## 概要

| 項目 | 内容 |
|------|------|
| パッケージ形式 | `.deb`（`dpkg-deb`） |
| デフォルトビルド | `--features full`（全オプションフィーチャー有効） |
| 設定ファイル | `contrib/config/config.toml`（`examples/config.toml` ベース） |
| 設定配置先 | `/var/etc/veil/config.toml` |
| systemd ユニット | `contrib/systemd/veil.service` |
| 実行ユーザー | `veil:veil` |

## ディレクトリ構成

```
packaging/
├── README.md                 # 本ファイル
├── build-deb.sh              # .deb ビルドスクリプト
├── test-docker-install.sh    # Ubuntu Docker での E2E 検証
├── Dockerfile.test            # systemd 入りテスト用 Ubuntu イメージ
├── debian/DEBIAN/
│   ├── control               # パッケージメタデータ
│   ├── postinst              # インストール後処理
│   └── prerm                 # アンインストール前処理
├── staging/                  # ビルド作業ディレクトリ（.gitignore）
└── output/                   # 生成された .deb（.gitignore）
```

## 前提条件

### ビルドホスト

- Linux（`dpkg-deb` が利用可能であること）
- Rust ツールチェーン（`cargo`）
- `full` フィーチャービルド用: `cmake`, `nasm`（[README.md](../README.md) の Build 節を参照）

### テスト（Docker E2E）

- Docker
- ネットワークアクセス（初回のみ `ubuntu:24.04` イメージの取得）

## ビルド

リポジトリルートから実行します。

```bash
./packaging/build-deb.sh
```

処理内容:

1. `cargo build --release --features full --locked` でバイナリをビルド
2. ステージングツリーへ以下を配置
   - `/usr/bin/veil` — バイナリ
   - `/usr/share/veil/config.toml.default` — デフォルト設定
   - `/usr/share/veil/www/index.html` — サンプルページ（`docker/assets/www/index.html`）
   - `/lib/systemd/system/veil.service` — systemd ユニット
   - `/var/log/veil`, `/var/cache/veil`, `/var/tmp/veil` — 空ディレクトリ
3. `dpkg-deb` で `.deb` を生成

成果物:

```
packaging/output/veil_<version>_<arch>.deb
```

例: `packaging/output/veil_0.4.0_amd64.deb`

## インストール（本番）

```bash
sudo dpkg -i packaging/output/veil_0.4.0_amd64.deb
# 依存関係不足時
sudo apt-get install -f

sudo systemctl enable --now veil
```

### postinst が行うこと

| 処理 | 詳細 |
|------|------|
| ユーザー作成 | `veil` ユーザー / グループ（未存在時のみ） |
| ディレクトリ作成 | `/var/www`, `/var/log/veil`, `/var/cache/veil`, `/var/tmp/veil`, `/var/etc/veil` |
| 設定配置 | `/var/etc/veil/config.toml`（既存がなければ `config.toml.default` をコピー） |
| サンプル HTML | `/var/www` が存在しなかった場合のみ `index.html` を配置 |
| TLS 証明書 | `/var/etc/veil/ssl/` に自己署名証明書を生成（未存在時のみ） |
| 権限設定 | ログ・キャッシュ・一時ディレクトリを `veil:veil` に、設定を `root:veil` に |

> `/var/www` が既に存在する場合は既存ディレクトリをそのまま使用し、`index.html` は上書きしません。

## 動作確認

### Docker E2E テスト（推奨）

ビルド後、Ubuntu 24.04 コンテナ内でインストール・起動・curl 検証を自動実行します。

```bash
./packaging/test-docker-install.sh
```

検証内容:

1. systemd 入り Ubuntu コンテナ起動
2. `.deb` のインストール（`dpkg -i`）
3. `systemctl enable` / `systemctl start veil`
4. コンテナ内 `curl` で HTTP リダイレクト（80）と HTTPS 応答（443）を確認
5. ホストポート `18080` / `18443` へのマッピング確認

成功時は `All checks passed` と表示されます。

カスタムテストイメージを使う場合:

```bash
VEIL_TEST_IMAGE=my-veil-test:latest ./packaging/test-docker-install.sh
```

### 手動確認

```bash
# サービス状態
sudo systemctl status veil

# HTTP → HTTPS リダイレクト
curl -skI http://127.0.0.1/

# HTTPS コンテンツ
curl -sk https://127.0.0.1/ | head

# ログ
sudo journalctl -u veil -f
tail -f /var/log/veil/veil-*.log
```

## 設定のカスタマイズ

インストール後の設定ファイル:

```
/var/etc/veil/config.toml
```

テンプレート（パッケージ同梱、上書きされない）:

```
/usr/share/veil/config.toml.default
```

設定変更後のリロード:

```bash
sudo systemctl reload veil
# または
sudo kill -HUP $(systemctl show -p MainPID --value veil)
```

TLS 証明書のパス（デフォルト）:

```
/var/etc/veil/ssl/cert.pem
/var/etc/veil/ssl/key.pem
```

## systemd ユニットの設計上の注意

`contrib/systemd/veil.service` では次の方針を採用しています。

- **SystemCallFilter なし** — `config.toml` の seccomp / Landlock と競合し、起動失敗の原因になるため
- **MemoryDenyWriteExecute なし** — `full` ビルドの WASM 実行に必要な実行可能メモリを確保するため
- **LogsDirectory / CacheDirectory** — `/var/log/veil`, `/var/cache/veil` を systemd が管理
- **AmbientCapabilities=CAP_NET_BIND_SERVICE** — 特権ポート（80/443）へのバインドを `veil` ユーザーで許可

## アンインストール

```bash
sudo systemctl stop veil
sudo dpkg -r veil
```

`prerm` がサービスの停止・無効化を行います。設定ファイル（`/var/etc/veil/config.toml`）やログは自動削除されません。

## トラブルシューティング

### サービスが起動しない

```bash
sudo journalctl -u veil --no-pager -n 50
sudo tail -50 /var/log/veil/veil.error-*.log
```

よくある原因:

| 症状 | 原因 | 対処 |
|------|------|------|
| `Permission denied`（設定読込） | TLS 鍵の権限 | `postinst` を再実行するか `key.pem` を `veil:veil` に変更 |
| `Landlock/seccomp failed` | systemd の SystemCallFilter 併用 | ユニットから SystemCallFilter を削除（現行版は対応済み） |
| `NAMESPACE` エラー | ReadWritePaths で存在しないパスを指定 | 現行版は LogsDirectory/CacheDirectory を使用 |

### ビルドが遅い / 失敗する

`--features full` は quiche（BoringSSL）と wasmtime を含むため初回ビルドに時間がかかります。`cmake` と `nasm` がインストールされていることを確認してください。

## 関連ファイル

| パス | 役割 |
|------|------|
| [contrib/config/config.toml](../contrib/config/config.toml) | パッケージ用デフォルト設定 |
| [contrib/systemd/veil.service](../contrib/systemd/veil.service) | systemd ユニット |
| [examples/config.toml](../examples/config.toml) | 設定リファレンス（全キー網羅） |
| [docker/assets/www/index.html](../docker/assets/www/index.html) | サンプル `index.html` の元 |