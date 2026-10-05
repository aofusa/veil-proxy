# メトリクス・トレーシング・ログ

[← ドキュメント目次](README.md) · [English](../observability.md)

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
| `veil_proxy_cache_evictions_total` | Counter | reason | キャッシュ削除総数 |
| `veil_proxy_cache_size_bytes` | Gauge | storage | 現在のキャッシュサイズ（バイト） |
| `veil_proxy_cache_entries` | Gauge | storage | 現在のキャッシュエントリ数 |
| `veil_proxy_buffering_used_total` | Counter | host | バッファリング使用リクエスト総数 |
| `veil_circuit_breaker_open_total` | Counter | upstream | CBオープンイベント数 |
| `veil_circuit_breaker_state` | Gauge | upstream | CB状態（0=Closed, 1=Open, 2=HalfOpen） |
| `veil_retry_total` | Counter | upstream, result | リトライ試行回数 |
| `veil_outlier_ejected` | Gauge | upstream, server | サーバー排除状態（1=排除中） |
| `veil_connection_pool_size` | Gauge | upstream | コネクションプールサイズ |
| `veil_connection_pool_hits_total` | Counter | upstream | コネクションプールヒット数 |
| `veil_connection_pool_misses_total` | Counter | upstream | コネクションプールミス数 |
| `veil_grpc_requests_total` | Counter | method, status_code, upstream | gRPCリクエスト数 |
| `veil_grpc_stream_duration_seconds` | Histogram | method | gRPCストリーム処理時間 |
| `veil_wasm_filter_duration_seconds` | Histogram | filter, phase | WASMフィルター実行時間 |
| `veil_wasm_fuel_consumed_total` | Counter | filter, phase | WASMフィルターの wasmtime fuel 累積消費量 |

### ランタイム有効/無効切り替え

再起動不要でメトリクスを動的にオン/オフできます。無効時は `/__metrics` が `404 Not Found` を返し、全記録関数がno-opになります。

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

## OpenTelemetry（OTLP/HTTP）

重い OpenTelemetry SDK（tokio 依存）を使わず、Prometheusメトリクスを OTLP 互換コレクタへプッシュ配信します。

> **必要なフィーチャー**: `--features opentelemetry`（または `--features full`）

### アーキテクチャ

- 専用の `std::thread` が設定間隔でメトリクスをエクスポート。
- 内部 Prometheus レジストリの値を OTLP/HTTP JSON（`POST /v1/metrics`）に変換。
- 制御メッセージ（Flush/Shutdown）は `std::sync::mpsc::channel` 経由 — tokio 不使用。

### 設定

```toml
[opentelemetry]
enabled = true
endpoint = "http://localhost:4318"   # OTLP/HTTP コレクタエンドポイント
service_name = "veil-proxy"          # service.name リソース属性
batch_interval_secs = 30             # エクスポート間隔（デフォルト: 30秒）
```

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | OTLPエクスポートを有効化 | `false` |
| `endpoint` | OTLP/HTTPエンドポイントURL | `http://localhost:4318` |
| `service_name` | `service.name` リソース属性 | `veil-proxy` |
| `batch_interval_secs` | エクスポート間隔（秒） | `30` |

### 対応コレクタ

| コレクタ | 備考 |
|---------|------|
| Grafana Alloy / Tempo | `http://alloy:4318` をエンドポイントに設定 |
| Jaeger (v1.35+) | OTLPレシーバーを有効化 |
| OpenTelemetry Collector | 標準の OTLP/HTTP レシーバー |
| Prometheus Remote Write | otel-collector の `prometheusremotewrite` エクスポーター経由 |

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
| `app_file_path` | アプリ本体ログ（INFO/WARN/DEBUG/TRACE）の出力先 | なし（**標準出力**） |
| `error_file_path` | エラーログ（ERROR）の出力先 | なし（**標準エラー出力**） |

### ログ出力先（系統別）

アプリ本体ログとエラーログは **レベルによって別々の出力先** に振り分けられ、各ログ行には識別用 `type` フィールド（`app` / `error` / `access`）が付与されます（混在出力でも判別可能）:

| 系統 | 対象レベル | 設定キー | デフォルト | `type` |
|------|-----------|----------|-----------|--------|
| アプリ本体ログ | INFO / WARN / DEBUG / TRACE | `app_file_path` | **標準出力 (stdout)** | `app` |
| エラーログ | ERROR | `error_file_path` | **標準エラー出力 (stderr)** | `error` |
| アクセスログ | （リクエストごと） | `[access_log].file_path` | **標準出力 (stdout)** | `access` |

いずれかの系統にファイルパスを指定した場合、その **親ディレクトリ** が `[security].landlock_write_paths` に自動追加され、Landlock 有効時にログ書き込みが拒否されるのを防ぎます（日次ローテーション生成ファイルも同ディレクトリ配下のため）。

### ログ出力形式

#### テキスト形式（デフォルト）

```
2024-01-01 00:00:00.000+00 0ms INFO type=app main [main.rs:123] Server started
```

#### JSON形式

構造化ログ収集システム（Elasticsearch、Loki等）との連携に適しています。

```json
{"timestamp":"2024-01-01T00:00:00.000Z","level":"INFO","type":"app","target":"veil","file":"main.rs","line":123,"message":"Server started"}
```

### 設定例

```toml
[logging]
level = "info"
format = "text"  # または "json"
channel_size = 100000
flush_interval_ms = 1000
# アプリ本体ログは既定で標準出力、エラーログは既定で標準エラー出力。
app_file_path = "/var/log/veil/veil.log"          # 任意
error_file_path = "/var/log/veil/veil.error.log"  # 任意
```

### JSON形式設定例

```toml
[logging]
level = "info"
format = "json"
file_path = "/var/log/veil.json"
```

## 構造化アクセスログ

リクエストごとのアクセスログをJSON/テキスト形式で出力します。アプリケーションログ（`[logging]`）とは独立したファイルに書き込めます。

**パフォーマンス設計：** ホットパス（ワーカースレッド）はスレッドローカルバッファでゼロアロケーション JSON/テキストを構築し、専用ログスレッドへ bounded チャネル経由で送信します。ファイル/stderrへのI/Oはログスレッドが独占するためグローバルロック競合が発生しません。

### 設定

```toml
[access_log]
enabled = true
format = "json"                              # "json" または "text"
file_path = "/var/log/veil/access.log"       # 省略時は標準出力 (stdout)。指定時は親ディレクトリを landlock_write_paths へ自動追加
# 出力フィールドを限定する（省略時は全フィールドを出力）
fields = ["timestamp", "method", "host", "path", "status", "duration_ms", "client_ip", "upstream"]
channel_size = 10000      # ログスレッドへの非同期チャネルキャパシティ（デフォルト: 10000）
flush_interval_ms = 1000  # BufWriter のフラッシュ間隔（ミリ秒、デフォルト: 1000）
```

ログはログスレッドが非同期で書き込みます。チャネルがフルの場合はリクエスト処理をブロックせずにログ行をサイレントドロップします。ホットリロード（SIGHUP）時にファイルパスやフォーマットが変わった場合、旧ログスレッドを終了して新ログスレッドを起動します。

### 利用可能なフィールド

| フィールド | 説明 |
|-----------|------|
| `timestamp` | リクエスト時刻（RFC 3339） |
| `method` | HTTPメソッド |
| `host` | リクエストの Host ヘッダ |
| `path` | リクエストパス |
| `status` | HTTPレスポンスステータスコード |
| `duration_ms` | リクエスト処理時間（ミリ秒） |
| `client_ip` | クライアントIPアドレス |
| `upstream` | アップストリームサーバーアドレス |
| `req_body_size` | リクエストボディサイズ（バイト） |
| `resp_body_size` | レスポンスボディサイズ（バイト） |
| `user_agent` | User-Agent ヘッダ |

### JSON出力例

```json
{"timestamp":"2026-01-01T00:00:00Z","type":"access","method":"GET","host":"example.com","path":"/api/data","status":200,"duration_ms":12,"client_ip":"10.0.0.1","upstream":"192.168.1.10:8080","req_body_size":0,"resp_body_size":1024,"user_agent":"curl/8.0"}
```
