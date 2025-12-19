# High-Performance Reverse Proxy Server

io_uring (monoio) と rustls を使用した高性能リバースプロキシサーバー。

## 特徴

### コア機能
- **非同期I/O**: monoio (io_uring) による効率的なI/O処理
- **TLS**: rustls によるメモリ安全な Pure Rust TLS実装
- **kTLS**: rustls + ktls2 によるカーネルTLSオフロード対応（Linux 5.15+）
- **高速アロケータ**: mimalloc による高速メモリ割り当て + Huge Pages対応
- **高速ルーティング**: Radix Tree (matchit) によるO(log n)パスマッチング

### プロキシ機能
- **コネクションプール**: バックエンド接続の再利用によるレイテンシ削減（HTTP/HTTPS両対応）
- **ロードバランシング**: 複数バックエンドへのリクエスト分散（Round Robin/Least Connections/IP Hash）
- **健康チェック**: HTTPベースのアクティブヘルスチェックによる自動フェイルオーバー
- **WebSocketサポート**: Upgradeヘッダー検知による双方向プロキシ
- **ヘッダー操作**: リクエスト/レスポンスヘッダーの追加・削除（X-Real-IP, HSTS等）
- **リダイレクト**: 301/302/307/308 HTTPリダイレクト（パス保持オプション付き）

### HTTP処理
- **Keep-Alive**: HTTP/1.1 Keep-Alive完全サポート
- **Chunked転送**: RFC 7230準拠のChunkedデコーダ（ステートマシンベース）
- **バッファプール**: メモリアロケーションの削減

### パフォーマンス
- **CPUアフィニティ**: ワーカースレッドのCPUコアピン留め
- **CBPF振り分け**: SO_REUSEPORTのクライアントIPベースロードバランシング（Linux 4.6+）

### 運用機能
- **Graceful Shutdown**: SIGINT/SIGTERMによる優雅な終了
- **Graceful Reload**: SIGHUPによる設定のホットリロード（ゼロダウンタイム）
- **非同期ログ**: ftlog による高性能非同期ログ
- **設定バリデーション**: 起動時の詳細な設定ファイル検証
- **Prometheusメトリクス**: `/__metrics` エンドポイントでリクエスト数、レイテンシ等を出力

### セキュリティ
- **同時接続数制限**: グローバルな接続数上限設定
- **レートリミッター**: スライディングウィンドウ方式のレート制限
- **IP制限**: CIDR対応のIPアドレスフィルタリング
- **権限降格**: root起動後の非特権ユーザーへの降格

## ビルド

```bash
# 通常ビルド（rustls使用）
cargo build --release

# kTLSサポート付きビルド（rustls + ktls2）
cargo build --release --features ktls
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

`config.toml`:

```toml
[server]
listen = "0.0.0.0:443"
# ワーカースレッド数（オプション）
# 未指定または0の場合はCPUコア数と同じスレッド数を使用
threads = 4

[logging]
# ログレベル: "trace", "debug", "info", "warn", "error", "off"
level = "info"
# ログチャネルサイズ（高負荷時のログドロップ防止）
channel_size = 100000
# フラッシュ間隔（ミリ秒）
flush_interval_ms = 1000
# 最大ログファイルサイズ（バイト、0=ローテーションなし）
max_log_size = 104857600
# ログファイルパス（オプション、未指定で標準エラー出力）
# file_path = "/var/log/zerocopy-server.log"

[security]
# 権限降格設定（Linux専用）
# drop_privileges_user = "nobody"
# drop_privileges_group = "nogroup"
# グローバル同時接続上限（0 = 無制限）
# max_concurrent_connections = 10000

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
| `File` | 静的ファイル配信 | `{ type = "File", path = "/var/www", mode = "sendfile" }` |
| `Redirect` | HTTPリダイレクト | `{ type = "Redirect", redirect_url = "https://new.example.com", redirect_status = 301 }` |

> **Note**: `Proxy` タイプは `url`（単一バックエンド）または `upstream`（ロードバランシング）のいずれかを指定します。WebSocketは両方で自動サポートされます。

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

  # 健康チェック（オプション）
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

### WebSocket設定

WebSocketは通常のProxyで自動サポート：

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

### グローバルセキュリティ設定

`[security]` セクションでサーバー全体のセキュリティ設定を行います。

```toml
[security]
# 権限降格設定（Linux専用、root起動時のみ有効）
drop_privileges_user = "nobody"
drop_privileges_group = "nogroup"

# グローバル同時接続上限（0 = 無制限）
max_concurrent_connections = 10000
```

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `drop_privileges_user` | 起動後に降格するユーザー名 | なし |
| `drop_privileges_group` | 起動後に降格するグループ名 | なし |
| `max_concurrent_connections` | 同時接続数の上限 | 0（無制限） |

> **注意**: 特権ポート（1024未満）を使用する場合は、`CAP_NET_BIND_SERVICE` ケイパビリティを付与するか、非特権ポートを使用してください。
>
> ```bash
> sudo setcap 'cap_net_bind_service=+ep' ./target/release/zerocopy-server
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

## Prometheusメトリクス

リクエスト数、レイテンシ、ボディサイズなどのメトリクスをPrometheus形式でエクスポートします。

### エンドポイント

```
GET /__metrics
```

このエンドポイントは自動的に有効化され、Prometheusからのスクレイピングに対応しています。

### 利用可能なメトリクス

| メトリクス | タイプ | ラベル | 説明 |
|-----------|--------|--------|------|
| `zerocopy_proxy_http_requests_total` | Counter | method, status, host | リクエスト総数 |
| `zerocopy_proxy_http_request_duration_seconds` | Histogram | method, host | リクエスト処理時間（秒） |
| `zerocopy_proxy_http_request_size_bytes` | Histogram | - | リクエストボディサイズ |
| `zerocopy_proxy_http_response_size_bytes` | Histogram | - | レスポンスボディサイズ |

### Grafanaダッシュボード例

```promql
# リクエストレート（リクエスト/秒）
rate(zerocopy_proxy_http_requests_total[5m])

# エラー率（4xx + 5xx）
sum(rate(zerocopy_proxy_http_requests_total{status=~"4..|5.."}[5m])) 
  / sum(rate(zerocopy_proxy_http_requests_total[5m]))

# レイテンシP95
histogram_quantile(0.95, rate(zerocopy_proxy_http_request_duration_seconds_bucket[5m]))

# ホスト別リクエストレート
sum by (host) (rate(zerocopy_proxy_http_requests_total[5m]))
```

### Prometheus設定例

```yaml
# prometheus.yml
scrape_configs:
  - job_name: 'zerocopy-proxy'
    static_configs:
      - targets: ['your-proxy-server:443']
    scheme: https
    tls_config:
      insecure_skip_verify: true  # 自己署名証明書の場合
    metrics_path: /__metrics
```

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
./target/release/zerocopy-server &
wrk -t4 -c100 -d30s https://localhost/

# 2. kTLS有効（rustls + ktls2使用）
cargo build --release --features ktls
# config.tomlでktls_enabled = true
./target/release/zerocopy-server &
wrk -t4 -c100 -d30s https://localhost/
```

## Graceful Shutdown

SIGINT（Ctrl+C）またはSIGTERMを受信すると、サーバーは優雅に終了します：

1. 新規接続の受付を停止
2. 既存のリクエスト処理を完了
3. 全ワーカースレッドの終了を待機
4. プロセス終了

```bash
# サーバー起動
./target/release/zerocopy-server &

# 優雅な終了
kill -SIGTERM $!
# または Ctrl+C
```

## Graceful Reload（ホットリロード）

SIGHUPを受信すると、サーバーは設定ファイルを再読み込みします。
既存の接続は中断されず、新しい接続から新しい設定が適用されます。

### 動作

1. SIGHUPシグナルを受信
2. `config.toml` を再読み込み
3. 設定のバリデーション
4. `ArcSwap` によるロックフリーな設定更新
5. 新規接続は新しい設定を使用

```bash
# 設定ファイルを編集
vim config.toml

# 設定を再読み込み（ゼロダウンタイム）
kill -SIGHUP $(pgrep zerocopy-server)
```

### 対応する変更

| 項目 | ホットリロード対応 |
|------|-------------------|
| ルーティング設定 | ✅ |
| セキュリティ設定 | ✅ |
| Upstream設定 | ✅ |
| TLS証明書 | ✅ |
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
4. 双方向のバイパス転送を開始
5. どちらかの接続が閉じるまで継続

### 設定例

WebSocketは通常のProxyバックエンドで自動的にサポートされます：

```toml
# WebSocketアプリケーション
[path_routes."example.com"."/ws/"]
type = "Proxy"
url = "http://localhost:3000"
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
# Upstreamグループの定義
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

### 単一バックエンドとの互換性

従来の `url` 指定も引き続き使用可能です：

```toml
# 従来の単一バックエンド指定
[path_routes."example.com"."/simple/"]
type = "Proxy"
url = "http://localhost:8080"
```

## 健康チェック（Health Check）

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
```

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
| `channel_size` | 内部チャネルバッファサイズ | 100000 |
| `flush_interval_ms` | ディスクフラッシュ間隔（ミリ秒） | 1000 |
| `max_log_size` | 最大ログファイルサイズ（バイト、0=無制限） | 104857600 |
| `file_path` | ログファイルパス（未指定で標準エラー出力） | なし |

### 設定例

```toml
[logging]
level = "info"
channel_size = 100000
flush_interval_ms = 1000
file_path = "/var/log/zerocopy-server.log"
```

## 参考資料

### コアライブラリ

- [monoio](https://github.com/bytedance/monoio): io_uringベースの非同期ランタイム
- [rustls](https://github.com/rustls/rustls): Pure Rust TLS実装
- [ktls2](https://crates.io/crates/ktls2): rustls用kTLS統合クレート
- [httparse](https://crates.io/crates/httparse): 高速HTTPパーサー

### パフォーマンス

- [mimalloc](https://github.com/microsoft/mimalloc): 高速汎用メモリアロケータ
- [matchit](https://crates.io/crates/matchit): 高速Radix Treeルーター
- [ftlog](https://crates.io/crates/ftlog): 高性能非同期ログライブラリ
- [memchr](https://crates.io/crates/memchr): SIMD最適化文字列検索
- [Linux Huge Pages](https://docs.kernel.org/admin-guide/mm/hugetlbpage.html): Large OS Pages設定ガイド

### モニタリング

- [prometheus](https://crates.io/crates/prometheus): Prometheusメトリクスライブラリ

### 並行制御

- [arc-swap](https://crates.io/crates/arc-swap): ロックフリーなArc交換（設定ホットリロード用）
- [ctrlc](https://crates.io/crates/ctrlc): シグナルハンドリング（Graceful Shutdown用）
- [signal-hook](https://crates.io/crates/signal-hook): SIGHUPハンドリング（Graceful Reload用）
- [core_affinity](https://crates.io/crates/core_affinity): CPUアフィニティ設定

### カーネル機能

- [Linux Kernel TLS](https://docs.kernel.org/networking/tls.html): kTLSドキュメント
- [io_uring](https://kernel.dk/io_uring.pdf): io_uring設計ドキュメント
- [SO_REUSEPORT](https://lwn.net/Articles/542629/): ポート共有とロードバランシング

## ライセンス

Apache License 2.0

(c) 2025 aofusa
