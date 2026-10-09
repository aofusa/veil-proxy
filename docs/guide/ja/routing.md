# ルーティング・リダイレクト・ヘッダー

[← ドキュメント目次](README.md) · [English](../routing.md)

## ルーティング

### 統合ルーティング（AWS ALB準拠）

配列の順序で評価（first-match方式）。すべてのルートは統合された `[[route]]` 構造を使用し、`conditions` と `action` フィールドを持ちます。

1. **ルート条件** (`[route.conditions]`): ホスト、パス、ヘッダー、メソッド、クエリパラメータ、またはソースIPでマッチ
   - `host`: ホストヘッダーマッチ（ワイルドカード対応、例: "api.example.com", "*.example.com"）
   - `path`: パスパターンマッチ（ワイルドカード対応、例: "/api/*", "/static/*"）
   - `header`: HTTPヘッダーマッチ（マップで複数ヘッダー指定可能、例: `{ "X-Version" = "v2" }`）
   - `method`: HTTPリクエストメソッドマッチ（配列で複数メソッド指定可能、例: `["GET", "POST"]`）
   - `query`: クエリパラメータマッチ（マップで複数クエリ指定可能、例: `{ "token" = "secret" }`）
   - `source_ip`: ソースIPマッチ（CIDR表記、配列で複数CIDR指定可能、例: `["192.168.0.0/16", "10.0.0.0/8"]`）
   - すべての条件はANDで結合されます。条件が指定されていない場合は、すべてのリクエストにマッチします（デフォルトルート）。
2. **ルートアクション** (`[route.action]`): バックエンドアクション（File、Proxy、Redirectなど）
3. **ルートレベルの設定** (`[route.security]`, `[route.cache]`, `[route.compression]`, `[route.buffering]`, `[route.open_file_cache]`): actionレベルの設定をオーバーライド
4. **ルートレベルのWASMモジュール** (`modules`): このルートに適用するWASMモジュール名のリスト（route直下で設定、`route.action`配下ではない）

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
[[route]]
[route.conditions]
host = "example.com"
path = "/robots.txt"
[route.action]
type = "File"
path = "/var/www/robots.txt"
```

#### 2. ディレクトリ配信（Alias動作）

設定の `path` がディレクトリの場合、プレフィックスを除去した残りのパスをディレクトリに結合します。
**末尾スラッシュの有無は問いません**（どちらでも同じ動作）。

```toml
# 末尾スラッシュあり（従来の書き方）
[[route]]
[route.conditions]
host = "example.com"
path = "/static/*"
[route.action]
type = "File"
path = "/var/www/assets/"

# 末尾スラッシュなし（同じ動作、301リダイレクトなし）
[[route]]
[route.conditions]
host = "example.com"
path = "/docs"
[route.action]
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
[[route]]
[route.conditions]
host = "example.com"
path = "/user/*"
[route.action]
type = "File"
path = "/var/www/user/"
index = "profile.html"

# /app/ → /var/www/app/dashboard.html を返す
[[route]]
[route.conditions]
host = "example.com"
path = "/app/*"
[route.action]
type = "File"
path = "/var/www/app/"
index = "dashboard.html"
```

#### 4. プロキシ（Proxy Pass動作）

プレフィックスを除去した残りのパスをバックエンドURLに結合します。
**末尾スラッシュの有無は問いません**。

```toml
# 末尾スラッシュあり
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/app/"

# 末尾スラッシュなし（同じ動作）
[[route]]
[route.conditions]
host = "example.com"
path = "/backend"
[route.action]
type = "Proxy"
url = "http://localhost:3000"
```

| リクエスト | 設定 | 転送先 |
|-----------|------|--------|
| `/api/v1/users` | `"/api/"` → `url = ".../app/"` | `http://localhost:8080/app/v1/users` |
| `/backend` | `"/backend"` → `url = ".../"` | `http://localhost:3000/` |
| `/backend/users` | `"/backend"` | `http://localhost:3000/users` |

### ルート条件の例

すべての条件はANDで結合されます。条件が指定されていない場合は、すべてのリクエストにマッチします（デフォルトルート）。

#### ホストとパス条件

```toml
# ホストベースルーティング
[[route]]
[route.conditions]
host = "api.example.com"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

# パスベースルーティング
[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"
```

#### HTTPヘッダー条件

```toml
# X-Versionヘッダーが"v2"の場合のみマッチ
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
header = { "X-Version" = "v2" }
[route.action]
type = "Proxy"
url = "http://localhost:8080/v2/"

# 複数ヘッダー（すべてマッチする必要がある）
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
header = { "X-Version" = "v2", "X-API-Key" = "secret" }
[route.action]
type = "Proxy"
url = "http://localhost:8080/v2/"
```

#### HTTPメソッド条件

```toml
# GETとPOSTのみマッチ
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
method = ["GET", "POST"]
[route.action]
type = "Proxy"
url = "http://localhost:8080/"
```

#### クエリパラメータ条件

```toml
# tokenクエリパラメータが"secret"の場合のみマッチ
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
query = { "token" = "secret" }
[route.action]
type = "Proxy"
url = "http://localhost:8080/"

# 複数クエリパラメータ（すべてマッチする必要がある）
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/*"
query = { "format" = "json", "version" = "1" }
[route.action]
type = "Proxy"
url = "http://localhost:8080/"
```

#### ソースIP条件

```toml
# 特定のCIDR範囲からのアクセスのみマッチ
[[route]]
[route.conditions]
host = "admin.example.com"
path = "/admin/*"
source_ip = ["192.168.0.0/16", "10.0.0.0/8"]
[route.action]
type = "Proxy"
url = "http://localhost:9000/"
```

#### 複数条件の組み合わせ

```toml
# すべての条件がマッチする必要がある（AND論理）
[[route]]
[route.conditions]
host = "api.example.com"
path = "/api/v2/*"
header = { "X-Version" = "v2", "X-API-Key" = "secret" }
method = ["GET", "POST"]
query = { "format" = "json" }
source_ip = ["192.168.0.0/16"]
[route.action]
type = "Proxy"
url = "http://localhost:8080/v2/"
```

### Proxy-Wasm拡張機能（ルートレベル設定）

WASMモジュールはroute直下で設定します（`route.action`配下ではありません）：

```toml
[[route]]
# このルートに適用するWASMモジュール名のリスト（[route.action] 配下ではなく [[route]] 直下）
modules = ["header_filter", "waf_filter"]

# 任意（F-148）: このルートだけモジュールのプラグイン設定を上書きする。
# ここに書けるのは上の modules に列挙したモジュール名のみ。
[route.module_configuration.waf_filter]
mode = "log_only"

[route.conditions]
host = "api.example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/"
```

値の型と合成規則は「[プラグイン設定の記述方法（F-148）](wasm.md#プラグイン設定の記述方法f-148)」を参照。

### ファイル配信モード

| モード | 説明 | 用途 |
|--------|------|------|
| `sendfile` | sendfileシステムコールでゼロコピー送信 | 大きなファイル、動画、画像 |
| `memory` | ファイルをメモリに読み込んで配信 | 小さなファイル、favicon.ico等 |

```toml
# ディレクトリ配信（sendfileモード）
[[route]]
[route.conditions]
host = "example.com"
path = "/static/*"
[route.action]
type = "File"
path = "/var/www/static"
mode = "sendfile"

# 単一ファイル配信（memoryモード）
#
# 注意（F-159）: ファイルは**設定ロード時に 1 回だけ**読み込まれ、以降はメモリから配信される。
# ディスク上のファイルを差し替えた場合は設定リロード（SIGHUP）が必要。
# F-159 以前はリクエストごとにファイルを読み直しており、memory モードの意味を成していなかった。
[[route]]
[route.conditions]
host = "example.com"
path = "/favicon.ico"
[route.action]
type = "File"
path = "/var/www/favicon.ico"
mode = "memory"

# typeとmodeを省略した場合のデフォルト（type = "File", mode = "sendfile"）
[[route]]
[route.conditions]
host = "example.com"
path = "/"
[route.action]
path = "/var/www/html"
```

### プロキシ設定

HTTPおよびHTTPSバックエンドへのプロキシに対応：

```toml
# HTTPバックエンド
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

# HTTPSバックエンド（TLSクライアント接続）
[[route]]
[route.conditions]
host = "example.com"
path = "/secure/*"
[route.action]
type = "Proxy"
url = "https://backend.example.com"
```

veil は上流（`https://` バックエンド）へは常に HTTP/1.1 で接続する。ALPN で `h2` を
提示しないため、HTTPS バックエンドが h2 を選択することはない。TLS 上の HTTP/2 上流は
非対応であり、平文の HTTP/2 上流は後述の `use_h2c` で対応する。

### H2C (HTTP/2 over cleartext) プロキシ

バックエンドがH2C（TLSなしのHTTP/2）をサポートしている場合、`use_h2c = true` を指定することでHTTP/2で通信できます。

```toml
# gRPCバックエンドへのH2C接続
[[route]]
[route.conditions]
host = "example.com"
path = "/grpc/*"
[route.action]
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

**H2C バックエンド接続の多重化（F-174）**: ワーカーごとにバックエンドのアドレス単位で上流 HTTP/2 接続をプールし、**1 本の接続を複数の要求が同時に共有**します（接続ごとに 1 つのアクタータスクが HEADERS/DATA を書き出し、受信フレームをストリームごとのチャネルへ振り分けます）。新しい接続を張るのは、プール内のすべての接続が相手の `SETTINGS_MAX_CONCURRENT_STREAMS` に達したか GOAWAY を受けたときだけです。要求本文は接続・ストリーム両方のフロー制御ウィンドウに従い（B-106）、受信した DATA は下流が受け取ってから WINDOW_UPDATE します。ストリームの無い接続は `idle_connection_timeout_secs` 後に GOAWAY で閉じます（`0` でプール無効）。アクターは上流からの切断を即座に検知するため、切れた接続を割り当てることはありません。上流に拒否されたストリーム（REFUSED_STREAM、GOAWAY の `last_stream_id` より後）と、応答 head より前に失敗した冪等な要求は、新しいストリームで 1 回だけ再送します。これにより gRPC 中継（`クライアント → veil (TLS h2/h3) → バックエンド (h2c)`）で呼び出しごとのハンドシェイクと上流接続を回避できます。

> **Note**: H2CはHTTPSバックエンド（TLS接続）では使用できません。内部ネットワークでのgRPC通信など、TLSが不要な環境でのみ使用してください。

#### SNI (Server Name Indication) 設定

HTTPSバックエンドへの接続時、バックエンドがIPアドレス指定の場合でもSNIにドメイン名を指定できます。
これにより、仮想ホスト構成のサーバーでも正しい証明書を取得できます。

```toml
# IPアドレス指定 + SNI名指定
[[route]]
[route.conditions]
host = "example.com"
path = "/internal-api/*"
[route.action]
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
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
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
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "https-pool"
```

> **Note**: 文字列形式と構造体形式は同一配列内で混在可能です。従来の文字列形式は後方互換性のためそのまま動作します。

#### Unix ドメインソケット（UDS）バックエンド

上流バックエンドへ `AF_UNIX` で接続できます（Unix 系のみ）。表記は nginx の
`proxy_pass http://unix:/path/to.sock:/uri;` と同じです。

```
http://unix:<socket-path>[:<path-prefix>]     # 平文 HTTP / h2c
https://unix:<socket-path>[:<path-prefix>]    # UDS 上で TLS 終端
```

```toml
[upstreams."uds-pool"]
algorithm = "round_robin"
servers = [
  "http://unix:/run/app1.sock",                               # パスプレフィックスなし（"/"）
  { url = "http://unix:/run/app2.sock:/api", use_h2c = true }  # 上流パスの先頭に "/api" を前置
]

  [upstreams."uds-pool".health_check]
  enabled = true
  check_type = "tcp"
  interval_secs = 10
  timeout_secs = 5

# 単一バックエンド形式
[[route]]
[route.conditions]
path = "/app/*"
[route.action]
type = "Proxy"
url = "http://unix:/run/app1.sock"
```

注意事項・制約:

- パスプレフィックスの区切りは「**最後の** `:` の直後が `/` で始まる」場合のみ認識します。
  したがって **ソケットパスに `:` は使えません**。
- UDS には host / port が無いため、上流へ送る `Host` ヘッダは既定で `localhost` に
  なります。変更する場合は `[route.security]` の
  `add_request_headers = { Host = "..." }` を使ってください。`https://unix:...` の
  SNI は `sni_name`（未指定なら `localhost`）です。
- コネクションプール・ログ・メトリクス・Consistent Hash のノード ID は
  `unix:<socket-path>` で識別されます（TCP バックエンドの識別子は不変）。
- ヘルスチェック（`http` / `tcp` / `grpc`）も `unix:` 上流に対応します。ただし
  `timeout_secs` は UDS の connect には適用されません（read/write のみ）。
- `[[l4]].upstreams` は `unix:<socket-path>`（スキーム無し。`[[l4]].listen` と同じ表記）
  で TCP ストリームプロキシの上流を UDS にできます。L4 UDP 上流は非対応です。
- HTTP/3 を **上流** に使う場合は UDS 不可です（QUIC は UDP のため）。
  下流 HTTP/3 → 上流 UDS の中継は対応します。
- Windows では設定検証エラーになります。
- OpenBSD ではバックエンドのソケットパスが `unveil(2)` の許可リストへ自動追加され、
  `unix` pledge promise が要求されます。FreeBSD の capability mode（capsicum）は
  UDS を含め上流接続のある構成では使えません。

### WebSocket設定

WebSocketは通常のProxyで自動サポートされます。双方向転送時のポーリング動作を設定でカスタマイズ可能です。

#### 基本設定

```toml
# WebSocketアプリケーション
[[route]]
[route.conditions]
host = "example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

# ロードバランシング付きWebSocket
[[route]]
[route.conditions]
host = "example.com"
path = "/ws-lb/*"
[route.action]
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
[[route]]
[route.conditions]
host = "game.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3000"

[route.security]
  websocket_poll_mode = "fixed"
  websocket_poll_timeout_ms = 1

# チャットアプリ（バランス重視）
[[route]]
[route.conditions]
host = "chat.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3001"

[route.security]
  websocket_poll_mode = "adaptive"
  websocket_poll_timeout_ms = 1
  websocket_poll_max_timeout_ms = 50
  websocket_poll_backoff_multiplier = 2.0

# 監視ダッシュボード（CPU効率優先）
[[route]]
[route.conditions]
host = "monitor.example.com"
path = "/ws/*"
[route.action]
type = "Proxy"
url = "http://localhost:3002"

[route.security]
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
| `blocked_ips` | 最前線 IP/CIDR ブロックリスト。`accept` 直後（TLS ハンドシェイク・ハンドラ生成の前）にマッチした接続を切断し、既知の不正 IP への高コスト処理を回避する。CIDR は起動時に一度だけパースされ（accept ホットパスはゼロアロケーション判定）、SIGHUP でホットリロード可能。ルート単位の `denied_ips` より前段で評価される。 | `[]` |
| `allow_security_failures` | セキュリティ機能の有効化に失敗した場合の動作 | false |

#### セキュリティ機能失敗時の動作

`allow_security_failures` オプションで、セキュリティ機能（サンドボックス、seccomp、Landlock）の有効化に失敗した場合の動作を制御できます。

| 設定値 | 動作 | 用途 |
|--------|------|------|
| `false`（デフォルト） | 有効化に失敗した場合はサーバーの起動を失敗させる | **本番環境推奨** - セキュリティ機能が確実に有効化されることを保証 |
| `true` | 有効化に失敗しても警告を出して起動を続行する | 開発・デバッグ用 - セキュリティ機能が利用できない環境でも開発を継続可能 |

**デフォルト動作（`allow_security_failures = false`）:**

セキュリティ機能の有効化に失敗した場合、詳細なエラーメッセージを出力してサーバーの起動を中止します。これにより、本番環境でセキュリティ機能が無効化された状態で動作することを防止します。

```toml
[security]
# デフォルト: false（失敗時に起動を中止）
# allow_security_failures = false

enable_sandbox = true
enable_seccomp = true
enable_landlock = true
```

**開発・デバッグモード（`allow_security_failures = true`）:**

セキュリティ機能が利用できない環境（カーネルバージョン不足、権限不足など）でも、警告を出して起動を続行します。

```toml
[security]
# 開発環境でセキュリティ機能が利用できない場合のみ true に設定
allow_security_failures = true

enable_sandbox = true
enable_seccomp = true
enable_landlock = true
```

**注意事項:**

- **本番環境では `false`（デフォルト）を推奨**: セキュリティ機能が確実に有効化されることを保証
- **権限降格の失敗**: 権限降格の失敗は常に起動を中止します（`allow_security_failures` の設定に関係なく）
- **エラーメッセージ**: 失敗時には詳細なエラーメッセージと対処法が表示されます

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
| コネクションプール | `max_idle_connections_per_host` | ホストごとの最大アイドル接続数 | 256 |
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
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080/app/"

[route.security]
  allowed_methods = ["GET", "POST", "PUT"]
  max_request_body_size = 5_242_880  # 5MB
  backend_connect_timeout_secs = 5
  rate_limit_requests_per_min = 60

# IP制限付き管理API
[[route]]
[route.conditions]
host = "example.com"
path = "/admin/*"
[route.action]
type = "Proxy"
url = "http://localhost:9000/"

[route.security]
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
[[route]]
[route.conditions]
host = "example.com"
path = "/"
[route.action]
type = "Redirect"
redirect_url = "https://www.example.com/"
redirect_status = 301

# 旧URLから新URLへの移行（パス保持）
[[route]]
[route.conditions]
host = "example.com"
path = "/legacy/*"
[route.action]
type = "Redirect"
redirect_url = "https://example.com/v2"
redirect_status = 301
preserve_path = true
# /legacy/users → https://example.com/v2/users
# /legacy/api/data → https://example.com/v2/api/data

# HTTPからHTTPSへの強制リダイレクト（別のhostで設定）
[[route]]
[route.conditions]
host = "http.example.com"
path = "/"
[route.action]
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

値の展開はテンプレートを **1 回だけ走査**して行うため、展開後の値が再度プレースホルダとして
解釈されることはありません。クライアント IP（や Host、URI）がたまたま `$host` という文字列を
含んでいても、その文字列はそのまま出力されます。認識できないプレースホルダ（例: `$foo`）は
値の中にそのまま残ります。

### レスポンスヘッダー操作

クライアントへ返送する前にヘッダーを追加・削除します。静的ファイル配信時にも適用されます。

| オプション | 説明 | 例 |
|-----------|------|-----|
| `add_response_headers` | 追加するヘッダー | `{ "Strict-Transport-Security" = "max-age=31536000" }` |
| `remove_response_headers` | 削除するヘッダー | `["Server", "X-Powered-By"]` |

### 設定例

```toml
# セキュリティヘッダー付きプロキシ
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"

[route.security]
  # バックエンドに転送前に追加
  add_request_headers = { "X-Real-IP" = "$client_ip", "X-Forwarded-Proto" = "https" }
  # バックエンドに転送前に削除
  remove_request_headers = ["X-Debug-Token", "X-Internal-Auth"]
  # クライアントに返送前に追加（セキュリティヘッダー）
  add_response_headers = { "Strict-Transport-Security" = "max-age=31536000; includeSubDomains", "X-Frame-Options" = "DENY", "X-Content-Type-Options" = "nosniff" }
  # クライアントに返送前に削除
  remove_response_headers = ["X-Powered-By"]
```
