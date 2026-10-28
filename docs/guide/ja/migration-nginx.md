# nginx からの移行

[← ドキュメント目次](README.md) · [English](../migration-nginx.md)

nginx の設定を veil の `config.toml` に置き換えるためのガイドです。概念の対応、ディレクティブごとの対応（veil に無いものも
含む）、置き換えの例、運用の違いをまとめます。nginx 1.28 と veil 0.9 で確かめました。下の完全な設定例はすべて CI で
`veil -t` を通します（`tools/lint/check-guide-configs.py`）。*（試験済み）* の付いた例は、nginx と並べて同じ要求を送り、
応答を比べています（`tests/migration/`）。

- [概念](#概念)
- [ディレクティブ](#ディレクティブ)
- [例](#例)
- [運用](#運用)
- [性能の設定](#性能の設定)
- [移した後に](#移した後に)

## 概念

| nginx | veil | 補足 |
|---|---|---|
| `http { server { ... } }` | `config.toml` 全体 | 1 つのファイル。`[server]` が全ホストのリスナーを持つ。 |
| `listen 443 ssl; listen 80;` | `[server] listen`（TLS）と `listen_cleartext`（平文） | TLS のリスナーと平文のリスナーが 1 つずつ。バーチャルホストは `host` 条件の付いたルート。 |
| `server_name` | `[route.conditions] host` | 完全一致と `*.example.com`。正規表現は無い。 |
| `location` | `[[route]]` | **書いた順に**試し、最初に当たったものを使う（nginx は最長の前方一致・最初の正規表現）。具体的なルートを先に書く。 |
| `location /static/` | `path = "/static/*"` | 前方一致（`/static` にも当たる）。`*` の無い `path` は完全一致（`"/"` だけは全パスに当たる）。正規表現の location は無い。 |
| `root` / `alias` | `type = "File"`、`path = "/var/www/..."` | 常に **alias** の意味。当たった接頭辞を外し、残りを `path` に繋ぐ。 |
| `proxy_pass` | `type = "Proxy"`、`url` か `upstream` | 当たった接頭辞を外し、URL のパスに繋ぐ（[プロキシのパス](#プロキシのパス)）。 |
| `upstream { server ...; }` | `[upstreams."name"] servers = [...]` | 能動的なヘルスチェック・サーキットブレーカー・外れ値の検出もある。 |
| `return 301 ...;` | `type = "Redirect"` | `redirect_url`・`redirect_status`・`preserve_path`。 |
| `return 200 "ok";` | `type = "Respond"` | `status`・`body`・`content_type`。 |
| `include` | — | 1 つのファイル。生成するか、環境ごとの値は `-o`・`VEIL_OVERRIDE_<n>` で渡す（[running.md](running.md)）。 |
| OpenResty・njs のスクリプト | Proxy-Wasm のフィルタ | [wasm.md](wasm.md)。Lua のインタプリタのフィルタの例がある（`examples/wasm-filters/lua-filter`）。 |

### プロキシのパス

veil は、当たった接頭辞を外してから上流の URL のパスに繋ぎます。接頭辞を残すとき（nginx の URI 無しの
`proxy_pass http://backend;`）は、URL に接頭辞を書きます。

| nginx | veil |
|---|---|
| `location /api/ { proxy_pass http://app:3000; }` → `/api/users` は `/api/users` へ | `path = "/api/*"`、`url = "http://app:3000/api/"` |
| `location /api/ { proxy_pass http://app:3000/; }` → `/api/users` は `/users` へ | `path = "/api/*"`、`url = "http://app:3000/"` |

## ディレクティブ

記号: **=** 同じ意味、**≈** 近い（補足を参照）、**—** 無い（代わりの方法を書く）。

### 待ち受け・TLS・プロトコル

| nginx | veil | |
|---|---|---|
| `listen 443 ssl http2;` | `[server] listen = "0.0.0.0:443"`、`http2_enabled = true` | = ALPN で HTTP/1.1 と HTTP/2 |
| `listen 443 quic reuseport;` + `add_header Alt-Svc` | `[http3] enabled = true`、`listen = "0.0.0.0:443"` | = Alt-Svc は veil が付ける |
| `listen 80;` + `return 301 https://...` | `listen_cleartext = "0.0.0.0:80"`（既定で https へリダイレクト） | = `enable_secure_redirect = false` で平文のまま配る |
| 平文のポートでの `http2 on`（h2c） | `listen_cleartext`（HTTP/1.1 と h2c の prior knowledge） | ≈ nginx は同じポートで h2c の prior knowledge を受けない |
| `ssl_certificate` / `ssl_certificate_key` | `[tls] cert_path` / `key_path` | = 自動で読み直す（`auto_reload`） |
| `ssl_protocols TLSv1.2 TLSv1.3;` | — | ≈ rustls は TLS 1.2・1.3 だけ、新しい暗号だけ。下げられない |
| `ssl_ciphers` | — | ≈ 決まった安全な組（rustls） |
| `ssl_session_cache` / `ssl_session_tickets` | `[tls] session_resumption` | = 全ワーカーで共有（[tls.md](tls.md)） |
| `ssl_verify_client on; ssl_client_certificate` | クライアント証明書（[tls.md](tls.md)） | = |
| `ssl_stapling` | — | — 対応していない |
| `proxy_ssl_certificate`（上流の mTLS） | Proxy の action の `tls_client_cert` / `tls_client_key` / `tls_ca` | = |
| `proxy_ssl_server_name` / `proxy_ssl_name` | `sni_name` | = |
| kTLS（`ssl_conf_command Options KTLS`） | `[tls] ktls_enabled = true` | = Linux。[tls.md](tls.md) |
| ACME（nginx の外の certbot） | `[acme]` | ≈ 組み込み（[acme.md](acme.md)） |

### location・ファイル・リダイレクト

| nginx | veil | |
|---|---|---|
| `location = /x` | `path = "/x"` | = 完全一致 |
| `location ~ \.php$`（正規表現） | — | — 前方一致・完全一致で書くか、Proxy-Wasm のフィルタ |
| `root` / `alias` / `index` | `type = "File"`、`path`、`index` | = alias の意味 |
| `try_files $uri $uri/ /index.html;`（SPA） | — | — 無い。資産の接頭辞ごとのルートと、最後に `index.html` を返す `type = "File"` のルートを置くか、フィルタ |
| `autoindex on;` | — | — ディレクトリの一覧は作らない |
| `return 301 https://$host$request_uri;` | `type = "Redirect"`、`redirect_url = "https://$host$request_uri"` | = |
| `rewrite ^/old/(.*)$ /new/$1 permanent;` | `path = "/old/*"` で `type = "Redirect"`、`redirect_url = "/new"`、`preserve_path = true` | ≈ 接頭辞の書き換えだけ |
| `rewrite ... last;`（内部） | — | — 内部の書き換えは無い。Proxy・File の接頭辞の除去でよくある場合は足りる |
| `open_file_cache` | `[open_file_cache]` と `[static_file_cache]`（ルートごとにも書ける） | = （[performance.md](performance.md)） |
| `sendfile on;` | `type = "File"` の既定（`mode = "sendfile"`） | = |
| `error_page 404 /404.html;` | — | — 無い |

### プロキシ・上流

| nginx | veil | |
|---|---|---|
| `proxy_pass http://app;` | `url = "http://app"` | = [プロキシのパス](#プロキシのパス) |
| `proxy_set_header X-Real-IP $remote_addr;` | `[route.security] add_request_headers = { "X-Real-IP" = "$client_ip" }` | = 変数は `$client_ip`・`$host`・`$request_uri` |
| `proxy_set_header Host $host;` | — | = 既定でクライアントの Host を送る |
| `proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;` | 既定（`[route.security] forwarded_headers = "append"`。前段は `trusted_proxies`） | ≈ 信頼しないクライアントの値には足さずに捨てる |
| `proxy_hide_header` / `add_header` | `remove_response_headers` / `add_response_headers` | = 静的ファイルにも効く |
| `proxy_http_version 1.1;` + 上流への keepalive | 既定（接続をプールする） | = `max_idle_connections_per_host`・`idle_connection_timeout_secs` |
| 上流への HTTP/2 | `use_h2c = true`（平文）・`https://` は ALPN | = 多重化（[routing.md](routing.md#h2c-http2-over-cleartext-proxy)） |
| `grpc_pass` | h2c・h2 の上流への `type = "Proxy"` | = gRPC、多重化（[routing.md](routing.md)） |
| WebSocket（`Upgrade` ヘッダ） | 自動 | = |
| `proxy_connect_timeout` | `backend_connect_timeout_secs` | = |
| `proxy_read_timeout` / `proxy_send_timeout` | TOML のルートには無い | ≈ 全体の I/O のタイムアウト。ルートごとの要求のタイムアウトは予定（F-226） |
| `proxy_next_upstream` | 閉じていたプールの接続での冪等な要求の 1 回の再送 | ≈ 状態コードによる再試行はまだ無い（F-226） |
| `proxy_buffering on/off` | `[route.buffering] mode` | = `streaming`・`full`・`adaptive`（[compression-cache.md](compression-cache.md)） |
| `client_max_body_size` | `[route.security] max_request_body_size` | = |
| `client_header_timeout` / `client_body_timeout` | `client_header_timeout_secs` / `client_body_timeout_secs` | = |
| `upstream` のラウンドロビン・`weight=` | `algorithm = "round_robin"`・`"weighted"` + `weight` | = |
| `least_conn;` | `algorithm = "least_conn"` | = |
| `ip_hash;` / `hash $cookie_x consistent;` | `algorithm = "ip_hash"` / `"consistent_hash"` + `hash_key` | = |
| `max_fails=` / `fail_timeout=`（受動的） | `[upstreams.X.outlier_detection]` / `circuit_breaker` | ≈ 能動的な `health_check` もある（nginx は Plus のみ） |
| `split_clients`（カナリア） | `[route.traffic] split` | = |
| `proxy_cache` | `[route.cache]` | ≈ メモリとディスク、`stale_while_revalidate`、`stale_if_error`。HTTP/1.1 のクライアントと `http://` の上流だけ（ほかは F-257） |
| `proxy_cache_key` | `include_query`・`key_headers` | ≈ |
| UNIX ソケットの上流（`unix:/run/app.sock`） | `url = "http://unix:/run/app.sock"` | = |

### セキュリティ・流量の制御

| nginx | veil | |
|---|---|---|
| `allow` / `deny` | `[route.security] allowed_ips` / `denied_ips`、または `[route.conditions] source_ip` | = |
| `limit_req zone=... rate=60r/m;` | `rate_limit_requests_per_min = 60` | ≈ クライアントの IP とルートごと、スライディングウィンドウ、burst の設定は無い |
| `limit_conn` | `[security] max_concurrent_connections` | ≈ 全体で数える（キーごとではない） |
| `limit_except GET { deny all; }` | `allowed_methods = ["GET", "HEAD"]` | = |
| `auth_basic` | — | — 無い。`[route.jwt]` か Proxy-Wasm のフィルタ |
| `auth_request`（サブリクエスト） | — | — TOML のルートには無い。ext_authz は予定（F-229）。JWT の検証は組み込み（`[route.jwt]`） |
| `add_header Strict-Transport-Security ...` | `add_response_headers` | = |
| `server_tokens off;` | 既定（`Server` ヘッダを出さない） | = `server_header_enabled` で出す |
| CORS（`add_header Access-Control-*`） | `[route.traffic.cors]`（preflight と実際の応答） | = |
| `stream {}`（TCP・UDP） | `[[l4]]` | = TCP と UDP、ヘルスチェック付き（[l4-proxy.md](l4-proxy.md)） |
| プロセスのサンドボックス（nginx には無い） | seccomp・Landlock・特権の降格・capsicum・pledge/unveil | veil だけ（[security.md](security.md)） |

### 圧縮・ログ・状態

| nginx | veil | |
|---|---|---|
| `gzip on;` / ngx_brotli | `[route.compression] enabled = true` | = gzip・brotli・zstd（`preferred_encodings`） |
| `gzip_types` | `[route.compression] compressible_types` / `skip_types` | = |
| `access_log` + `log_format` | `[access_log]`（`format = "text"` か `"json"`、`fields`） | ≈ 決まった項目（メソッド・Host・パス・状態・時間・クライアントの IP・上流・大きさ・User-Agent） |
| `error_log` | `[logging]` | = |
| `stub_status` | `[prometheus]`（`/__metrics`） | ≈ ずっと細かい（[observability.md](observability.md)） |
| OpenTelemetry のモジュール | `[opentelemetry]` | = OTLP/HTTP |

## 例

### 静的なサイト *（試験済み）*

nginx:

```nginx
server {
    listen 8080;
    root /var/www;
    index index.html;
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
index = "index.html"
```

### 上流のグループとヘッダを使うリバースプロキシ *（試験済み）*

nginx:

```nginx
upstream app {
    least_conn;
    server 10.0.0.11:3000;
    server 10.0.0.12:3000;
}
server {
    listen 8080;
    location /api/ {
        proxy_pass http://app/api/;
        proxy_set_header X-Real-IP $remote_addr;
        add_header X-Frame-Options DENY;
    }
    location / {
        root /var/www;
    }
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[upstreams."app"]
algorithm = "least_conn"
servers = ["http://10.0.0.11:3000", "http://10.0.0.12:3000"]

  [upstreams."app".health_check]
  path = "/health"
  interval_secs = 10

[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "app"
[route.security]
add_request_headers = { "X-Real-IP" = "$client_ip" }
add_response_headers = { "X-Frame-Options" = "DENY" }

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
```

上流の URL にパスが無いので、`/api/users` は `http://10.0.0.11:3000/users` へ届きます。nginx のこの例と同じく `/api/` の
接頭辞を残すときは、サーバーを `"http://10.0.0.11:3000/api/"` と書きます。

### HTTP/2・HTTP/3 の HTTPS と、80 番からのリダイレクト

nginx:

```nginx
server {
    listen 80;
    return 301 https://$host$request_uri;
}
server {
    listen 443 ssl;
    listen 443 quic reuseport;
    http2 on;
    ssl_certificate     /etc/veil/ssl/cert.pem;
    ssl_certificate_key /etc/veil/ssl/key.pem;
    add_header Alt-Svc 'h3=":443"; ma=86400';
    location / { proxy_pass http://127.0.0.1:3000; }
}
```

veil:

```toml
[server]
listen = "0.0.0.0:443"
listen_cleartext = "0.0.0.0:80"   # redirects to https (enable_secure_redirect defaults to true)
http2_enabled = true

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"

[http3]
enabled = true
listen = "0.0.0.0:443"

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000"
```

### バーチャルホスト・リダイレクト・固定の応答 *（試験済み）*

nginx:

```nginx
server {
    listen 8080;
    server_name old.example.com;
    return 301 https://new.example.com$request_uri;
}
server {
    listen 8080;
    server_name api.example.com;
    location = /healthz { return 200 "ok"; }
    location / { proxy_pass http://127.0.0.1:3000; }
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
host = "old.example.com"
[route.action]
type = "Redirect"
redirect_url = "https://new.example.com$request_uri"
redirect_status = 301

[[route]]
[route.conditions]
host = "api.example.com"
path = "/healthz"
[route.action]
type = "Respond"
status = 200
body = "ok"
content_type = "text/plain"

[[route]]
[route.conditions]
host = "api.example.com"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000"
```

### 流量の制限・IP の許可リスト・メソッド

nginx:

```nginx
limit_req_zone $binary_remote_addr zone=api:10m rate=120r/m;
server {
    listen 8080;
    location /admin/ {
        allow 10.0.0.0/8;
        deny all;
        limit_except GET POST { deny all; }
        limit_req zone=api;
        proxy_pass http://127.0.0.1:3000/admin/;
    }
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
path = "/admin/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000/admin/"
[route.security]
allowed_ips = ["10.0.0.0/8"]
allowed_methods = ["GET", "POST"]
rate_limit_requests_per_min = 120
```

### キャッシュと圧縮

nginx:

```nginx
proxy_cache_path /var/cache/nginx keys_zone=c:10m max_size=1g;
server {
    listen 8080;
    gzip on;
    location / {
        proxy_cache c;
        proxy_cache_valid 200 5m;
        proxy_pass http://127.0.0.1:3000;
    }
}
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000"
[route.cache]
enabled = true
default_ttl_secs = 300
cacheable_statuses = [200]
[route.compression]
enabled = true
```

### TCP と UDP（`stream {}`）

nginx:

```nginx
stream {
    server { listen 5432; proxy_pass 10.0.0.20:5432; }
    server { listen 53 udp; proxy_pass 10.0.0.53:53; }
}
```

veil（全キーは [l4-proxy.md](l4-proxy.md)）:

```toml
# veil needs an HTTP listener even when it is used only for [[l4]] (here on loopback only)
[server]
listen_cleartext = "127.0.0.1:8080"
enable_secure_redirect = false

[[l4]]
name = "postgres"
listen = "0.0.0.0:5432"

  [[l4.upstreams]]
  addr = "10.0.0.20:5432"

[[l4]]
name = "dns"
listen = "0.0.0.0:53"
protocol = "udp"

  [[l4.upstreams]]
  addr = "10.0.0.53:53"
```

## 運用

| nginx | veil |
|---|---|
| `nginx -t` | `veil -t`（`-c FILE`） |
| `nginx -s reload` / `systemctl reload nginx` | `kill -HUP <pid>` / `systemctl reload veil`: 止めずに読み直す。リスナーはそのまま |
| `nginx -s quit` | `SIGTERM` / `SIGINT`: 処理中の要求を終えてから止める（`graceful_shutdown_timeout_secs`） |
| `-c /etc/nginx/nginx.conf` | `-c FILE`、`VEIL_CONFIG`、または既定のパス（[running.md](running.md#設定ファイル)） |
| 環境ごとの `include`・envsubst | `-o "key = value"` / `VEIL_OVERRIDE_<n>` |
| `/var/log/nginx` のログ | 既定は標準出力・journal。ファイルは `file_path` |

veil は起動・読み直しの前にファイル全体を検証します。不正なファイルで読み直すと、動いている設定のまま続けます。

## 性能の設定

| nginx | veil | |
|---|---|---|
| `worker_processes auto;` | `[server] threads = 0` | = CPU ごとに 1 ワーカー |
| `worker_cpu_affinity` | 既定 | = ワーカーを CPU に固定する |
| `reuseport` | 既定（`reuseport_balancing = "cbpf"`） | = |
| `worker_connections` / `worker_rlimit_nofile` | ハードリミットまで自動で上げる | = |
| `sendfile on; tcp_nopush on;` | 静的ファイルの既定 | = |
| `open_file_cache max=...` | `[static_file_cache]` + `[open_file_cache]` | = 静的配信で offload を無くすには両方が要る |
| `ssl_conf_command Options KTLS` | `[tls] ktls_enabled = true` | = |
| `http3_stream_buffer_size` など | `[http3] mmsg_batch_size` / `recv_drain_max` | ≈ HTTP/3 の主な調整項目（[performance.md](performance.md)） |

スループットを比べるときは、同じ条件で比べてください（同じ `open_file_cache`、kTLS は両方で有効か無効か）。
[docs/perf/README.md](../../perf/README.md) を参照してください。

## 移した後に

1. `veil -t -c config.toml` が通るまで直す。
2. nginx の横の別のポートで veil を起動し、実際の要求の一部を両方へ送る。状態コードと、クライアントが頼っているヘッダを比べる。
3. ログの集計が期待するアクセスログの形式（`format = "json"` と `fields`）を確かめる。
4. リスナー（またはロードバランサの宛先）を移し、`/__metrics` を見る（[Grafana のダッシュボード](observability.md)）。
