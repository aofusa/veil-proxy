[English](../../README.md) | [日本語](README.ja.md)

<p align="center">
  <img src="../images/veil_logo.webp" alt="Veil Logo" width="300" align="middle" />
  &nbsp;&nbsp;&nbsp;
  <img src="../images/veil_logo_text.svg" alt="Veil" height="50" align="middle" />
</p>

# Veil - 高性能リバースプロキシサーバー

Veil は Rust で書かれたリバースプロキシ兼 Web サーバーです。データプレーンは独自の
io_uring ランタイム（tokio/monoio 非依存）と rustls・カーネル TLS（任意）の上で動き、
HTTP/1.1・HTTP/2（TLS / h2c）・HTTP/3（QUIC）・gRPC・WebSocket・生の TCP/UDP（L4）を扱えます。

## 特徴

- **高速なデータプレーン** — Linux は io_uring（他 OS は epoll/kqueue/WSAPoll）、
  `splice(2)`/`sendfile(2)` によるゼロコピー、kTLS オフロード、`SO_REUSEPORT` による
  スレッドごとのワーカー。
- **プロトコル** — HTTP/1.1・HTTP/2・h2c・HTTP/3・gRPC / gRPC-Web・WebSocket・L4 TCP/UDP
  プロキシ・Unix ドメインソケットのリスナーとバックエンド。
- **プロキシ機能** — パス/ホスト/ヘッダーによるルーティング、ロードバランシング、
  ヘルスチェック、サーキットブレーカー、リトライ、レスポンス圧縮、プロキシキャッシュ、
  バッファリング、ヘッダー書き換え。
- **拡張性** — 全プロトコルで Proxy-Wasm フィルタ（Wasmtime）。
- **運用性** — 設定・証明書のホットリロード（`SIGHUP`）、Prometheus メトリクス、
  OpenTelemetry、構造化アクセスログ、管理 API。
- **既定で安全** — メモリ安全な Rust、seccomp/Landlock（Linux）、capsicum（FreeBSD）、
  pledge/unveil（OpenBSD）、Seatbelt（macOS）、特権降格。
- **クロスプラットフォーム** — Linux・FreeBSD・OpenBSD・NetBSD・macOS・Windows
  （x86_64 / aarch64）。

nginx との性能比較は [docs/perf/README.md](../perf/README.md) にあります。

## インストール

ビルド済みバイナリとパッケージ（`.deb`・`.rpm`、Linux/BSD/macOS/Windows 向け tarball）は
各 [GitHub リリース](https://github.com/aofusa/veil-proxy/releases) に添付しています。

ソースからビルドする場合（Rust stable、`cmake`、`nasm` が必要）:

```bash
cargo build --release                    # 既定: kTLS + HTTP/2 + mimalloc
cargo build --release --features full    # 全機能（HTTP/3・gRPC・WASM・キャッシュ・メトリクス等）
```

バイナリは `target/release/veil` に生成されます。feature の詳細・パッケージ作成・
BSD/macOS/Windows 向けビルドは [docs/guide/ja/building.md](../guide/ja/building.md) を参照してください。

## クイックスタート

1. 証明書を用意する（テスト用の自己署名）:

   ```bash
   openssl req -x509 -newkey ec -pkeyopt ec_paramgen_curve:secp384r1 -nodes \
     -keyout key.pem -out cert.pem -days 365 -subj "/CN=localhost"
   ```

2. `config.toml` を書く — `/static/` はディスクから配信し、それ以外はアプリへプロキシする:

   ```toml
   [server]
   listen = "0.0.0.0:443"
   http2_enabled = true

   [tls]
   cert_path = "/etc/veil/ssl/cert.pem"
   key_path  = "/etc/veil/ssl/key.pem"

   # /static/ 配下は静的ファイル
   [[route]]
   [route.conditions]
   path = "/static/*"
   [route.action]
   type = "File"
   path = "/var/www/"

   # それ以外はアプリケーションへ
   [[route]]
   [route.conditions]
   path = "/*"
   [route.action]
   type = "Proxy"
   url = "http://127.0.0.1:8080"
   ```

3. 検証して起動する:

   ```bash
   veil -t -c config.toml   # 設定の検証
   veil -c config.toml      # 起動（SIGHUP でリロード、SIGTERM で graceful shutdown）
   curl -k https://localhost/static/index.html
   ```

全設定キーをコメント付きで網羅したリファレンスは [examples/config.toml](../../examples/config.toml) です。

## ドキュメント

| トピック | ガイド |
|----------|--------|
| 機能一覧 | [docs/guide/ja/features.md](../guide/ja/features.md) |
| プラットフォーム・ランタイムバックエンド | [docs/guide/ja/platforms.md](../guide/ja/platforms.md) |
| ビルドとパッケージ | [docs/guide/ja/building.md](../guide/ja/building.md) |
| 起動・設定検証・リロード・停止 | [docs/guide/ja/running.md](../guide/ja/running.md) |
| 設定リファレンス | [docs/guide/ja/configuration.md](../guide/ja/configuration.md) |
| TLS・kTLS・証明書リロード | [docs/guide/ja/tls.md](../guide/ja/tls.md) |
| ルーティング・リダイレクト・ヘッダー | [docs/guide/ja/routing.md](../guide/ja/routing.md) |
| ロードバランシング・ヘルスチェック・レジリエンス | [docs/guide/ja/load-balancing.md](../guide/ja/load-balancing.md) |
| L4 ストリームプロキシ | [docs/guide/ja/l4-proxy.md](../guide/ja/l4-proxy.md) |
| HTTP/2・HTTP/3・WebSocket | [docs/guide/ja/protocols.md](../guide/ja/protocols.md) |
| 圧縮・キャッシュ・バッファリング | [docs/guide/ja/compression-cache.md](../guide/ja/compression-cache.md) |
| WASM 拡張 | [docs/guide/ja/wasm.md](../guide/ja/wasm.md) |
| メトリクス・トレーシング・ログ | [docs/guide/ja/observability.md](../guide/ja/observability.md) |
| 管理 API・キャッシュ Purge API | [docs/guide/ja/admin-api.md](../guide/ja/admin-api.md) |
| セキュリティとサンドボックス | [docs/guide/ja/security.md](../guide/ja/security.md) |
| パフォーマンスチューニングとベンチマーク | [docs/guide/ja/performance.md](../guide/ja/performance.md) |
| テスト（開発者向け） | [docs/guide/ja/testing.md](../guide/ja/testing.md) |
| 参考資料・ロゴ | [docs/guide/ja/references.md](../guide/ja/references.md) |

コントリビュータは [AGENTS.md](../../AGENTS.md)（設計哲学・制約・作業フロー）から読んでください。

## ライセンス

以下のいずれかのライセンスの下で提供されます:

- Apache License, Version 2.0（[LICENSE-APACHE](../../LICENSE-APACHE)）
- MIT License（[LICENSE-MIT](../../LICENSE-MIT)）

利用者の選択によります。

(c) 2025 aofusa
