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
  プロキシ・1 プロセスで複数ポートの待ち受け（`[[listener]]`）・Unix ドメインソケットのリスナーとバックエンド。
- **プロキシ機能** — パス/ホスト/ヘッダーによるルーティング、ロードバランシング、
  ヘルスチェック、サーキットブレーカー、リトライ、レスポンス圧縮、プロキシキャッシュ、
  バッファリング、ヘッダー書き換え。
- **拡張性** — 全プロトコルで Proxy-Wasm フィルタ（Wasmtime）。
- **動的設定** — [VDS](../guide/ja/vds.md) で制御プレーンからルート・アップストリームを配信（xDS 方式の長輪詢・ACK/NACK）、
  組み込みの [ACME](../guide/ja/acme.md) で証明書を自動発行・更新（HTTP-01、Let's Encrypt）。
- **組み込み可能** — veil を[ライブラリ](../guide/ja/library.md)として使い、同じ io_uring データプレーン上で Rust（マクロ・OpenAPI・tower・GraphQL・WebSocket・multipart・L4 ハンドラ）・
  C（`libveil`）・Python（FastAPI 互換・ASGI）の API サーバを書けます。ヘルパーのパッケージ `veil-proxy-kit`・`veil-api-kit` と
  [本番運用ガイド](../guide/ja/library-production.md)もあります。
- **運用性** — 設定・証明書のホットリロード（`SIGHUP`）、Prometheus メトリクス、
  OpenTelemetry、構造化アクセスログ、管理 API。
- **既定で安全** — メモリ安全な Rust、seccomp/Landlock（Linux）、capsicum（FreeBSD）、
  pledge/unveil（OpenBSD）、Seatbelt（macOS）、特権降格。
- **クロスプラットフォーム** — Linux・FreeBSD・OpenBSD・NetBSD・macOS・Windows
  （x86_64 / aarch64）。

nginx との性能比較は [docs/perf/README.md](../perf/README.md) にあります。

## インストール

パッケージとコンテナイメージは、既定の設定とページも入れます。veil を起動して <http://127.0.0.1:8080/>
（コンテナは <http://localhost:8080/>）を開いてください。

| | コマンド |
|---|---|
| コンテナ | `docker run --rm -p 8080:8080 ghcr.io/aofusa/veil`（Docker Hub の `aofusa/veil` も同じ） |
| Debian・Ubuntu、RHEL・Fedora・Amazon Linux | [apt・dnf のリポジトリ](../guide/ja/running.md#aptdnf-のリポジトリ) を足して `apt install veil`・`dnf install veil` |
| Homebrew（macOS・Linux） | `brew install aofusa/tap/veil` |
| Windows | `winget install Aofusa.Veil` |
| FreeBSD・OpenBSD・NetBSD | [packaging/bsd/ports](../../packaging/bsd/ports/README.md) の ports |
| Rust | `cargo binstall veil-proxy`・`cargo install veil-proxy --locked --features full` |

`cargo install`・`cargo binstall`・各 [GitHub リリース](https://github.com/aofusa/veil-proxy/releases) のアーカイブで入るのは
`veil` のバイナリだけです。設定ファイルは自分で用意してください（アーカイブの `examples/` に例があります）。veil は
`-c FILE`、無ければ `VEIL_CONFIG`、無ければ既定のパス（`/etc/veil/config.toml`、Windows は `%ProgramData%\veil\config.toml`）を
読みます。詳しくは [設定ファイル](../guide/ja/running.md#設定ファイル)。

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

   コンテナイメージでも動かせる（`latest` は musl 版。glibc 版もある。`linux/amd64` と `linux/arm64`）。
   何もマウントしなければ、内蔵のページを 8080 番で配る:

   ```bash
   docker run --rm -p 8080:8080 ghcr.io/aofusa/veil      # http://localhost:8080/ を開く
   ```

   自分の設定・証明書・ファイルを使う場合（[コンテナのガイド](../guide/ja/container.md)）:

   ```bash
   docker run --rm -p 443:443 -p 443:443/udp \
     -v "$PWD/config.toml:/etc/veil/conf.d/config.toml:ro" \
     -v "$PWD/cert.pem:/etc/veil/ssl/cert.pem:ro" -v "$PWD/key.pem:/etc/veil/ssl/key.pem:ro" \
     ghcr.io/aofusa/veil:latest
   ```

   イメージは `full-container`（epoll の readiness reactor）のビルドなので、コンテナ既定の seccomp（Docker の既定・
   Kubernetes の `RuntimeDefault` は io_uring を禁じる）のまま動く。

全設定キーをコメント付きで網羅したリファレンスは [examples/config.toml](../../examples/config.toml) です。

### ライブラリとして使う

```toml
# Cargo.toml（crates.io の名前は veil-proxy。マクロが veil の名前を使うので、依存のキーは veil のままにする）
[dependencies]
veil = { package = "veil-proxy", version = "0.10", default-features = false, features = ["full", "api"] }
serde_json = "1"
```

`.cargo/config.toml` の設定は要らない（[ライブラリのガイド](../guide/ja/library.md#cargotoml)）。
Python は `pip install veil-api`（`import veil`）。

```rust
use veil::prelude::*;

#[veil::route(path = "/hello/{name}", method = "GET")]
async fn hello(name: String) -> Json<serde_json::Value> {
    Json(serde_json::json!({ "hello": name }))
}

#[veil::main]
async fn main() -> Result<(), veil::api::Error> {
    veil::serve(Config::load()?).await   // 127.0.0.1:8080 で平文 HTTP/1.1 + h2c、OpenAPI は /docs
}
```

詳しくは[ライブラリガイド](../guide/ja/library.md)（Rust・C API・Python・成果物のビルド）を参照してください。

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
| API サーバライブラリ（Rust・C・Python） | [docs/guide/ja/library.md](../guide/ja/library.md) |
| VDS: 制御プレーンからの動的ルート配信 | [docs/guide/ja/vds.md](../guide/ja/vds.md) |
| ACME: 証明書の自動発行・更新 | [docs/guide/ja/acme.md](../guide/ja/acme.md) |
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
