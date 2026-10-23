# Envoy からの移行

[← ドキュメント目次](README.md) · [English](../migration-envoy.md)

Envoy を veil に置き換える方法は 2 つあります。

1. **プロキシそのものとして置き換える。** 静的な `envoy.yaml`（エッジのプロキシ・API ゲートウェイ）や自前の xDS の
   制御プレーンで動かしている Envoy を、veil と `config.toml`、必要なら制御プレーンからルートと上流を受け取る
   [VDS](vds.md) に置き換えます。このガイドはこちらを扱います。
2. **サービスメッシュのデータプレーンとして置き換える。** Istio・Linkerd の下では、veil が Envoy の xDS（Delta ADS・SDS）と
   Linkerd のポリシーを直接話すので、設定の置き換えは要りません。プロキシのイメージを替えます。[mesh.md](mesh.md) を参照してください。

Envoy 1.36 と veil 0.9 で確かめました。下の完全な設定例はすべて CI で `veil -t` を通します
（`tools/lint/check-guide-configs.py`）。*（試験済み）* の付いた例は、Envoy と並べて同じ要求を送り、応答を比べています
（`tests/migration/`）。

## 概念

| Envoy | veil | 補足 |
|---|---|---|
| `static_resources.listeners[]` | `[server] listen`（TLS）・`listen_cleartext`（平文）、`[http3]`、`[[l4]]` | HTTP のリスナーは TLS と平文が 1 つずつ。他のポートは `[[l4]]` の TCP・UDP。 |
| `filter_chains[].transport_socket`（`DownstreamTlsContext`） | `[tls]` | 証明書は 1 組（TOML のリスナーでは、複数の証明書を SNI で選べない）。 |
| `http_connection_manager` | 組み込み | 上のリスナーで HTTP/1.1・HTTP/2・h2c・HTTP/3。 |
| `route_config.virtual_hosts[].domains` | `[route.conditions] host` | 完全一致と `*.example.com`。 |
| `routes[].match` | `[route.conditions]` | `prefix` → `path = "/x/*"`、`path` → 完全一致の `path`、`headers` → `header`、`query_parameters` → `query`。`safe_regex` は無い。 |
| `routes[].route.cluster` | `[route.action] type = "Proxy"`、`upstream = "<cluster>"` | |
| `clusters[]` | `[upstreams."<name>"]` | `lb_policy`、ヘルスチェック、外れ値の検出、サーキットブレーカー。 |
| `load_assignment.endpoints` | `servers = [...]` | 重み付きの URL。 |
| HTTP のフィルタ（`envoy.filters.http.*`） | ルートの設定と Proxy-Wasm のモジュール | 下の表。 |
| `envoy.filters.http.wasm` | `[wasm]` + ルートの `modules` | Proxy-Wasm ABI 0.2.1。Envoy 向けの Rust・C++・TinyGo のフィルタがそのまま動く（[wasm.md](wasm.md)）。 |
| xDS（LDS・RDS・CDS・EDS） | [VDS](vds.md) | ルートと上流だけ、State-of-the-World。リスナーと TLS の変更は再起動が要る。 |
| `admin` | `[admin]`、`[prometheus]` | 読み直しの状態、`/__metrics` のメトリクス。 |
| hot restart | `SIGHUP` | 同じプロセスのまま止めずに読み直す。リスナーはそのまま。 |

ルートは**書いた順に**試します（Envoy の virtual host の中のルートと同じ）。具体的なルートを先に書いてください。veil には
virtual host の階層が無いので、各ルートに `host` 条件を書きます。

## ルートの一致と action

| Envoy | veil | |
|---|---|---|
| `match: { prefix: "/api/" }` | `path = "/api/*"` | = |
| `match: { path: "/healthz" }` | `path = "/healthz"` | ≈ File のルートでは完全一致。他の action では `/healthz/...` にも当たる |
| `match: { safe_regex: ... }` | — | — 前方一致で書くか、Proxy-Wasm のフィルタ |
| `headers: [{ name, string_match }]` | `header = { "X-Version" = "v2" }` | ≈ 完全一致の値 |
| `query_parameters` | `query = { "token" = "..." }` | ≈ 完全一致の値 |
| 送信元の IP（`rbac`・`remote_ip` の matcher） | `source_ip = ["10.0.0.0/8"]` か `[route.security] allowed_ips` | = |
| `route: { cluster }` | `type = "Proxy"`、`upstream` | = |
| `prefix_rewrite: "/"` | 当たった接頭辞は常に外す。足すときは上流の URL にパスを書く | ≈（[routing.md](routing.md)） |
| `host_rewrite_literal` | `add_request_headers = { "Host" = "..." }` | ≈ |
| `weighted_clusters` | `[route.traffic] split` | = |
| `direct_response` | `type = "Respond"` | = |
| `redirect`（`https_redirect`・`path_redirect`） | `type = "Redirect"` / `listen_cleartext` と `enable_secure_redirect` | = |
| `timeout`・`idle_timeout` | TOML のルートには無い | ≈ 全体の I/O のタイムアウト。ルートごとは予定（F-226。メッシュのデータプレーンにはある） |
| `retry_policy` | 閉じていたプールの接続での冪等な要求の 1 回の再送 | ≈ 条件付きの再試行は予定（F-226） |
| `request_mirror_policies` | — | — 予定（F-226） |
| `request_headers_to_add` / `response_headers_to_add` / `*_to_remove` | `add_request_headers` / `add_response_headers` / `remove_*` | = |

## HTTP のフィルタ

| Envoy のフィルタ | veil | |
|---|---|---|
| `router` | 組み込み | = |
| `cors` | `[route.traffic.cors]` | = preflight に答える。応答の `Access-Control-Allow-Origin` は `add_response_headers` で足す |
| `fault`（abort） | `[route.traffic.fault]` | = |
| `fault`（delay） | — | — 予定（F-226） |
| `local_ratelimit` | `rate_limit_requests_per_min` | ≈ クライアントの IP とルートごと |
| `ratelimit`（全体のサービス） | — | — 無い |
| `jwt_authn` | `[route.jwt]` | = 手元の JWKS のファイルかインライン。リモートの JWKS の URI は取りに行かない |
| `rbac` | `allowed_ips` / `denied_ips` / `allowed_methods`、`[tls] client_allowed_sans` | ≈ |
| `ext_authz` | — | — 予定（F-229） |
| `compressor`（gzip・brotli・zstd） | `[route.compression]` | = |
| `cache` | `[route.cache]` | = |
| `buffer` | `[route.buffering] mode = "full"` | = |
| `grpc_web` | プロキシのルートには無い | — API ライブラリは gRPC-Web を自分で受ける（[library.md](library.md)）。プロキシのルートでは上流で変換する |
| `wasm` | Proxy-Wasm の `modules` | = |
| `lua` | Proxy-Wasm（Lua のフィルタの例がある） | ≈ |
| `health_check`（素通し・固定の応答） | ヘルスチェックのパスに `type = "Respond"` | ≈ |

## クラスター

| Envoy | veil | |
|---|---|---|
| `lb_policy: ROUND_ROBIN` / `LEAST_REQUEST` / `RING_HASH` / `MAGLEV` / `RANDOM` | `algorithm = "round_robin"` / `"least_conn"` / `"consistent_hash"` / `"consistent_hash"` / — | ≈ |
| エンドポイントの `load_balancing_weight` | `algorithm = "weighted"` と `{ url = "...", weight = 3 }` | = |
| `health_checks`（HTTP・TCP・gRPC） | `[upstreams.X.health_check] check_type = "http"` / `"tcp"` / `"grpc"` | = |
| `outlier_detection` | `[upstreams.X.outlier_detection]` | = エラー率で外す |
| `circuit_breakers`（接続数・要求数の上限） | `[upstreams.X.circuit_breaker]` | ≈ **意味が違う**: veil のブレーカーは失敗で開く（Envoy は同時数を制限する）。接続数は `max_idle_connections_per_host` と `[security] max_concurrent_connections` で抑える |
| `transport_socket`（`UpstreamTlsContext`） | `https://` の URL、`sni_name`、`tls_client_cert` / `tls_client_key` / `tls_ca` | = |
| `http2_protocol_options` | `use_h2c = true` / ALPN | = 多重化 |
| `STRICT_DNS` / `LOGICAL_DNS` | `servers` のホスト名（データプレーンの外で引いてキャッシュする） | ≈ |
| `EDS` | VDS の `Upstream` のリソース | ≈ 更新のたびにサーバーの一覧全体 |

## リスナー

| Envoy | veil | |
|---|---|---|
| `tcp_proxy` | `[[l4]]`（`protocol = "tcp"`） | = ヘルスチェック・least-connections 付き |
| `udp_proxy` | `[[l4]]`（`protocol = "udp"`） | = |
| SNI による TLS の素通し（`tls_inspector` + `server_names`） | `[[l4]] tls = "passthrough"` | ≈（[l4-proxy.md](l4-proxy.md)） |
| `proxy_protocol` のリスナーのフィルタ | — | — 無い |
| `access_log` | `[access_log]` | ≈ テキストか JSON、決まった項目 |
| `stats_sinks`・Prometheus の統計 | `[prometheus]`（`/__metrics`）、`[opentelemetry]` | ≈ メトリクスの名前が違う（[observability.md](observability.md)） |
| `tracing` | `[opentelemetry]` | ≈ |

## 例

### クラスターと固定の応答のあるエッジのプロキシ *（試験済み）*

Envoy:

<!-- not-a-full-config -->
```yaml
static_resources:
  listeners:
    - name: http
      address: { socket_address: { address: 0.0.0.0, port_value: 8080 } }
      filter_chains:
        - filters:
            - name: envoy.filters.network.http_connection_manager
              typed_config:
                "@type": type.googleapis.com/envoy.extensions.filters.network.http_connection_manager.v3.HttpConnectionManager
                stat_prefix: http
                route_config:
                  virtual_hosts:
                    - name: app
                      domains: ["*"]
                      routes:
                        - match: { path: "/healthz" }
                          direct_response: { status: 200, body: { inline_string: "ok" } }
                        - match: { prefix: "/" }
                          route: { cluster: app }
                http_filters:
                  - name: envoy.filters.http.router
                    typed_config: { "@type": type.googleapis.com/envoy.extensions.filters.http.router.v3.Router }
  clusters:
    - name: app
      type: STRICT_DNS
      lb_policy: LEAST_REQUEST
      load_assignment:
        cluster_name: app
        endpoints:
          - lb_endpoints:
              - endpoint: { address: { socket_address: { address: app1, port_value: 3000 } } }
              - endpoint: { address: { socket_address: { address: app2, port_value: 3000 } } }
      health_checks:
        - timeout: 2s
          interval: 10s
          unhealthy_threshold: 3
          healthy_threshold: 2
          http_health_check: { path: "/health" }
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[upstreams."app"]
algorithm = "least_conn"
servers = ["http://app1:3000", "http://app2:3000"]

  [upstreams."app".health_check]
  path = "/health"
  interval_secs = 10
  timeout_secs = 2
  unhealthy_threshold = 3
  healthy_threshold = 2

[[route]]
[route.conditions]
path = "/healthz"
[route.action]
type = "Respond"
status = 200
body = "ok"

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "Proxy"
upstream = "app"
```

### 重み付きのクラスターによるカナリア、CORS、障害の注入

Envoy（ルートの部分）:

<!-- not-a-full-config -->
```yaml
routes:
  - match: { prefix: "/api/" }
    route:
      weighted_clusters:
        clusters:
          - { name: stable, weight: 90 }
          - { name: canary, weight: 10 }
    typed_per_filter_config:
      envoy.filters.http.cors:
        "@type": type.googleapis.com/envoy.extensions.filters.http.cors.v3.CorsPolicy
        allow_origin_string_match: [{ exact: "https://app.example.com" }]
        allow_methods: "GET,POST"
      envoy.filters.http.fault:
        "@type": type.googleapis.com/envoy.extensions.filters.http.fault.v3.HTTPFault
        abort: { http_status: 503, percentage: { numerator: 1, denominator: HUNDRED } }
```

veil:

```toml
[server]
listen_cleartext = "0.0.0.0:8080"
enable_secure_redirect = false

[upstreams."stable"]
servers = ["http://stable:3000"]

[upstreams."canary"]
servers = ["http://canary:3000"]

[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
upstream = "stable"
[route.traffic]
split = [
  { upstream = "stable", weight = 90 },
  { upstream = "canary", weight = 10 },
]
[route.traffic.cors]
allow_origins = ["https://app.example.com"]
allow_methods = ["GET", "POST"]
[route.traffic.fault]
abort_status = 503
abort_percent = 1.0
[route.security]
add_response_headers = { "Access-Control-Allow-Origin" = "https://app.example.com", "Vary" = "Origin" }
```

### TLS・HTTP/3・JWT

Envoy: `DownstreamTlsContext` の付いたリスナー、QUIC のリスナーの `http3_protocol_options`、手元の JWKS を使う
`jwt_authn` のフィルタ。veil:

```toml
[server]
listen = "0.0.0.0:443"
listen_cleartext = "0.0.0.0:80"
http2_enabled = true

[tls]
cert_path = "/etc/veil/ssl/cert.pem"
key_path = "/etc/veil/ssl/key.pem"

[http3]
enabled = true
listen = "0.0.0.0:443"

[[route]]
[route.conditions]
path = "/api/*"
[route.action]
type = "Proxy"
url = "http://127.0.0.1:3000/"
[route.jwt]
issuer = "https://auth.example.com"
audiences = ["api"]
jwks_path = "/etc/veil/jwks.json"
```

### TCP のプロキシ（`tcp_proxy`）

```toml
# veil needs an HTTP listener even when it is used only for [[l4]] (here on loopback only)
[server]
listen_cleartext = "127.0.0.1:8080"
enable_secure_redirect = false

[[l4]]
name = "postgres"
listen = "0.0.0.0:5432"
lb = "least_conn"

  [[l4.upstreams]]
  addr = "10.0.0.20:5432"

  [[l4.upstreams]]
  addr = "10.0.0.21:5432"
```

## xDS の制御プレーンから VDS へ

VDS は xDS の意味（version・nonce・ACK/NACK、State-of-the-World）を保ったまま、gRPC のストリームか REST の long polling で
2 種類のリソースを送ります。`Route`（JSON にした `[[route]]` の 1 項目）と `Upstream`（`[upstreams.<name>]` の表）です。
制御プレーンを移すときは:

1. `RouteConfiguration`・`Cluster`・`ClusterLoadAssignment` の代わりに veil の `Route`・`Upstream` を作る
   （JSON の形は TOML と同じ。[vds.md](vds.md#resource-types)）。
2. リスナー・TLS・`[[l4]]` は `config.toml` に残す（VDS では変わらない）。
3. 参照の実装（Rust の `tools/vds/server`、Python の `tools/vds/vds_server.py`）から始める。

Delta xDS、リソースの種類をまたぐ ADS、on-demand（VHDS）は VDS にはありません。Istio・Linkerd の下では、Delta ADS を
そのまま話すメッシュのデータプレーンを使ってください（[mesh.md](mesh.md)）。

## 運用

| Envoy | veil |
|---|---|
| `envoy --mode validate -c envoy.yaml` | `veil -t -c config.toml` |
| hot restart・xDS の更新 | `SIGHUP`（ファイル全体）・VDS（ルートと上流） |
| `/ready`・`/stats`・`/config_dump`（admin） | `/__metrics`、`[admin]`（読み直し・状態） |
| drain（`/drain_listeners`・`--drain-time-s`） | `SIGTERM`: 処理中の要求を終えてから止める（`graceful_shutdown_timeout_secs`） |
| `--concurrency` | `[server] threads`（0 = CPU ごとに 1 つ） |

## 移した後に

1. `veil -t -c config.toml` が通るまで直す。
2. Envoy の横で veil を動かし、実際の要求の一部を両方へ送り、状態コードと、クライアントが頼っているヘッダを比べる。
3. ダッシュボードとアラートを veil のメトリクスの名前に合わせる（[observability.md](observability.md)、Grafana のダッシュボード）。
4. トラフィックを移す。
