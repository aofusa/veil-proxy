[English](README.md) | [日本語](README.ja.md)

<p align="center">
  <img src="docs/images/veil_logo.webp" alt="Veil ロゴ" width="300" align="middle" />
  &nbsp;&nbsp;&nbsp;
  <img src="docs/images/veil_logo_text.svg" alt="Veil" height="50" align="middle" />
</p>

# Veil - High-Performance Reverse Proxy Server

io_uring (monoio) と rustls を使用した高性能リバースプロキシサーバー。

## 特徴

### コア機能
- **非同期I/O**: monoio (io_uring) による効率的なI/O処理
- **TLS**: rustls によるメモリ安全な Pure Rust TLS実装
- **kTLS**: rustls + ktls2 によるカーネルTLSオフロード対応（Linux 5.15+）
- **HTTP/2**: TLS ALPNネゴシエーションによるHTTP/2サポート（ストリーム多重化、HPACK圧縮）
- **HTTP/3**: QUIC/UDPベースのHTTP/3サポート（quiche使用、0-RTT接続確立）
- **高速アロケータ**: mimalloc による高速メモリ割り当て + Huge Pages対応
- **高速ルーティング**: Radix Tree (matchit) によるO(log n)パスマッチング

### プロキシ機能
- **コネクションプール**: バックエンド接続の再利用によるレイテンシ削減（HTTP/HTTPS両対応）
- **ロードバランシング**: 複数バックエンドへのリクエスト分散（Round Robin/Least Connections/IP Hash）
- **ヘルスチェック**: HTTP/TLSベースのアクティブヘルスチェックによる自動フェイルオーバー
- **プロキシキャッシュ**: メモリ・ディスクベースのレスポンスキャッシュ（ETag/304、stale-while-revalidate、stale-if-error）
- **バッファリング制御**: 低速クライアントによるバックエンド占有防止のためのレスポンスバッファリング（Streaming/Full/Adaptiveモード）
- **WebSocketサポート**: Upgradeヘッダー検知による双方向プロキシ（Fixed/Adaptiveポーリングモード）
- **H2C (HTTP/2 over cleartext)**: TLSなしのHTTP/2バックエンド接続（gRPC対応）
- **ヘッダー操作**: リクエスト/レスポンスヘッダーの追加・削除（X-Real-IP, HSTS等）
- **リダイレクト**: 301/302/307/308 HTTPリダイレクト（パス保持オプション付き）
- **SNI設定**: HTTPSバックエンドへのIP直打ち時にSNI名を指定可能（仮想ホスト対応）

### HTTP処理
- **Keep-Alive**: HTTP/1.1 Keep-Alive完全サポート
- **Chunked転送**: RFC 7230準拠のChunkedデコーダ（ステートマシンベース）
- **バッファプール**: メモリアロケーションの削減
- **レスポンス圧縮**: Gzip/Brotli/Zstdによる動的圧縮（Accept-Encodingネゴシエーション対応）

### パフォーマンス
- **CPUアフィニティ**: ワーカースレッドのCPUコアピン留め
- **CBPF振り分け**: SO_REUSEPORTのクライアントIPベースロードバランシング（Linux 4.6+）

### 運用機能
- **Graceful Shutdown**: SIGINT/SIGTERMによる安全な終了
- **Graceful Reload**: SIGHUPによる設定のホットリロード（ゼロダウンタイム）
- **非同期ログ**: ftlog による高性能非同期ログ
- **設定バリデーション**: 起動時の詳細な設定ファイル検証
- **Prometheusメトリクス**: メトリクスエンドポイントでリクエスト数、レイテンシ、アクティブ接続数、アップストリーム健康状態等を出力（要設定、デフォルト無効）

### セキュリティ
- **HTTP to HTTPSリダイレクト**: HTTPアクセスを自動的にHTTPSへ301リダイレクト
- **同時接続数制限**: グローバルな接続数上限設定
- **レートリミッター**: スライディングウィンドウ方式のレート制限
- **IP制限**: CIDR対応のIPアドレスフィルタリング
- **権限降格**: root起動後の非特権ユーザーへの降格
- **seccompフィルタ**: BPFベースのシステムコール制限（オプション）
- **Landlockサンドボックス**: ファイルシステムアクセス制限（Linux 5.13+）
- **systemdサンドボックス**: 名前空間隔離・システムコール制限対応

## ビルド

```bash
# 通常ビルド（rustls使用、HTTP/1.1のみ）
cargo build --release

# kTLSサポート付きビルド（rustls + ktls2）
cargo build --release --features ktls

# HTTP/2サポート付きビルド
cargo build --release --features http2

# HTTP/3サポート付きビルド（quiche使用）
cargo build --release --features http3

# 全プロトコルサポート（HTTP/2 + HTTP/3）
cargo build --release --features all-protocols

# kTLS + HTTP/2（推奨構成）
cargo build --release --features "ktls,http2"

# フルビルド（kTLS + 全プロトコル）
cargo build --release --features "ktls,all-protocols"
```

ビルド後のバイナリは `target/release/veil` に生成されます。

### フィーチャーフラグ一覧

| フィーチャー | 説明 | 備考 |
|-------------|------|------|
| `ktls` | kTLSカーネルオフロード | Linux 5.15+、要`modprobe tls` |
| `http2` | HTTP/2 (ALPN h2) | TLS接続でのHTTP/2サポート |
| `http3` | HTTP/3 (QUIC) | UDP/QUICベース、quiche使用 |
| `all-protocols` | http2 + http3 | 全プロトコル有効化 |

> **Note**: HTTP/3はUDPベースのため、kTLSとの併用はできません（HTTP/3はTCP/TLSを使用しないため）。

## 起動

```bash
# デフォルト設定ファイル（/etc/veil/config.toml）で起動
./veil

# 設定ファイルを指定して起動
./veil -c /path/to/config.toml
./veil --config /path/to/config.toml

# ヘルプを表示
./veil --help

# バージョンを表示
./veil --version
```

### コマンドラインオプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `-c, --config <PATH>` | 設定ファイルのパス | `/etc/veil/config.toml` |
| `-h, --help` | ヘルプメッセージを表示 | - |
| `-V, --version` | バージョン情報を表示 | - |

## HTTP to HTTPS リダイレクト

HTTPアクセスを自動的にHTTPSにリダイレクトする機能です。

### 設定

```toml
[server]
listen = "0.0.0.0:443"
http = "0.0.0.0:80"  # HTTPリダイレクトを有効化
```

### 動作

- `http://example.com/path` へのアクセスは `https://example.com/path` に301リダイレクト
- Hostヘッダーからドメイン名を取得し、リダイレクト先URLを構築
- ポート番号が含まれている場合は自動的に除去（HTTPSのデフォルトポート443を使用）

### セキュリティ考慮事項

- **リダイレクト専用**: HTTPではリダイレクトのみを行い、コンテンツは一切配信しません
- **301 Moved Permanently**: ブラウザがリダイレクト先をキャッシュするため、2回目以降は直接HTTPSにアクセスします
- **初回アクセス**: 初回HTTPアクセス時のみ平文通信が発生しますが、コンテンツは含まれません

### 注意事項

- 特権ポート（80番）を使用するため、以下のいずれかが必要です：
  1. rootで起動（権限降格機能と併用を推奨）
  2. `CAP_NET_BIND_SERVICE`ケイパビリティを付与

```bash
# ケイパビリティを付与する場合
sudo setcap 'cap_net_bind_service=+ep' ./target/release/veil
```

## TLS証明書の生成

開発・テスト用の自己署名証明書を生成するには、以下のコマンドを実行します：

```bash
# ECDSA秘密鍵の生成（secp384r1）
openssl genpkey -algorithm EC -out server.key -pkeyopt ec_paramgen_curve:secp384r1 -pkeyopt ec_param_enc:named_curve

# 自己署名証明書の生成（有効期限365日）
openssl req -new -x509 -key server.key -out server.crt -days 365 -subj "/CN=localhost/O=Development/C=JP"
```

生成されたファイルを `config.toml` で指定してください：

```toml
[tls]
cert_path = "./server.crt"
key_path = "./server.key"
```

> **注意**: 本番環境では、Let's Encryptなどの認証局から発行された証明書を使用してください。

## TLSライブラリ

### rustls（デフォルト）

- メモリ安全な純Rust実装
- 追加の依存関係なし
- kTLSを使用しない場合のデフォルト

### rustls + ktls2（`--features ktls`）

- rustls でTLSハンドシェイクを実行
- ハンドシェイク完了後、ktls2 経由でkTLSへオフロード
- 追加の外部依存関係なし（純Rust実装）

```bash
# ビルド
cargo build --release --features ktls
```

## 設定

デフォルトでは `/etc/veil/config.toml` を読み込みます。
`-c` または `--config` オプションで別のパスを指定できます。

設定ファイル例（`config.toml`）:

```toml
[server]
listen = "0.0.0.0:443"
# HTTP to HTTPSリダイレクト（オプション）
# HTTPアクセスを自動的にHTTPSにリダイレクト（301 Moved Permanently）
http = "0.0.0.0:80"
# ワーカースレッド数（オプション）
# 未指定または0の場合はCPUコア数と同じスレッド数を使用
threads = 4
# HTTP/2を有効化（--features http2 でビルド時のみ）
http2_enabled = true
# HTTP/3を有効化（--features http3 でビルド時のみ）
http3_enabled = true

[logging]
# ログレベル: "trace", "debug", "info", "warn", "error", "off"
level = "info"
# ログ出力形式: "text", "json"
# format = "text"
# ログチャネルサイズ（高負荷時のログドロップ防止）
channel_size = 100000
# フラッシュ間隔（ミリ秒）
flush_interval_ms = 1000
# 最大ログファイルサイズ（バイト、0=ローテーションなし）
max_log_size = 104857600
# ログファイルパス（オプション、未指定で標準エラー出力）
# file_path = "/var/log/veil.log"

[security]
# 権限降格設定（Linux専用）
drop_privileges_user = "nobody"
drop_privileges_group = "nogroup"
# グローバル同時接続上限（0 = 無制限）
max_concurrent_connections = 10000

# seccomp システムコール制限（Linux専用）
# まずログモードで動作確認後、filterモードに変更推奨
enable_seccomp = true
seccomp_mode = "filter"  # "disabled" / "log" / "filter" / "strict"

# Landlock ファイルシステム制限（Linux 5.13+）
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]

[performance]
# SO_REUSEPORT の振り分け方式
# "kernel" = カーネルデフォルト（3元タプルハッシュ）
# "cbpf"   = クライアントIPベースのCBPF（キャッシュ効率向上、Linux 4.6+必須）
reuseport_balancing = "cbpf"

# Huge Pages (Large OS Pages) の使用
# TLBミス削減により5-10%のパフォーマンス向上
huge_pages_enabled = true

[tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
ktls_enabled = true         # kTLS有効化（Linux 5.15+、feature flag必須）
ktls_fallback_enabled = true # kTLS失敗時のrustlsフォールバック（デフォルト: true）

# ホストベースルーティング
[host_routes."example.com"]
type = "File"
path = "/var/www/example"
mode = "sendfile"

[host_routes."api.example.com"]
type = "Proxy"
url = "http://localhost:8080"

# パスベースルーティング

# 静的ファイル（完全一致）
[path_routes."example.com"."/robots.txt"]
type = "File"
path = "/var/www/robots.txt"

# ディレクトリ配信（末尾スラッシュあり）
[path_routes."example.com"."/static/"]
type = "File"
path = "/var/www/assets/"
mode = "sendfile"

# ディレクトリ配信（末尾スラッシュなし - 同じ動作、リダイレクトなし）
[path_routes."example.com"."/docs"]
type = "File"
path = "/var/www/docs/"

# カスタムインデックスファイル
[path_routes."example.com"."/user/"]
type = "File"
path = "/var/www/user/"
index = "profile.html"

# プロキシ（末尾スラッシュあり）
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080/app/"

# プロキシ（末尾スラッシュなし - 同じ動作）
[path_routes."example.com"."/backend"]
type = "Proxy"
url = "http://localhost:3000"

# ルート
[path_routes."example.com"."/"]
type = "File"
path = "/var/www/index.html"
```

## ルーティング

### ルーティングの優先順位

1. **ホストベースルーティング** (`[host_routes]`): Hostヘッダーで完全一致
2. **パスベースルーティング** (`[path_routes."hostname"]`): パスの最長一致（Radix Tree）

### バックエンドタイプ

| タイプ | 説明 | 設定例 |
|--------|------|--------|
| `Proxy` | HTTPリバースプロキシ（単一） | `{ type = "Proxy", url = "http://localhost:8080" }` |
| `Proxy` | HTTPリバースプロキシ（LB） | `{ type = "Proxy", upstream = "backend-pool" }` |
| `Proxy` | HTTPSプロキシ（SNI指定） | `{ type = "Proxy", url = "https://192.168.1.100", sni_name = "api.example.com" }` |
| `File` | 静的ファイル配信 | `{ type = "File", path = "/var/www", mode = "sendfile" }` |
| `Redirect` | HTTPリダイレクト | `{ type = "Redirect", redirect_url = "https://new.example.com", redirect_status = 301 }` |

> **Note**: `Proxy` タイプは `url`（単一バックエンド）または `upstream`（ロードバランシング）のいずれかを指定します。WebSocketは両方で自動サポートされます。HTTPSバックエンドへのIP直打ち時は `sni_name` でSNI名を指定可能です。

### ルーティングの挙動（Nginx風）

#### 1. 静的ファイル（完全一致）

設定の `path` がファイルの場合、リクエストパスが完全一致した場合のみファイルを返します。

```toml
# /robots.txt → /var/www/robots.txt を返す
# /robots.txt/extra → 404 Not Found（ファイルの下は掘れない）
[path_routes."example.com"."/robots.txt"]
type = "File"
path = "/var/www/robots.txt"
```

#### 2. ディレクトリ配信（Alias動作）

設定の `path` がディレクトリの場合、プレフィックスを除去した残りのパスをディレクトリに結合します。
**末尾スラッシュの有無は問いません**（どちらでも同じ動作）。

```toml
# 末尾スラッシュあり（従来の書き方）
[path_routes."example.com"."/static/"]
type = "File"
path = "/var/www/assets/"

# 末尾スラッシュなし（同じ動作、301リダイレクトなし）
[path_routes."example.com"."/docs"]
type = "File"
path = "/var/www/docs/"
```

| リクエスト | 設定 | 解決パス |
|-----------|------|---------|
| `/static/css/style.css` | `"/static/"` | `/var/www/assets/css/style.css` |
| `/static/` | `"/static/"` | `/var/www/assets/index.html` |
| `/docs` | `"/docs"` | `/var/www/docs/index.html` ※直接返す |
| `/docs/` | `"/docs"` | `/var/www/docs/index.html` |
| `/docs/guide/intro.html` | `"/docs"` | `/var/www/docs/guide/intro.html` |

#### 3. インデックスファイルの指定

`index` オプションでディレクトリアクセス時に返すファイルを指定できます。
未指定の場合はデフォルトで `index.html` を使用します。

```toml
# /user/ → /var/www/user/profile.html を返す
[path_routes."example.com"."/user/"]
type = "File"
path = "/var/www/user/"
index = "profile.html"

# /app/ → /var/www/app/dashboard.html を返す
[path_routes."example.com"."/app/"]
type = "File"
path = "/var/www/app/"
index = "dashboard.html"
```

#### 4. プロキシ（Proxy Pass動作）

プレフィックスを除去した残りのパスをバックエンドURLに結合します。
**末尾スラッシュの有無は問いません**。

```toml
# 末尾スラッシュあり
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080/app/"

# 末尾スラッシュなし（同じ動作）
[path_routes."example.com"."/backend"]
type = "Proxy"
url = "http://localhost:3000"
```

| リクエスト | 設定 | 転送先 |
|-----------|------|--------|
| `/api/v1/users` | `"/api/"` → `url = ".../app/"` | `http://localhost:8080/app/v1/users` |
| `/backend` | `"/backend"` → `url = ".../"` | `http://localhost:3000/` |
| `/backend/users` | `"/backend"` | `http://localhost:3000/users` |

### ファイル配信モード

| モード | 説明 | 用途 |
|--------|------|------|
| `sendfile` | sendfileシステムコールでゼロコピー送信 | 大きなファイル、動画、画像 |
| `memory` | ファイルをメモリに読み込んで配信 | 小さなファイル、favicon.ico等 |

```toml
# ディレクトリ配信（sendfileモード）
[path_routes."example.com"."/static/"]
type = "File"
path = "/var/www/static"
mode = "sendfile"

# 単一ファイル配信（memoryモード）
[path_routes."example.com"."/favicon.ico"]
type = "File"
path = "/var/www/favicon.ico"
mode = "memory"

# typeとmodeを省略した場合のデフォルト（type = "File", mode = "sendfile"）
[path_routes."example.com"."/"]
path = "/var/www/html"
```

### プロキシ設定

HTTPおよびHTTPSバックエンドへのプロキシに対応：

```toml
# HTTPバックエンド
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080"

# HTTPSバックエンド（TLSクライアント接続）
[path_routes."example.com"."/secure/"]
type = "Proxy"
url = "https://backend.example.com"
```

### H2C (HTTP/2 over cleartext) プロキシ

バックエンドがH2C（TLSなしのHTTP/2）をサポートしている場合、`use_h2c = true` を指定することでHTTP/2で通信できます。

```toml
# gRPCバックエンドへのH2C接続
[path_routes."example.com"."/grpc/"]
type = "Proxy"
url = "http://localhost:50051"
use_h2c = true
```

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `use_h2c` | H2C (HTTP/2 without TLS) を使用 | false |

**H2Cの用途：**
- gRPCバックエンドへの接続（内部ネットワーク）
- HTTP/2の多重化とヘッダー圧縮をバックエンド通信でも活用
- Prior Knowledgeモードを使用（Upgrade経由ではない）

> **Note**: H2CはHTTPSバックエンド（TLS接続）では使用できません。内部ネットワークでのgRPC通信など、TLSが不要な環境でのみ使用してください。

#### SNI (Server Name Indication) 設定

HTTPSバックエンドへの接続時、バックエンドがIPアドレス指定の場合でもSNIにドメイン名を指定できます。
これにより、仮想ホスト構成のサーバーでも正しい証明書を取得できます。

```toml
# IPアドレス指定 + SNI名指定
[path_routes."example.com"."/internal-api/"]
type = "Proxy"
url = "https://192.168.1.100:443"
sni_name = "api.internal.example.com"
```

| 設定 | 説明 | デフォルト |
|------|------|-----------|
| `sni_name` | TLS接続時のSNI名（省略時はURLのホスト名を使用） | URLのホスト名 |

> **Note**: `sni_name` を指定した場合、TLS証明書の検証もその名前で行われます。バックエンドサーバーの証明書は指定したドメイン名（またはワイルドカード）を含む必要があります。

### ロードバランシング設定

複数バックエンドへのリクエスト分散：

```toml
# Upstreamグループの定義
[upstreams."api-pool"]
algorithm = "round_robin"  # または "least_conn", "ip_hash"
servers = [
  "http://api1:8080",
  "http://api2:8080",
  "http://api3:8080"
]

  # ヘルスチェック（オプション）
  [upstreams."api-pool".health_check]
  interval_secs = 10
  path = "/health"
  timeout_secs = 5
  healthy_statuses = [200]
  unhealthy_threshold = 3
  healthy_threshold = 2

# Upstreamを参照するルート
[path_routes."example.com"."/api/"]
type = "Proxy"
upstream = "api-pool"
```

#### UpstreamでのSNI設定

Upstreamのサーバーエントリは文字列形式と構造体形式の両方をサポートします。
構造体形式を使用すると、IPアドレス指定時にSNI名を指定できます。

```toml
# HTTPSバックエンドプール（SNI名指定付き）
[upstreams."https-pool"]
algorithm = "least_conn"
servers = [
  # 構造体形式: IPアドレス + SNI名
  { url = "https://192.168.1.100:443", sni_name = "api.example.com" },
  { url = "https://192.168.1.101:443", sni_name = "api.example.com" },
  # 文字列形式: ドメイン名指定（SNI名は自動的にURLのホスト名）
  "https://api.example.com:443"
]

# ルートでUpstreamを参照
[path_routes."example.com"."/api/"]
type = "Proxy"
upstream = "https-pool"
```

> **Note**: 文字列形式と構造体形式は同一配列内で混在可能です。従来の文字列形式は後方互換性のためそのまま動作します。

### WebSocket設定

WebSocketは通常のProxyで自動サポートされます。双方向転送時のポーリング動作を設定でカスタマイズ可能です。

#### 基本設定

```toml
# WebSocketアプリケーション
[path_routes."example.com"."/ws/"]
type = "Proxy"
url = "http://localhost:3000"

# ロードバランシング付きWebSocket
[path_routes."example.com"."/ws-lb/"]
type = "Proxy"
upstream = "websocket-pool"
```

#### ポーリングモード設定

WebSocket双方向転送時のポーリング動作を制御します。

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `websocket_poll_mode` | ポーリングモード（`"fixed"` / `"adaptive"`） | `"adaptive"` |
| `websocket_poll_timeout_ms` | 初期タイムアウト（ミリ秒） | 1 |
| `websocket_poll_max_timeout_ms` | 最大タイムアウト（ミリ秒）※adaptiveのみ | 100 |
| `websocket_poll_backoff_multiplier` | バックオフ倍率 ※adaptiveのみ | 2.0 |

#### ポーリングモードの選択

| モード | 動作 | 用途 |
|--------|------|------|
| `fixed` | 常に固定タイムアウトを使用 | リアルタイムゲームなど低レイテンシ最優先 |
| `adaptive` | アクティブ時は短く、アイドル時は長くなる | チャット、監視ダッシュボードなどバランス重視 |

**Adaptive モードの動作:**

```
データ転送あり → タイムアウトをリセット（初期値に戻す）
タイムアウト発生 → タイムアウト × 倍率（最大値まで延長）

例: 初期値=1ms, 最大=100ms, 倍率=2.0 の場合
1ms → 2ms → 4ms → 8ms → 16ms → 32ms → 64ms → 100ms（最大値で停止）
↓ データが来たら
1ms（リセット）
```

#### WebSocket設定例

```toml
# リアルタイムゲーム（低レイテンシ最優先）
[path_routes."game.example.com"."/ws/"]
type = "Proxy"
url = "http://localhost:3000"

  [path_routes."game.example.com"."/ws/".security]
  websocket_poll_mode = "fixed"
  websocket_poll_timeout_ms = 1

# チャットアプリ（バランス重視）
[path_routes."chat.example.com"."/ws/"]
type = "Proxy"
url = "http://localhost:3001"

  [path_routes."chat.example.com"."/ws/".security]
  websocket_poll_mode = "adaptive"
  websocket_poll_timeout_ms = 1
  websocket_poll_max_timeout_ms = 50
  websocket_poll_backoff_multiplier = 2.0

# 監視ダッシュボード（CPU効率優先）
[path_routes."monitor.example.com"."/ws/"]
type = "Proxy"
url = "http://localhost:3002"

  [path_routes."monitor.example.com"."/ws/".security]
  websocket_poll_mode = "adaptive"
  websocket_poll_timeout_ms = 10
  websocket_poll_max_timeout_ms = 200
  websocket_poll_backoff_multiplier = 1.5
```

### グローバルセキュリティ設定

`[security]` セクションでサーバー全体のセキュリティ設定を行います。

```toml
[security]
# 権限降格設定（Linux専用、root起動時のみ有効）
drop_privileges_user = "veil"
drop_privileges_group = "veil"

# グローバル同時接続上限（0 = 無制限）
max_concurrent_connections = 10000

# seccomp システムコール制限
enable_seccomp = true
seccomp_mode = "filter"

# Landlock ファイルシステム制限（Linux 5.13+）
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]
```

#### 権限・接続制限

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `drop_privileges_user` | 起動後に降格するユーザー名 | なし |
| `drop_privileges_group` | 起動後に降格するグループ名 | なし |
| `max_concurrent_connections` | 同時接続数の上限 | 0（無制限） |

#### seccomp 設定

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enable_seccomp` | seccompフィルタを有効化 | false |
| `seccomp_mode` | seccompモード | "disabled" |

| seccompモード | 説明 |
|--------------|------|
| `disabled` | 無効 |
| `log` | 違反をログに記録（ブロックしない、導入時推奨） |
| `filter` | 違反をEPERMで拒否（**本番推奨**） |
| `strict` | 違反したプロセスをSIGKILL（最も厳格） |

#### Landlock 設定 (Linux 5.13+)

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enable_landlock` | Landlockを有効化 | false |
| `landlock_read_paths` | 読み取り専用パス | `["/etc", "/usr", "/lib", "/lib64"]` |
| `landlock_write_paths` | 読み書き可能パス | `["/var/log", "/tmp"]` |

**対応ABIバージョン:**

| ABI | カーネル | 追加機能 |
|-----|---------|---------|
| v1 | 5.13+ | 基本的なファイルシステムアクセス制御 |
| v2 | 5.19+ | ファイル参照権限 (REFER) |
| v3 | 6.2+ | TRUNCATE権限 |
| v4 | 6.7+ | ネットワーク制限（FSは変更なし） |
| v5+ | 6.10+ | IOCTL_DEV権限 |

#### サンドボックス設定（bubblewrap相当）

Linuxのnamespace分離、bind mounts、capabilities制限を適用することで、bubblewrapと同等のセキュリティ分離を実現します。

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enable_sandbox` | サンドボックスを有効化 | false |
| `sandbox_unshare_mount` | Mount namespace分離 | true |
| `sandbox_unshare_uts` | UTS namespace分離（ホスト名隔離） | true |
| `sandbox_unshare_ipc` | IPC namespace分離 | true |
| `sandbox_unshare_pid` | PID namespace分離 | false |
| `sandbox_unshare_user` | User namespace分離 | false |
| `sandbox_unshare_net` | Network namespace分離（**警告: 通信不可**） | false |
| `sandbox_keep_capabilities` | 保持するケイパビリティ | [] |
| `sandbox_ro_bind_mounts` | 読み取り専用バインドマウント（source:dest形式） | 標準パス |
| `sandbox_rw_bind_mounts` | 読み書きバインドマウント | [] |
| `sandbox_tmpfs_mounts` | tmpfsマウント先 | ["/tmp"] |
| `sandbox_mount_proc` | /procをマウント | true |
| `sandbox_mount_dev` | /devを作成 | true |
| `sandbox_hostname` | サンドボックス内のホスト名 | "veil-sandbox" |
| `sandbox_no_new_privs` | PR_SET_NO_NEW_PRIVSを設定 | true |

```toml
[security]
enable_sandbox = true
sandbox_unshare_mount = true
sandbox_unshare_uts = true
sandbox_unshare_ipc = true
sandbox_keep_capabilities = ["CAP_NET_BIND_SERVICE"]
sandbox_ro_bind_mounts = ["/usr:/usr", "/lib:/lib", "/lib64:/lib64"]
sandbox_tmpfs_mounts = ["/tmp"]
```

> **注意**: `sandbox_unshare_net = true` にするとネットワーク通信ができなくなります。リバースプロキシでは通常 `false` のままにしてください。

> **注意**: 特権ポート（1024未満）を使用する場合は、`CAP_NET_BIND_SERVICE` ケイパビリティを付与するか、非特権ポートを使用してください。
>
> ```bash
> sudo setcap 'cap_net_bind_service=+ep' ./target/release/veil
> ```

### ルートごとのセキュリティ設定

各ルートに `security` サブセクションを追加することで、細かいセキュリティ設定が可能です。

#### 設定オプション一覧

| カテゴリ | オプション | 説明 | デフォルト |
|----------|-----------|------|-----------|
| サイズ制限 | `max_request_body_size` | リクエストボディ最大サイズ（バイト） | 10MB |
| | `max_chunked_body_size` | Chunked転送時の累積最大サイズ | 10MB |
| | `max_request_header_size` | リクエストヘッダー最大サイズ | 8KB |
| タイムアウト | `client_header_timeout_secs` | クライアントヘッダー受信タイムアウト | 30秒 |
| | `client_body_timeout_secs` | クライアントボディ受信タイムアウト | 30秒 |
| | `backend_connect_timeout_secs` | バックエンド接続タイムアウト | 10秒 |
| アクセス制御 | `allowed_methods` | 許可するHTTPメソッド（配列） | すべて許可 |
| | `rate_limit_requests_per_min` | 分間リクエスト数上限 | 0（無制限） |
| | `allowed_ips` | 許可するIP/CIDR（配列） | すべて許可 |
| | `denied_ips` | 拒否するIP/CIDR（配列、優先） | なし |
| コネクションプール | `max_idle_connections_per_host` | ホストごとの最大アイドル接続数 | 8 |
| | `idle_connection_timeout_secs` | アイドル接続の維持時間 | 30秒 |
| ヘッダー操作 | `add_request_headers` | バックエンドに転送前に追加するヘッダー | なし |
| | `remove_request_headers` | バックエンドに転送前に削除するヘッダー | なし |
| | `add_response_headers` | クライアントに返送前に追加するヘッダー | なし |
| | `remove_response_headers` | クライアントに返送前に削除するヘッダー | なし |
| WebSocket | `websocket_poll_mode` | ポーリングモード（`"fixed"` / `"adaptive"`） | `"adaptive"` |
| | `websocket_poll_timeout_ms` | 初期タイムアウト（ミリ秒） | 1 |
| | `websocket_poll_max_timeout_ms` | 最大タイムアウト（ミリ秒）※adaptiveのみ | 100 |
| | `websocket_poll_backoff_multiplier` | バックオフ倍率 ※adaptiveのみ | 2.0 |

#### セキュリティ設定例

```toml
# API用セキュリティ設定
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080/app/"

  [path_routes."example.com"."/api/".security]
  allowed_methods = ["GET", "POST", "PUT"]
  max_request_body_size = 5_242_880  # 5MB
  backend_connect_timeout_secs = 5
  rate_limit_requests_per_min = 60

# IP制限付き管理API
[path_routes."example.com"."/admin/"]
type = "Proxy"
url = "http://localhost:9000/"

  [path_routes."example.com"."/admin/".security]
  allowed_ips = [
    "192.168.0.0/16",
    "10.0.0.0/8",
    "127.0.0.1"
  ]
  denied_ips = ["192.168.1.100"]
  allowed_methods = ["GET", "POST"]
```

#### IP制限の評価順序

IP制限は **deny → allow** の順で評価されます（denyが優先）。

1. `denied_ips` にマッチ → 拒否（403 Forbidden）
2. `allowed_ips` が空 → 許可
3. `allowed_ips` にマッチ → 許可
4. それ以外 → 拒否（403 Forbidden）

| 形式 | 例 |
|------|-----|
| 単一IPv4 | `192.168.1.1` |
| IPv4 CIDR | `192.168.0.0/24` |
| 単一IPv6 | `::1` |
| IPv6 CIDR | `2001:db8::/32` |

## ヘッダー操作

リクエスト/レスポンスヘッダーの追加・削除が可能です。X-Real-IP、X-Forwarded-Proto、HSTSなどのセキュリティヘッダーを設定できます。

### リクエストヘッダー操作

バックエンドへ転送する前にヘッダーを追加・削除します。

| オプション | 説明 | 例 |
|-----------|------|-----|
| `add_request_headers` | 追加するヘッダー（テーブル形式） | `{ "X-Real-IP" = "$client_ip" }` |
| `remove_request_headers` | 削除するヘッダー（配列） | `["X-Debug-Token"]` |

#### 特殊変数

`add_request_headers` の値では以下の変数を使用できます：

| 変数 | 説明 |
|------|------|
| `$client_ip` | クライアントのIPアドレス |
| `$host` | リクエストのHostヘッダー |
| `$request_uri` | リクエストURI（パス + クエリ文字列） |

### レスポンスヘッダー操作

クライアントへ返送する前にヘッダーを追加・削除します。静的ファイル配信時にも適用されます。

| オプション | 説明 | 例 |
|-----------|------|-----|
| `add_response_headers` | 追加するヘッダー | `{ "Strict-Transport-Security" = "max-age=31536000" }` |
| `remove_response_headers` | 削除するヘッダー | `["Server", "X-Powered-By"]` |

### 設定例

```toml
# セキュリティヘッダー付きプロキシ
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080"

  [path_routes."example.com"."/api/".security]
  # バックエンドに転送前に追加
  add_request_headers = { "X-Real-IP" = "$client_ip", "X-Forwarded-Proto" = "https" }
  # バックエンドに転送前に削除
  remove_request_headers = ["X-Debug-Token", "X-Internal-Auth"]
  # クライアントに返送前に追加（セキュリティヘッダー）
  add_response_headers = { "Strict-Transport-Security" = "max-age=31536000; includeSubDomains", "X-Frame-Options" = "DENY", "X-Content-Type-Options" = "nosniff" }
  # クライアントに返送前に削除
  remove_response_headers = ["X-Powered-By"]
```

## リダイレクト

HTTPリダイレクト（301/302/303/307/308）を設定できます。WWW非対応、HTTPS強制、旧URL移行などに使用します。

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `redirect_url` | リダイレクト先URL（必須） | - |
| `redirect_status` | ステータスコード（301, 302, 303, 307, 308） | 301 |
| `preserve_path` | 元のパスをリダイレクト先に追加するか | false |

### ステータスコードの使い分け

| コード | 説明 | 用途 |
|--------|------|------|
| 301 | Moved Permanently | 永続的な移転（SEO引き継ぎ） |
| 302 | Found | 一時的なリダイレクト |
| 303 | See Other | POSTからGETへのリダイレクト |
| 307 | Temporary Redirect | 一時的（メソッド維持） |
| 308 | Permanent Redirect | 永続的（メソッド維持） |

### 設定例

```toml
# WWWへのリダイレクト
[path_routes."example.com"."/"]
type = "Redirect"
redirect_url = "https://www.example.com/"
redirect_status = 301

# 旧URLから新URLへの移行（パス保持）
[path_routes."example.com"."/legacy/"]
type = "Redirect"
redirect_url = "https://example.com/v2"
redirect_status = 301
preserve_path = true
# /legacy/users → https://example.com/v2/users
# /legacy/api/data → https://example.com/v2/api/data

# HTTPからHTTPSへの強制リダイレクト（別のhostで設定）
[path_routes."http.example.com"."/"]
type = "Redirect"
redirect_url = "https://example.com$request_uri"
redirect_status = 301
```

### 特殊変数

`redirect_url` では以下の変数を使用できます：

| 変数 | 説明 |
|------|------|
| `$request_uri` | 元のリクエストURI |
| `$path` | prefix除去後のパス部分 |

## レスポンス圧縮

動的レスポンス圧縮（Gzip、Brotli、Zstd）をサポートします。Accept-Encodingヘッダーに基づいて、クライアントに送信する前にレスポンスを圧縮します。

### 特徴

| 項目 | 説明 |
|------|------|
| **複数アルゴリズム対応** | Gzip、Brotli、Zstd、Deflateをサポート |
| **Content-Typeフィルタリング** | text/HTML/JSON等のみ圧縮 |
| **最小サイズ閾値** | 小さなレスポンスは圧縮スキップ |
| **Accept-Encodingネゴシエーション** | 最適なエンコーディングを自動選択 |

### 有効化

圧縮はデフォルトで**無効**です（kTLS最適化のゼロコピーsendfileを維持）。
ルートごとに `compression` セクションで有効化します：

```toml
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080"

  [path_routes."example.com"."/api/".compression]
  enabled = true
```

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | 圧縮を有効化 | false |
| `preferred_encodings` | エンコーディング優先順位（配列） | ["zstd", "br", "gzip"] |
| `gzip_level` | Gzip圧縮レベル（1-9） | 4 |
| `brotli_level` | Brotli圧縮レベル（0-11） | 4 |
| `zstd_level` | Zstd圧縮レベル（1-22） | 3 |
| `min_size` | 圧縮する最小サイズ（バイト） | 1024 |
| `compressible_types` | 圧縮対象のMIMEタイプ（プレフィックスマッチ） | text/*, application/json等 |
| `skip_types` | スキップするMIMEタイプ（プレフィックスマッチ） | image/*, video/*, audio/*等 |

### 圧縮レベルガイドライン

| アルゴリズム | レベル | 速度 | 圧縮率 | 用途 |
|-------------|--------|------|--------|------|
| Gzip | 1-3 | 高速 | 低 | リアルタイム、高スループット |
| Gzip | 4-6 | バランス | 中 | 汎用 |
| Gzip | 7-9 | 低速 | 高 | 静的アセット、帯域優先 |
| Brotli | 0-4 | 高速 | 中 | 動的コンテンツ |
| Brotli | 5-9 | バランス | 高 | 汎用 |
| Brotli | 10-11 | 低速 | 最高 | 静的アセット |
| Zstd | 1-3 | 高速 | 中 | リアルタイムAPI |
| Zstd | 4-9 | バランス | 高 | 汎用 |
| Zstd | 10-22 | 低速 | 最高 | アーカイブ |

### 設定例

```toml
# API圧縮（高速、バランス重視）
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080"

  [path_routes."example.com"."/api/".compression]
  enabled = true
  preferred_encodings = ["zstd", "br", "gzip"]
  zstd_level = 3
  brotli_level = 4
  gzip_level = 4
  min_size = 1024

# 静的アセット（高圧縮率）
[path_routes."example.com"."/static/"]
type = "File"
path = "/var/www/static"

  [path_routes."example.com"."/static/".compression]
  enabled = true
  preferred_encodings = ["br", "gzip"]
  brotli_level = 6
  gzip_level = 6
  min_size = 256
```

### デフォルト圧縮対象タイプ

以下のMIMEタイプはデフォルトで圧縮されます：

- `text/*`（HTML、CSS、プレーンテキスト等）
- `application/json`
- `application/javascript`
- `application/xml`
- `application/xhtml+xml`
- `application/rss+xml`
- `application/atom+xml`
- `image/svg+xml`
- `application/wasm`

### デフォルトスキップタイプ

以下のMIMEタイプは圧縮**されません**（既に圧縮済み、またはバイナリ）：

- `image/*`
- `video/*`
- `audio/*`
- `application/octet-stream`
- `application/zip`
- `application/gzip`
- `application/x-gzip`
- `application/x-brotli`

### HTTP/3圧縮設定

HTTP/3では `[http3]` セクションで別途圧縮設定が可能です：

```toml
[http3]
compression_enabled = true

  [http3.compression]
  preferred_encodings = ["br", "gzip"]
  brotli_level = 5
  gzip_level = 5
```

> **Note**: 圧縮を有効にすると、圧縮されたレスポンスに対してはkTLSのゼロコピーsendfile最適化は使用されません。大きなファイルの最大スループットを得るには、静的ファイルルートでは圧縮を無効にすることを検討してください。

## プロキシキャッシュ

バックエンドレスポンスをキャッシュし、バックエンド負荷の軽減とレスポンス時間の改善を実現します。

### 特徴

| 機能 | 説明 |
|------|------|
| **メモリキャッシュ** | サイズ制限付きの高速インメモリLRUキャッシュ |
| **ディスクキャッシュ** | monoio非同期I/Oを使用した大容量レスポンス保存 |
| **ETag/If-None-Match** | 条件付きリクエストに対する304 Not Modifiedレスポンス |
| **If-Modified-Since** | 日付ベースの条件付きリクエスト検証 |
| **stale-while-revalidate** | バックグラウンドで更新しながらstaleコンテンツを提供 |
| **stale-if-error** | バックエンドエラー時にstaleコンテンツを提供 |
| **Varyヘッダーサポート** | リクエストヘッダーに基づくキャッシュ分離 |
| **パターンベース無効化** | globパターンによるキャッシュ無効化 |

### 有効化

キャッシュはデフォルトで**無効**です。ルートごとに `cache` セクションで有効化します：

```toml
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080"

  [path_routes."example.com"."/api/".cache]
  enabled = true
```

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | キャッシュを有効化 | false |
| `max_memory_size` | メモリキャッシュ最大サイズ（バイト） | 100MB |
| `disk_path` | ディスクキャッシュディレクトリ（オプション） | なし |
| `max_disk_size` | ディスクキャッシュ最大サイズ（バイト） | 1GB |
| `memory_threshold` | これより大きいレスポンスはディスクへ（バイト） | 64KB |
| `default_ttl_secs` | Cache-Controlがない場合のデフォルトTTL | 300 |
| `methods` | キャッシュ対象HTTPメソッド | ["GET", "HEAD"] |
| `cacheable_statuses` | キャッシュ対象ステータスコード | [200, 301, 302, 304] |
| `bypass_patterns` | キャッシュスキップ用globパターン | [] |
| `respect_vary` | Varyヘッダーによるキャッシュ分離を尊重 | true |
| `enable_etag` | ETag/If-None-Match検証を有効化 | true |
| `stale_while_revalidate` | バックグラウンド更新中にstaleを提供 | false |
| `stale_if_error` | バックエンドエラー時にstaleを提供 | false |
| `include_query` | クエリパラメータをキャッシュキーに含める | true |
| `key_headers` | キャッシュキーに含めるリクエストヘッダー | [] |

### 設定例

```toml
[path_routes."example.com"."/cached-api/"]
type = "Proxy"
url = "http://localhost:8080"

  [path_routes."example.com"."/cached-api/".cache]
  enabled = true
  max_memory_size = 104857600  # 100MB
  disk_path = "/var/cache/veil/api"
  max_disk_size = 1073741824   # 1GB
  memory_threshold = 65536     # 64KB
  default_ttl_secs = 300
  methods = ["GET", "HEAD"]
  cacheable_statuses = [200, 301, 302, 304]
  bypass_patterns = ["/cached-api/user/*", "/cached-api/session"]
  respect_vary = true
  enable_etag = true
  stale_while_revalidate = true
  stale_if_error = true
  include_query = true
  key_headers = ["Authorization"]  # ユーザーごとのキャッシュ
```

### キャッシュキー生成

キャッシュキーは以下から生成されます：
1. ホスト名
2. リクエストパス
3. クエリパラメータ（`include_query = true` の場合）
4. 指定された `key_headers` の値

### 注意事項

- `streaming` バッファリングモード使用時、kTLSのゼロコピー転送は維持されます
- キャッシュは `Cache-Control: no-cache`、`no-store`、`private` ヘッダーを尊重します
- `respect_vary = true` の場合、`Vary: *` レスポンスはキャッシュされません

## バッファリング制御

低速クライアントによるバックエンド接続の占有を防止するためのレスポンスバッファリングを制御します。

### 特徴

| 機能 | 説明 |
|------|------|
| **Streamingモード** | パススルー転送（デフォルト、kTLS維持） |
| **Fullバッファリング** | クライアント送信前にレスポンス全体をバッファ |
| **Adaptiveモード** | レスポンスサイズに基づく自動切り替え |
| **ディスクスピルオーバー** | メモリ制限超過時にディスクへ書き込み |

### モード

| モード | 説明 | 用途 |
|--------|------|------|
| `streaming` | 直接転送（デフォルト） | 大きなファイル、リアルタイムAPI、kTLS最適化 |
| `full` | レスポンス全体をバッファ | 低速クライアント対応、小さなレスポンス |
| `adaptive` | Content-Lengthに基づく自動切り替え | 混在ワークロード |

### 有効化

バッファリングはデフォルトで **streaming（パススルー）** です。ルートごとに `buffering` セクションで設定します：

```toml
[path_routes."example.com"."/api/"]
type = "Proxy"
url = "http://localhost:8080"

  [path_routes."example.com"."/api/".buffering]
  mode = "adaptive"
```

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `mode` | バッファリングモード（`streaming`/`full`/`adaptive`） | `streaming` |
| `max_memory_buffer` | メモリバッファ最大サイズ（バイト） | 10MB |
| `adaptive_threshold` | adaptiveモードのサイズ閾値（バイト） | 1MB |
| `disk_buffer_path` | ディスクスピルオーバーディレクトリ（オプション） | なし |
| `max_disk_buffer` | ディスクバッファ最大サイズ（バイト） | 100MB |
| `client_write_timeout_secs` | クライアント書き込みタイムアウト | 60 |
| `buffer_headers` | ヘッダーもボディと一緒にバッファリング | true |

### 設定例

```toml
[path_routes."example.com"."/buffered-api/"]
type = "Proxy"
url = "http://localhost:8080"

  [path_routes."example.com"."/buffered-api/".buffering]
  mode = "adaptive"
  adaptive_threshold = 1048576   # 1MB
  max_memory_buffer = 10485760   # 10MB
  disk_buffer_path = "/var/tmp/veil/buffer"
  max_disk_buffer = 104857600    # 100MB
  client_write_timeout_secs = 60
  buffer_headers = true
```

### Adaptiveモードの動作

```
Content-Length <= adaptive_threshold → フルバッファリング
Content-Length > adaptive_threshold  → ストリーミング
Content-Length 不明（chunked）       → ストリーミング
```

### kTLS互換性

- **Streamingモード**: kTLS `splice(2)` ゼロコピー転送が完全に維持されます
- **Full/Adaptiveモード**: レスポンスはユーザースペースバッファを経由します（kTLS最適化なし）

> **Note**: kTLSで最大パフォーマンスを得るには、低レイテンシが重要なルートで `streaming` モードを使用してください。

## Prometheusメトリクス

リクエスト数、レイテンシ、ボディサイズなどのメトリクスをPrometheus形式でエクスポートします。

### 有効化

Prometheusメトリクスはデフォルトで**無効**です。`[prometheus]` セクションで明示的に有効化する必要があります。

```toml
[prometheus]
enabled = true
```

> **Note**: `[prometheus]` セクション自体が存在しない場合も、メトリクスは無効です。

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | メトリクスエンドポイントを有効化 | **false** |
| `path` | メトリクスエンドポイントのパス | `/__metrics` |
| `allowed_ips` | アクセスを許可するIP/CIDR（配列） | []（すべて許可） |
| `enable_active_connections` | アクティブ接続数メトリクスを有効化 | **true** |
| `enable_upstream_health` | アップストリーム健康状態メトリクスを有効化 | **true** |

### エンドポイント

```
GET /__metrics
```

`path` オプションでエンドポイントのパスを変更できます。

### 利用可能なメトリクス

| メトリクス | タイプ | ラベル | 説明 |
|-----------|--------|--------|------|
| `veil_proxy_http_requests_total` | Counter | method, status, host | リクエスト総数 |
| `veil_proxy_http_request_duration_seconds` | Histogram | method, host | リクエスト処理時間（秒） |
| `veil_proxy_http_request_size_bytes` | Histogram | - | リクエストボディサイズ |
| `veil_proxy_http_response_size_bytes` | Histogram | - | レスポンスボディサイズ |
| `veil_proxy_http_active_connections` | Gauge | host | アクティブな接続数 |
| `veil_proxy_http_upstream_health` | Gauge | upstream, server | アップストリーム健康状態（1=healthy, 0=unhealthy） |
| `veil_proxy_cache_hits_total` | Counter | host | キャッシュヒット総数 |
| `veil_proxy_cache_misses_total` | Counter | host | キャッシュミス総数 |
| `veil_proxy_cache_stores_total` | Counter | host, storage | キャッシュ保存操作総数 |
| `veil_proxy_cache_size_bytes` | Gauge | storage | 現在のキャッシュサイズ（バイト） |
| `veil_proxy_cache_entries` | Gauge | storage | 現在のキャッシュエントリ数 |
| `veil_proxy_buffering_used_total` | Counter | host | バッファリング使用リクエスト総数 |

### Grafanaダッシュボード例

```promql
# リクエストレート（リクエスト/秒）
rate(veil_proxy_http_requests_total[5m])

# エラー率（4xx + 5xx）
sum(rate(veil_proxy_http_requests_total{status=~"4..|5.."}[5m])) 
  / sum(rate(veil_proxy_http_requests_total[5m]))

# レイテンシP95
histogram_quantile(0.95, rate(veil_proxy_http_request_duration_seconds_bucket[5m]))

# ホスト別リクエストレート
sum by (host) (rate(veil_proxy_http_requests_total[5m]))
```

### 設定例（config.toml）

```toml
# 基本設定（全IPからアクセス可能）
[prometheus]
enabled = true
path = "/__metrics"

# セキュリティ強化版（内部ネットワークのみ許可）
[prometheus]
enabled = true
path = "/metrics"
allowed_ips = [
  "127.0.0.1",
  "::1",
  "10.0.0.0/8",
  "172.16.0.0/12",
  "192.168.0.0/16"
]
# 特定のメトリクスの有効化/無効化（デフォルト: 両方有効）
enable_active_connections = true
enable_upstream_health = true
```

### アクセス制御

`allowed_ips` を設定すると、指定したIPアドレス/CIDRからのみメトリクスエンドポイントにアクセス可能になります。
空の場合（デフォルト）は全てのIPからアクセス可能です。

| 形式 | 例 |
|------|-----|
| 単一IPv4 | `127.0.0.1` |
| IPv4 CIDR | `10.0.0.0/8` |
| 単一IPv6 | `::1` |
| IPv6 CIDR | `2001:db8::/32` |

### Prometheus設定例

```yaml
# prometheus.yml
scrape_configs:
  - job_name: 'veil-proxy'
    static_configs:
      - targets: ['your-proxy-server:443']
    scheme: https
    tls_config:
      insecure_skip_verify: true  # 自己署名証明書の場合
    metrics_path: /__metrics
```

## HTTP/2サポート

HTTP/2（RFC 7540）をTLS ALPNネゴシエーションによりサポートします。

### 特徴

| 項目 | 効果 |
|------|------|
| ストリーム多重化 | 単一接続で複数リクエストを並列処理 |
| HPACKヘッダー圧縮 | ヘッダーオーバーヘッドを大幅削減 |
| サーバープッシュ | 先行リソース送信によるレイテンシ削減 |
| フロー制御 | ストリーム・コネクションレベルの制御 |

### 有効化

```bash
# HTTP/2フィーチャー付きでビルド
cargo build --release --features http2
```

```toml
# config.toml
[server]
listen = "0.0.0.0:443"
http2_enabled = true  # HTTP/2を有効化（ALPN h2）
```

### 詳細設定

`[http2]` セクションでHTTP/2プロトコルの詳細パラメータを設定できます：

```toml
[http2]
# HPACK動的テーブルサイズ（デフォルト: 65536）
header_table_size = 65536

# 同時ストリーム数（デフォルト: 256）
max_concurrent_streams = 256

# ストリームウィンドウサイズ（デフォルト: 1048576 = 1MB）
initial_window_size = 1048576

# 最大フレームサイズ（デフォルト: 65536）
max_frame_size = 65536

# 最大ヘッダーリストサイズ（デフォルト: 65536）
max_header_list_size = 65536

# コネクションウィンドウサイズ（デフォルト: 1048576 = 1MB）
connection_window_size = 1048576
```

### HTTP/1.1フォールバック

HTTP/2をサポートしないクライアントは自動的にHTTP/1.1にフォールバックします。

## HTTP/3サポート

HTTP/3（RFC 9114）をQUIC/UDPベースでサポートします。Cloudflare製の[quiche](https://github.com/cloudflare/quiche)を使用。

### 特徴

| 項目 | 効果 |
|------|------|
| 0-RTT接続確立 | TLSハンドシェイク不要で即時通信 |
| Head-of-Lineブロッキング解消 | パケットロスが他ストリームに影響しない |
| 接続マイグレーション | ネットワーク切り替え時も接続維持 |
| GSO/GRO最適化 | 高パフォーマンスUDP処理 |

### 有効化

```bash
# HTTP/3フィーチャー付きでビルド
cargo build --release --features http3
```

```toml
# config.toml
[server]
listen = "0.0.0.0:443"
http3_enabled = true  # HTTP/3を有効化（QUIC/UDP）
```

### 詳細設定

`[http3]` セクションでHTTP/3（QUIC）プロトコルの詳細パラメータを設定できます：

```toml
[http3]
# HTTP/3リッスンアドレス（UDP、未指定時はserver.listenと同じ）
listen = "0.0.0.0:443"

# 最大アイドルタイムアウト（ミリ秒、デフォルト: 30000）
max_idle_timeout = 30000

# 最大UDPペイロードサイズ（デフォルト: 1350）
max_udp_payload_size = 1350

# 初期最大データサイズ（コネクション全体、デフォルト: 10000000）
initial_max_data = 10000000

# 初期最大双方向ストリーム数
initial_max_streams_bidi = 100

# 初期最大単方向ストリーム数
initial_max_streams_uni = 100
```

### 注意事項

- HTTP/3はUDPベースのため、**kTLSは使用不可**です（TCPを使用しないため）
- UDPポート443をファイアウォールで開放する必要があります
- Alt-SvcヘッダーでブラウザにHTTP/3対応を通知できます

## kTLS（Kernel TLS）サポート

### 概要

kTLSはLinuxカーネルの機能で、TLSデータ転送フェーズの暗号化/復号化をカーネルレベルで行います。
本プロジェクトでは、rustls + ktls2 を使用してkTLSをサポートしています。

### パフォーマンス向上

| 項目 | 効果 |
|------|------|
| CPU使用率 | 20-40%削減（高負荷時） |
| スループット | 最大2倍向上 |
| レイテンシ | コンテキストスイッチ削減 |
| ゼロコピー | sendfile + TLS暗号化 |

### 有効化手順

```bash
# 1. カーネルモジュールのロード
sudo modprobe tls

# 2. ktlsフィーチャー付きでビルド
cargo build --release --features ktls

# 3. 設定ファイルで有効化（config.toml）
# [tls]
# ktls_enabled = true
# ktls_fallback_enabled = true  # オプション
```

### フォールバック設定

kTLSの有効化に失敗した場合の動作を `ktls_fallback_enabled` で制御できます：

| 設定値 | 動作 |
|--------|------|
| `true`（デフォルト） | kTLS失敗時はrustlsで継続（graceful degradation） |
| `false` | kTLS必須モード（失敗時は接続拒否） |

**フォールバック無効化 (`ktls_fallback_enabled = false`) のメリット:**

| 観点 | 効果 |
|------|------|
| パフォーマンス予測可能性 | すべての接続が確実にkTLSを使用 |
| デバッグ容易性 | kTLS/rustls混在状態がなくなる |
| 環境問題の早期発見 | kTLS利用不可時に即座に失敗 |

**注意:** フォールバック無効時は、kTLSが利用できない環境で接続が失敗します。
事前に `modprobe tls` でカーネルモジュールがロードされていることを確認してください。

```toml
[tls]
cert_path = "/path/to/cert.pem"
key_path = "/path/to/key.pem"
ktls_enabled = true
ktls_fallback_enabled = false  # kTLS必須モード
```

### 要件

- Linux 5.15以上（推奨、5.15未満でも動作可能）
- `tls`カーネルモジュールがロード済み
- AES-GCM暗号スイート（TLS 1.2/1.3）
- ktlsフィーチャーでビルド（`--features ktls`）

### 実装状況

**ktlsフィーチャー有効時（`--features ktls`）:**
- ✅ kTLSカーネルモジュールの可用性チェック
- ✅ TLSハンドシェイク完了後の自動kTLS有効化
- ✅ 送信（TX）と受信（RX）の両方でkTLSオフロード
- ✅ monoio (io_uring) との完全な非同期統合

**デフォルトビルド（rustls使用）:**
- ❌ kTLSはサポートされていない
- 👉 kTLSを使用するには `--features ktls` でビルドしてください

### セキュリティ考慮事項

| リスク | 緩和策 |
|--------|--------|
| カーネルバグ | カーネルバージョン固定、定期的なパッチ適用 |
| セッションキー露出 | TLSハンドシェイクはユーザースペース（rustls）で実行（PFS維持） |
| DoS攻撃 | カーネルリソース監視、レート制限 |

## パフォーマンスチューニング

### ワーカースレッド数

ワーカースレッド数は `config.toml` の `[server]` セクションで設定できます。

```toml
[server]
listen = "0.0.0.0:443"
threads = 0  # 未指定または0の場合はCPUコア数と同じ
```

| 設定 | 動作 |
|------|------|
| 未指定 | CPUコア数と同じスレッド数 |
| `threads = 0` | CPUコア数と同じスレッド数 |
| `threads = 4` | 4スレッドで起動 |

- 各ワーカースレッドはCPUコアにピン留めされます（CPUアフィニティ）
- コア数よりスレッド数が多い場合はラウンドロビンで割り当て
- メモリ制約がある環境では少なめに設定することを推奨

### SO_REUSEPORT CBPFロードバランシング

#### 概要

SO_REUSEPORTを使用して複数のワーカースレッドが同一ポートをリッスンする際、デフォルトではLinuxカーネルが3元タプルハッシュ（protocol + source IP + source port）で接続を振り分けます。CBPFモードでは、クライアントIPアドレスのみに基づいてワーカーを選択するカスタムBPFプログラムをカーネルにアタッチします。

#### 効果

| 項目 | Kernel（デフォルト） | CBPF |
|------|---------------------|------|
| 振り分けキー | protocol + src IP + src port | src IP のみ |
| 同一クライアント | source portで変動 | 常に同じワーカー |
| CPUキャッシュ効率 | 中 | 高（L1/L2ヒット率向上） |
| TLSセッション再開 | 低〜中 | 高（セッションキャッシュ活用） |

#### 設定

```toml
[performance]
# "kernel" = カーネルデフォルト（後方互換性）
# "cbpf"   = クライアントIPベースのCBPF（推奨）
reuseport_balancing = "cbpf"
```

#### 要件

- **Linux 4.6以上**（SO_ATTACH_REUSEPORT_CBPFサポート）
- CBPFアタッチ失敗時は自動的にカーネルデフォルトにフォールバック

### Huge Pages（Large OS Pages）

#### 概要

mimallocアロケータでHuge Pages（2MB）を使用することで、TLB（Translation Lookaside Buffer）ミスを削減し、パフォーマンスを向上させます。

#### 効果

| 項目 | 効果 |
|------|------|
| TLBミス | 大幅削減（ページテーブル参照の減少） |
| ページフォルト | 大容量メモリ使用時に減少 |
| パフォーマンス | 5-10%向上（ワークロード依存） |
| kTLS/splice | カーネル連携時に特に効果的 |

#### 設定

```toml
[performance]
huge_pages_enabled = true
```

#### OSレベルの設定（Linux）

```bash
# 一時的にHuge Pagesを有効化（128ページ = 256MB）
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# 永続化（/etc/sysctl.conf）
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.conf
sudo sysctl -p

# 現在のHuge Pages状態を確認
grep -i huge /proc/meminfo
```

#### コンテナ環境での注意

Docker/Kubernetes環境では、ホスト側でHuge Pagesを事前に確保する必要があります：

```bash
# ホスト側でHuge Pagesを確保
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# Docker起動時（オプション）
docker run --shm-size=256m ...

# Kubernetes（Pod仕様に追加）
# resources.limits.hugepages-2Mi: "256Mi"
```

Huge Pagesが利用できない場合は、自動的に通常の4KBページにフォールバックします。

### システム設定

```bash
# ファイルディスクリプタ上限
ulimit -n 65535

# カーネルパラメータ
sysctl -w net.core.somaxconn=65535
sysctl -w net.ipv4.tcp_max_syn_backlog=65535
sysctl -w net.core.netdev_max_backlog=65535

# io_uringの設定（必要に応じて）
sysctl -w kernel.io_uring_setup_flags=0
```

### バッファサイズとタイムアウト

コード内の定数（コンパイル時に設定、再ビルドが必要）：

```rust
// バッファサイズ
const BUF_SIZE: usize = 65536;           // 64KB - io_uring最適サイズ
const HEADER_BUF_CAPACITY: usize = 512;  // HTTPヘッダー用
const MAX_HEADER_SIZE: usize = 8192;     // 8KB - ヘッダーサイズ上限
const MAX_BODY_SIZE: usize = 10485760;   // 10MB - ボディサイズ上限

// タイムアウト
const READ_TIMEOUT: Duration = Duration::from_secs(30);   // 読み込みタイムアウト
const WRITE_TIMEOUT: Duration = Duration::from_secs(30);  // 書き込みタイムアウト
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10); // バックエンド接続タイムアウト
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);   // Keep-Aliveアイドルタイムアウト
```

> **注意**: ルートごとのセキュリティ設定で `client_header_timeout_secs` や `backend_connect_timeout_secs` を設定することで、一部のタイムアウトはconfig.tomlから個別に調整可能です。

## ベンチマーク

```bash
# wrk を使用したベンチマーク
wrk -t4 -c100 -d30s https://localhost/

# kTLS有効/無効での比較

# 1. kTLS無効（rustls使用）
cargo build --release
./veil -c ./config.toml &
wrk -t4 -c100 -d30s https://localhost/

# 2. kTLS有効（rustls + ktls2使用）
cargo build --release --features ktls
# config.tomlでktls_enabled = true
./veil -c ./config.toml &
wrk -t4 -c100 -d30s https://localhost/
```

## テスト

Veilには、ユニットテスト、統合テスト、エンドツーエンド（E2E）テストを含む包括的なテストスイートが含まれています。

### テスト概要

| テスト種別 | テスト数 | 状態 |
|-----------|---------|------|
| **ユニットテスト** | 251 | ✅ すべて成功 |
| **統合テスト** | 13 | ✅ すべて成功 |
| **E2Eテスト** | 24 | ✅ すべて成功 |
| **ベンチマーク** | 2ファイル | ✅ 準備完了 |

**合計: 288テスト - すべて成功 ✅**

### テストの実行

#### ユニットテスト

```bash
# すべてのユニットテストを実行
cargo test --features http2

# 特定のテストモジュールを実行
cargo test --features http2 tests::rate_limit_tests

# 出力付きで実行
cargo test --features http2 -- --nocapture
```

#### 統合テスト

```bash
# 統合テストを実行
cargo test --test integration_tests --features http2
```

#### E2Eテスト

E2Eテストは実行中のテスト環境が必要です。セットアップスクリプトを使用してください：

```bash
# 方法1: 自動実行（推奨）
./tests/e2e_setup.sh test

# 方法2: 手動実行
./tests/e2e_setup.sh start
cargo test --test e2e_tests --features http2 -- --test-threads=1
./tests/e2e_setup.sh stop

# クリーンアップのみ
./tests/e2e_setup.sh clean
```

#### ベンチマーク

```bash
# E2E環境を起動
./tests/e2e_setup.sh start

# すべてのベンチマークを実行
cargo bench --features http2

# 特定のベンチマークを実行
cargo bench --bench throughput --features http2
cargo bench --bench latency --features http2

# 環境を停止
./tests/e2e_setup.sh stop

# または自動化スクリプトを使用
./tests/run_bench.sh              # すべてのベンチマーク
./tests/run_bench.sh throughput   # スループットのみ
./tests/run_bench.sh latency      # レイテンシのみ
```

### テストカバレッジ

#### ユニットテスト (251テスト)

- **CIDR/IPフィルタリング**: IPアドレスフィルタリング、CIDR範囲検証
- **レート制限**: スライディングウィンドウレート制限、エントリ管理
- **設定パース**: TOMLパース、デフォルト値
- **ロードバランシング**: Round Robin、Least Connections、IP Hashアルゴリズム
- **ヘルスチェック**: サーバー状態管理、成功/失敗カウント
- **コネクションプール**: プール管理、タイムアウト検証
- **キャッシュ管理**: メモリ/ディスクキャッシュ、キー生成
- **HTTP/2**: フレームエンコード/デコード、HPACK圧縮
- **セキュリティ**: セキュリティ設定、カーネルバージョン検出
- **ユーティリティ**: 各種ヘルパー関数

#### 統合テスト (13テスト)

- TCP接続処理
- HTTPサーバーレスポンス
- 複数サーバー連携
- 動的ポート割り当て
- TLS証明書生成
- 設定ファイル生成
- ポート可用性ユーティリティ

#### E2Eテスト (24テスト)

- **プロキシコア**: 基本リクエスト、ヘルスエンドポイント
- **ヘッダー操作**: ヘッダーの追加/削除、バックエンドID
- **ロードバランシング**: Round Robin分散
- **静的ファイル配信**: インデックスファイル、大容量ファイル
- **圧縮**: gzip、brotli、優先順位処理
- **バックエンドアクセス**: 直接バックエンド接続
- **Prometheus**: メトリクスエンドポイント
- **エラーハンドリング**: 404レスポンス
- **HTTPリダイレクト**: HTTPからHTTPSへのリダイレクト
- **並行性**: 並行および順次リクエスト
- **パフォーマンス**: レスポンスタイム検証
- **Content-Type**: HTML、JSON処理
- **Keep-Alive**: 持続接続
- **カスタムヘッダー**: User-Agent、Hostヘッダー

### 環境クリーンアップ

すべてのテスト環境は自動的にクリーンアップされます：

- **Rust Dropトレイト**: サーバー構造体がスコープを抜けると自動終了
- **シェルスクリプトのtrap**: 成功/失敗/中断時にクリーンアップ
- **Graceful Shutdown**: SIGTERM → 待機 → SIGKILLの段階的終了
- **プロセスクリーンアップ**: 残存プロセスの自動クリーンアップ

クリーンアップ機構により、テスト結果に関わらず、テスト実行後にクリーンな状態が保証されます。

### テストファイル構造

```
veil-proxy/
├── src/
│   ├── main.rs          # 103ユニットテスト
│   ├── security.rs      # 26ユニットテスト
│   ├── cache/           # 50+ユニットテスト
│   ├── http2/           # 30+ユニットテスト
│   └── ...
├── tests/
│   ├── integration_tests.rs  # 13統合テスト
│   ├── e2e_tests.rs          # 24 E2Eテスト
│   ├── e2e_setup.sh          # E2E環境セットアップ
│   ├── run_bench.sh          # ベンチマーク自動化
│   └── common/
│       └── mod.rs            # テストユーティリティ
└── benches/
    ├── throughput.rs    # スループットベンチマーク
    └── latency.rs       # レイテンシベンチマーク
```

### 継続的インテグレーション

CI/CDパイプライン用の例：

```yaml
# GitHub Actionsワークフローの例
- name: テストを実行
  run: |
    cargo test --features http2 --all-targets
    
- name: E2Eテストを実行
  run: |
    ./tests/e2e_setup.sh test
```

## Graceful Shutdown

SIGINT（Ctrl+C）またはSIGTERMを受信すると、サーバーは安全に終了します：

1. 新規接続の受付を停止
2. 既存のリクエスト処理を完了
3. 全ワーカースレッドの終了を待機
4. プロセス終了

```bash
# サーバー起動
./veil -c ./config.toml &

# 安全に終了する
kill -SIGTERM $!
# または Ctrl+C
```

## Graceful Reload（ホットリロード）

SIGHUPを受信すると、サーバーは設定ファイルを再読み込みします。
既存の接続は中断されず、新しい接続から新しい設定が適用されます。

### 動作

1. SIGHUPシグナルを受信
2. 起動時に指定した設定ファイルを再読み込み
3. 設定のバリデーション
4. `ArcSwap` によるロックフリーな設定更新
5. 新規接続は新しい設定を使用

> **Note**: リロード時は起動時に `-c` オプションで指定したパス（またはデフォルトの `/etc/veil/config.toml`）が使用されます。

```bash
# 設定ファイルを編集
vim config.toml

# 設定を再読み込み（ゼロダウンタイム）
kill -SIGHUP $(pgrep veil)
```

### 対応する変更

| 項目 | ホットリロード対応 |
|------|-------------------|
| ルーティング設定 | ✅ |
| セキュリティ設定 | ✅ |
| Upstream設定 | ✅ |
| TLS証明書 | ❌ |
| リッスンアドレス | ❌（再起動が必要） |
| ワーカースレッド数 | ❌（再起動が必要） |

## WebSocketサポート

WebSocket（RFC 6455）のプロキシに対応しています。
`Connection: Upgrade` と `Upgrade: websocket` ヘッダーを自動検出し、
双方向のデータ転送を行います。

### 動作

1. クライアントからの Upgrade リクエストを検出
2. バックエンドに Upgrade リクエストを転送
3. 101 Switching Protocols を受信
4. 双方向のバイパス転送を開始（設定されたポーリングモードで動作）
5. どちらかの接続が閉じるまで継続

### ポーリングモード

WebSocket双方向転送時のポーリング動作を設定で制御できます。

| モード | 説明 | 用途 |
|--------|------|------|
| `adaptive`（デフォルト） | データ転送時は短く、アイドル時は長くなる | 汎用、CPU効率重視 |
| `fixed` | 常に固定のタイムアウトを使用 | リアルタイムゲーム、低レイテンシ最優先 |

詳細な設定オプションは「[WebSocket設定](#websocket設定)」セクションを参照してください。

### 設定例

WebSocketは通常のProxyバックエンドで自動的にサポートされます：

```toml
# WebSocketアプリケーション（デフォルト設定）
[path_routes."example.com"."/ws/"]
type = "Proxy"
url = "http://localhost:3000"

# 低レイテンシ設定（リアルタイムゲーム向け）
[path_routes."game.example.com"."/ws/"]
type = "Proxy"
url = "http://localhost:3001"

  [path_routes."game.example.com"."/ws/".security]
  websocket_poll_mode = "fixed"
  websocket_poll_timeout_ms = 1
```

### 対応バックエンド

| プロトコル | サポート |
|-----------|---------|
| HTTP → WS | ✅ |
| HTTPS → WSS | ✅ |

## ロードバランシング

複数のバックエンドサーバーへのリクエスト分散に対応しています。

### アルゴリズム

| アルゴリズム | 説明 | 用途 |
|-------------|------|------|
| `round_robin` | 順番に振り分け（デフォルト） | 汎用 |
| `least_conn` | 接続数が最小のサーバーを選択 | 長時間接続 |
| `ip_hash` | クライアントIPでハッシュ | セッション維持 |

### 設定例

```toml
# Upstreamグループの定義（文字列形式）
[upstreams."backend-pool"]
algorithm = "round_robin"
servers = [
  "http://localhost:8080",
  "http://localhost:8081",
  "http://localhost:8082"
]

# ルートでUpstreamを参照
[path_routes."example.com"."/api/"]
type = "Proxy"
upstream = "backend-pool"  # URLの代わりにupstreamを指定
```

#### SNI名付きHTTPSバックエンド

IPアドレス指定のHTTPSバックエンドに対してSNI名を指定できます：

```toml
# HTTPSバックエンドプール（構造体形式と文字列形式の混在）
[upstreams."https-api-pool"]
algorithm = "least_conn"
servers = [
  # 構造体形式: IPアドレス + SNI名指定
  { url = "https://192.168.1.100:443", sni_name = "api.internal.example.com" },
  { url = "https://192.168.1.101:443", sni_name = "api.internal.example.com" },
  # 文字列形式: ドメイン名指定（SNI名は自動的にURLのホスト名）
  "https://api.example.com:443"
]
```

### 単一バックエンドとの互換性

従来の `url` 指定も引き続き使用可能です：

```toml
# 従来の単一バックエンド指定
[path_routes."example.com"."/simple/"]
type = "Proxy"
url = "http://localhost:8080"
```

## ヘルスチェック（Health Check）

バックエンドサーバーの健康状態を監視し、異常なサーバーを自動的に除外します。

### 動作

1. バックグラウンドスレッドで定期的にHTTPリクエストを送信
2. レスポンスのステータスコードをチェック
3. 連続失敗回数が閾値に達したらサーバーを除外
4. 連続成功回数が閾値に達したらサーバーを復帰

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `interval_secs` | チェック間隔（秒） | 10 |
| `path` | チェック対象パス | `/` |
| `timeout_secs` | タイムアウト（秒） | 5 |
| `healthy_statuses` | 成功と判断するステータスコード | [200, 201, 202, 204, 301, 302, 304] |
| `unhealthy_threshold` | unhealthyにする連続失敗回数 | 3 |
| `healthy_threshold` | healthyに戻す連続成功回数 | 2 |
| `use_tls` | TLS接続を使用したヘルスチェック | **false** |
| `verify_cert` | TLS証明書の検証（use_tls=true時のみ有効） | **true** |

### 設定例

```toml
[upstreams."api-servers"]
algorithm = "least_conn"
servers = [
  "http://api1.internal:8080",
  "http://api2.internal:8080",
  "http://api3.internal:8080"
]

  [upstreams."api-servers".health_check]
  interval_secs = 10
  path = "/health"
  timeout_secs = 5
  healthy_statuses = [200]
  unhealthy_threshold = 3
  healthy_threshold = 2
  # TLSヘルスチェック（HTTPSバックエンド用）
  use_tls = false
  verify_cert = true
```

### TLSヘルスチェック

`use_tls = true` を設定すると、プレーンHTTPではなくTLS接続を使用してヘルスチェックを実行します。HTTPSバックエンドの監視に有用です。

**TLSヘルスチェックの設定例:**

```toml
[upstreams."api-servers"]
algorithm = "least_conn"
servers = [
  "https://api1.internal:8443",
  "https://api2.internal:8443"
]

  [upstreams."api-servers".health_check]
  interval_secs = 10
  path = "/health"
  timeout_secs = 5
  healthy_statuses = [200]
  # TLSヘルスチェックを有効化
  use_tls = true
  # 証明書検証（自己署名証明書の場合はfalseに設定）
  verify_cert = true
```

> **Note**: `verify_cert = false` に設定すると、自己署名証明書が許可されます。開発環境では有用ですが、本番環境では推奨されません。

### ログ出力

健康状態の変化はログに出力されます：

```
[INFO] Upstream api1.internal:8080 is now unhealthy
[INFO] Upstream api1.internal:8080 is now healthy
```

## 設定ファイルバリデーション

起動時に設定ファイルの詳細な検証を行い、問題があれば明確なエラーメッセージを出力します。

### 検証項目

| 項目 | チェック内容 |
|------|-------------|
| TLS証明書 | ファイルの存在確認 |
| TLS秘密鍵 | ファイルの存在確認 |
| リッスンアドレス | 有効なソケットアドレス形式 |
| Upstream URL | 有効なURL形式 |
| プロキシURL | 有効なURL形式 |
| ファイルパス | ファイル/ディレクトリの存在確認 |
| ファイルモード | `sendfile` または `memory` |

### エラーメッセージ例

```
Error: TLS certificate file not found: /path/to/cert.pem
Error: Invalid proxy URL for route 'example.com:/api/': invalid-url
Error: Upstream 'backend-pool' not found
```

## ログ設定

ftlogを使用した高性能非同期ログを提供します。ftlogは内部でバックグラウンドスレッドとチャネルを使用しており、ワーカースレッドへの影響を最小化しています。

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `level` | ログレベル（trace/debug/info/warn/error/off） | info |
| `format` | ログ出力形式（text/json） | text |
| `channel_size` | 内部チャネルバッファサイズ | 100000 |
| `flush_interval_ms` | ディスクフラッシュ間隔（ミリ秒） | 1000 |
| `max_log_size` | 最大ログファイルサイズ（バイト、0=無制限） | 104857600 |
| `file_path` | ログファイルパス（未指定で標準エラー出力） | なし |

### ログ出力形式

#### テキスト形式（デフォルト）

```
2024-01-01 00:00:00.000+00 0ms INFO main [main.rs:123] Server started
```

#### JSON形式

構造化ログ収集システム（Elasticsearch、Loki等）との連携に適しています。

```json
{"timestamp":"2024-01-01T00:00:00.000Z","level":"INFO","target":"veil","file":"main.rs","line":123,"message":"Server started"}
```

### 設定例

```toml
[logging]
level = "info"
format = "text"  # または "json"
channel_size = 100000
flush_interval_ms = 1000
file_path = "/var/log/veil.log"
```

### JSON形式設定例

```toml
[logging]
level = "info"
format = "json"
file_path = "/var/log/veil.json"
```

## 自己サンドボックス化（Self-Sandboxing）

このサーバーは、bubblewrapなどの外部ツールを使用せずに、**コード内から自己隔離**する機能を内蔵しています。

### なぜ外部ツールではなくコード内実装か？

| 方式 | メリット | デメリット |
|------|---------|-----------|
| bubblewrap (外部) | 柔軟な設定、既存ツール | 追加の依存、設定の複雑さ |
| **本サーバー (内蔵)** | ゼロ依存、コードで宣言、自動継承 | Linuxカーネル依存 |

### 実装済みの自己隔離機能

#### 1. Landlock ファイルシステム制限 (Linux 5.13+)

プロセスが「これ以降、このディレクトリ以外は見ません」と宣言できます。

```toml
[security]
enable_landlock = true
landlock_read_paths = ["/etc/veil", "/usr", "/lib", "/lib64"]
landlock_write_paths = ["/var/log/veil"]
```

**対応ABIバージョン:**

| ABI | カーネル | 機能 |
|-----|---------|------|
| v1 | 5.13+ | 基本的なファイルシステムアクセス制御 |
| v2 | 5.19+ | ファイル参照権限 (REFER) |
| v3 | 6.2+ | TRUNCATE権限 |
| v4 | 6.7+ | ioctl権限 |

#### 2. seccomp システムコール制限

許可リストに基づいてシステムコールを制限します。

```toml
[security]
enable_seccomp = true
seccomp_mode = "filter"  # "log" / "filter" / "strict"
```

**推奨導入手順:**

```bash
# 1. まずログモードで動作確認
enable_seccomp = true
seccomp_mode = "log"

# 2. ブロックされるシステムコールを確認
journalctl -f | grep -i seccomp

# 3. 問題なければfilterモードに変更
seccomp_mode = "filter"
```

#### 3. 権限降格 (Privilege Dropping)

root起動後、リスナー作成後に非特権ユーザーへ降格します。

```toml
[security]
drop_privileges_user = "veil"
drop_privileges_group = "veil"
```

### Namespace隔離について

> **注意**: `unshare(CLONE_NEWNET)` などのNamespace隔離は、リバースプロキシでは**非推奨**です。
> ネットワーク名前空間を分離するとプロキシ機能が失われます。
> 
> Namespace隔離が必要な場合は、**systemdレベル**で行うことを推奨します（下記参照）。

## セキュリティ強化（systemd サンドボックス化）

io_uringは強力な非同期I/Oインターフェースですが、悪用されるとカーネル権限を奪われるリスクがあります。
このサーバーはsystemdのサンドボックス機能と組み合わせることで、堅牢なセキュリティを実現できます。

### セキュリティアーキテクチャ（多層防御）

```
┌─────────────────────────────────────────────────────────────────┐
│ systemd (PID 1) - 外側の隔離層                                   │
│ ┌─────────────────────────────────────────────────────────────┐ │
│ │ Namespace 隔離 (ProtectSystem, PrivateTmp, PrivateDevices)  │ │
│ │ ┌─────────────────────────────────────────────────────────┐ │ │
│ │ │ veil 内蔵セキュリティ                        │ │ │
│ │ │ ┌─────────────────────────────────────────────────────┐ │ │ │
│ │ │ │ Landlock (ファイルシステム制限)                     │ │ │ │
│ │ │ │ ┌─────────────────────────────────────────────────┐ │ │ │ │
│ │ │ │ │ seccomp (システムコール制限)                    │ │ │ │ │
│ │ │ │ │ ┌─────────────────────────────────────────────┐ │ │ │ │ │
│ │ │ │ │ │ アプリケーション (io_uring + rustls)        │ │ │ │ │ │
│ │ │ │ │ │ - 許可: io_uring_*, socket, read, write...  │ │ │ │ │ │
│ │ │ │ │ │ - 拒否: fork, execve, ptrace, mount...      │ │ │ │ │ │
│ │ │ │ │ └─────────────────────────────────────────────┘ │ │ │ │ │
│ │ │ │ └─────────────────────────────────────────────────┘ │ │ │ │
│ │ │ └─────────────────────────────────────────────────────┘ │ │ │
│ │ └─────────────────────────────────────────────────────────┘ │ │
│ └─────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### 必須システムコール一覧

このサーバーが動作するために必要な最小限のシステムコールです：

| カテゴリ | システムコール | 用途 |
|---------|---------------|------|
| **io_uring** | `io_uring_setup`, `io_uring_enter`, `io_uring_register` | monoio ランタイム |
| **ネットワーク** | `socket`, `bind`, `listen`, `accept4`, `connect`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg`, `setsockopt`, `getsockopt` | TCP/UDP ソケット |
| **ファイルI/O** | `openat`, `read`, `write`, `close`, `fstat`, `readv`, `writev` | 設定、証明書、ログ |
| **メモリ** | `mmap`, `munmap`, `mprotect`, `brk`, `madvise`, `mremap`, `mlock`, `mlock2` | mimalloc、Huge Pages、io_uring登録バッファ |
| **スレッド** | `clone`, `clone3`, `futex`, `exit_group`, `set_tid_address` | ワーカースレッド |
| **CPUアフィニティ** | `sched_setaffinity`, `sched_getaffinity` | CPUピンニング |
| **シグナル** | `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn` | SIGTERM/SIGHUP |
| **時間** | `clock_gettime`, `nanosleep` | タイムアウト |
| **その他** | `prctl`, `ioctl`, `getrandom`, `fcntl`, `uname` | 各種制御 |

### systemd サービスファイル

`contrib/systemd/veil.service` にサンドボックス化対応のサービスファイルを用意しています。

#### インストール

```bash
# 1. 専用ユーザーを作成
sudo useradd -r -s /sbin/nologin veil

# 2. ディレクトリを作成
sudo mkdir -p /etc/veil
sudo mkdir -p /var/log/veil
sudo chown veil:veil /var/log/veil

# 3. 設定ファイルをコピー
sudo cp config.toml /etc/veil/
sudo cp server.crt server.key /etc/veil/
sudo chmod 600 /etc/veil/server.key
sudo chown -R veil:veil /etc/veil

# 4. バイナリをインストール
sudo cp target/release/veil /usr/local/bin/

# 5. サービスファイルをインストール
sudo cp contrib/systemd/veil.service /etc/systemd/system/
sudo systemctl daemon-reload

# 6. サービスを有効化・起動
sudo systemctl enable veil
sudo systemctl start veil
```

#### 重要な設定項目

```ini
[Service]
# === ユーザー ===
User=veil
Group=veil

# === リソース制限 ===
# io_uring 登録バッファにはメモリロックが必要
LimitMEMLOCK=infinity
LimitNOFILE=1048576

# === ファイルシステム隔離 ===
ProtectSystem=strict
ProtectHome=yes
PrivateTmp=yes
PrivateDevices=yes
ReadOnlyPaths=/etc/veil
ReadWritePaths=/var/log/veil

# === 名前空間隔離 ===
RestrictNamespaces=yes
ProtectKernelModules=yes
ProtectKernelLogs=yes
ProtectKernelTunables=yes

# === ネットワーク ===
RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX AF_NETLINK

# === セキュリティ強化 ===
NoNewPrivileges=yes
MemoryDenyWriteExecute=yes
RestrictSUIDSGID=yes

# === システムコール制限 ===
# @system-service + io_uring + mlock
SystemCallFilter=@system-service
SystemCallFilter=io_uring_setup io_uring_enter io_uring_register
SystemCallFilter=mlock mlock2 mlockall munlock munlockall
SystemCallFilter=sched_setaffinity sched_getaffinity
SystemCallErrorNumber=EPERM
```

### Huge Pages の有効化

io_uring と mimalloc のパフォーマンスを最大化するには、Huge Pages を有効化します。

```bash
# 1. Huge Pages を確保（128 * 2MB = 256MB）
echo 128 | sudo tee /proc/sys/vm/nr_hugepages

# 2. 永続化
echo "vm.nr_hugepages=128" | sudo tee -a /etc/sysctl.d/99-veil.conf
sudo sysctl -p /etc/sysctl.d/99-veil.conf

# 3. systemd で MEMLOCK 制限を解除
# veil.service に LimitMEMLOCK=infinity を設定
```

### セキュリティ検証

サービスのセキュリティ状態を確認する方法：

```bash
# systemd-analyze で設定を検証
systemd-analyze security veil.service

# 実行中のセキュリティ状態を確認
cat /proc/$(pgrep veil)/status | grep -E "Seccomp|NoNewPrivs|CapBnd"

# 期待される出力:
# Seccomp:        2                    # seccomp フィルタが有効
# NoNewPrivs:     1                    # 新規特権取得不可
# CapBnd:         0000000000000c00     # CAP_NET_BIND_SERVICE のみ
```

### トラブルシューティング

#### io_uring が動作しない

```bash
# 原因: システムコールがブロックされている
# 解決: SystemCallFilter に io_uring_* を追加
journalctl -u veil | grep -i "seccomp"

# 手動テスト
sudo strace -f -e trace=io_uring_setup /usr/local/bin/veil -c /etc/veil/config.toml
```

#### メモリロックに失敗

```bash
# 原因: MEMLOCK 制限が低い
# 解決: LimitMEMLOCK=infinity を設定
cat /proc/$(pgrep veil)/limits | grep "locked memory"
```

#### 特権ポート (443/80) にバインドできない

```bash
# 原因: CAP_NET_BIND_SERVICE がない
# 解決 1: systemd で設定
#   AmbientCapabilities=CAP_NET_BIND_SERVICE

# 解決 2: バイナリにケイパビリティを付与
sudo setcap 'cap_net_bind_service=+ep' /usr/local/bin/veil
```

### 代替: bubblewrap との併用

より厳格な隔離が必要な場合は、systemd と bubblewrap を併用できます：

```ini
[Service]
ExecStart=/usr/bin/bwrap \
    --ro-bind /usr /usr \
    --ro-bind /lib /lib \
    --ro-bind /lib64 /lib64 \
    --ro-bind /etc/veil /etc/veil \
    --bind /var/log/veil /var/log/veil \
    --unshare-pid \
    --die-with-parent \
    /usr/local/bin/veil -c /etc/veil/config.toml
```

この構成では、systemd が外側の「器」を作り、bubblewrap がさらに厳格なファイルシステムビューを提供します。

## 参考資料

### コアライブラリ

- [monoio](https://github.com/bytedance/monoio): io_uringベースの非同期ランタイム
- [rustls](https://github.com/rustls/rustls): Pure Rust TLS実装
- [ktls2](https://crates.io/crates/ktls2): rustls用kTLS統合クレート
- [httparse](https://crates.io/crates/httparse): 高速HTTPパーサー
- [quiche](https://github.com/cloudflare/quiche): Cloudflare製 QUIC/HTTP/3実装

### パフォーマンス

- [mimalloc](https://github.com/microsoft/mimalloc): 高速汎用メモリアロケータ
- [matchit](https://crates.io/crates/matchit): 高速Radix Treeルーター
- [ftlog](https://crates.io/crates/ftlog): 高性能非同期ログライブラリ
- [memchr](https://crates.io/crates/memchr): SIMD最適化文字列検索
- [Linux Huge Pages](https://docs.kernel.org/admin-guide/mm/hugetlbpage.html): Large OS Pages設定ガイド

### モニタリング

- [prometheus](https://crates.io/crates/prometheus): Prometheusメトリクスライブラリ

### CLI・並行制御

- [clap](https://crates.io/crates/clap): コマンドライン引数パーサー
- [arc-swap](https://crates.io/crates/arc-swap): ロックフリーなArc交換（設定ホットリロード用）
- [ctrlc](https://crates.io/crates/ctrlc): シグナルハンドリング（Graceful Shutdown用）
- [signal-hook](https://crates.io/crates/signal-hook): SIGHUPハンドリング（Graceful Reload用）
- [core_affinity](https://crates.io/crates/core_affinity): CPUアフィニティ設定

### カーネル機能

- [Linux Kernel TLS](https://docs.kernel.org/networking/tls.html): kTLSドキュメント
- [io_uring](https://kernel.dk/io_uring.pdf): io_uring設計ドキュメント
- [SO_REUSEPORT](https://lwn.net/Articles/542629/): ポート共有とロードバランシング

### セキュリティ

- [systemd.exec](https://www.freedesktop.org/software/systemd/man/systemd.exec.html): systemdセキュリティ設定
- [seccomp](https://docs.kernel.org/userspace-api/seccomp_filter.html): Seccomp BPFフィルタ
- [Landlock](https://docs.kernel.org/userspace-api/landlock.html): ファイルシステムサンドボックス
- [io_uring Security](https://www.kernel.org/doc/html/latest/userspace-api/io_uring.html): io_uringセキュリティ考慮事項
- [bubblewrap](https://github.com/containers/bubblewrap): 非特権サンドボックスツール

## ロゴ

<table align="center">
  <tr>
    <th align="center">メインロゴ (WebP)</th>
    <th align="center">代替ロゴ (SVG)</th>
    <th align="center">ロゴ文字 (SVG)</th>
  </tr>
  <tr>
    <td align="center">
      <img src="docs/images/veil_logo.webp" alt="Veil メインロゴ" width="200" />
    </td>
    <td align="center">
      <img src="docs/images/veil_logo_alternative.svg" alt="Veil 代替ロゴ" width="200" />
    </td>
    <td align="center">
      <img src="docs/images/veil_logo_text.svg" alt="Veil ロゴ文字" width="200" />
    </td>
  </tr>
</table>

## ライセンス

Apache License 2.0

(c) 2025 aofusa
