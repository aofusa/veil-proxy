# ロードバランシング・ヘルスチェック・レジリエンス

[← ドキュメント目次](README.md) · [English](../load-balancing.md)

## ロードバランシング

複数のバックエンドサーバーへのリクエスト分散に対応しています。

### アルゴリズム

| アルゴリズム | 説明 | 用途 |
|-------------|------|------|
| `round_robin` | 順番に振り分け（デフォルト） | 汎用 |
| `least_conn` | 接続数が最小のサーバーを選択 | 長時間接続 |
| `ip_hash` | クライアントIPでハッシュ | セッション維持 |
| `weighted` | 重み付きラウンドロビン（`weight` に比例） | サーバースペックが異なる場合 |
| `consistent_hash` | 150-vnode コンシステントハッシュリング（xxh3） | キャッシュ局所性、スティッキールーティング |

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
[[route]]
[route.conditions]
host = "example.com"
path = "/api/*"
[route.action]
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

#### 重み付きラウンドロビン

サーバーの処理能力に応じてトラフィックを重み付きで分散します：

```toml
[upstreams."weighted-api"]
algorithm = "weighted"
servers = [
  { url = "http://api1:8080", weight = 3 },  # トラフィックの75%
  { url = "http://api2:8080", weight = 1 },  # トラフィックの25%
]
```

> **注意**: `weight = 0` は `weight = 1`（最小値）として扱われます。オフセットはサーバーグループ構築時に計算され、選択はロックフリー（atomic fetch_add + 二分探索）で行われます。

#### コンシステントハッシュ

同じ送信元のリクエストを同じバックエンドに転送します（スティッキールーティング）：

```toml
# クライアントIPでハッシュ（デフォルト）
[upstreams."ch-pool"]
algorithm = "consistent_hash"
servers = ["http://cache1:8080", "http://cache2:8080", "http://cache3:8080"]

# HTTPヘッダー値でハッシュ
[upstreams."ch-by-user"]
algorithm = "consistent_hash"
hash_key = "header:X-User-Id"
servers = ["http://shard1:8080", "http://shard2:8080"]

# Cookie値でハッシュ
[upstreams."ch-by-session"]
algorithm = "consistent_hash"
hash_key = "cookie:session_id"
servers = ["http://node1:8080", "http://node2:8080"]
```

> **注意**: サーバーあたり150個の仮想ノード（vnode）リングを使用（xxh3ハッシュ）。サーバーが unhealthy になると除外され、リング上の次のノードが引き継ぎます。

### 単一バックエンドとの互換性

従来の `url` 指定も引き続き使用可能です：

```toml
# 従来の単一バックエンド指定
[[route]]
[route.conditions]
host = "example.com"
path = "/simple/*"
[route.action]
type = "Proxy"
url = "http://localhost:8080"
```

## ヘルスチェック（Health Check）

バックエンドサーバーの健康状態を監視し、異常なサーバーを自動的に除外します。

### 動作

1. バックグラウンドスレッドで定期的にチェックを実行
2. `check_type` に応じてHTTP/TCP/gRPCのいずれかのプロトコルでチェック
3. 連続失敗回数が閾値に達したらサーバーを除外
4. 連続成功回数が閾値に達したらサーバーを復帰

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `check_type` | チェックプロトコル: `http`、`tcp`、`grpc` | `http` |
| `interval_secs` | チェック間隔（秒） | 10 |
| `path` | チェック対象パス（HTTP: リクエストパス、gRPC: サービス名） | `/` |
| `timeout_secs` | タイムアウト（秒） | 5 |
| `healthy_statuses` | 成功と判断するステータスコード（HTTPのみ有効） | [200, 201, 202, 204, 301, 302, 304] |
| `unhealthy_threshold` | unhealthyにする連続失敗回数 | 3 |
| `healthy_threshold` | healthyに戻す連続成功回数 | 2 |
| `use_tls` | TLS接続を使用したヘルスチェック | **false** |
| `verify_cert` | TLS証明書の検証（use_tls=true時のみ有効） | **true** |

### HTTPヘルスチェック（デフォルト）

HTTPリクエストを送信してレスポンスのステータスコードを検証します。

```toml
  [upstreams."api-servers".health_check]
  check_type = "http"      # デフォルトのため省略可
  interval_secs = 10
  path = "/health"
  timeout_secs = 5
  healthy_statuses = [200]
  unhealthy_threshold = 3
  healthy_threshold = 2
```

### TCPヘルスチェック

TCP接続の確立可否のみを確認します。HTTP通信は行いません。データベースやメッセージブローカーなど、非HTTPバックエンドに適しています。

```toml
  [upstreams."db-servers".health_check]
  check_type = "tcp"
  interval_secs = 15
  timeout_secs = 3
  unhealthy_threshold = 2
  healthy_threshold = 1
```

### gRPCヘルスチェック

[gRPC Health Checking Protocol](https://github.com/grpc/grpc/blob/master/doc/health-checking.md) に基づき、`Content-Type: application/grpc` の HTTP/1.1 POST でチェックを行います。`grpc-status: 0`（OK）を受信した場合に healthy と判定します。

> **注意**: この実装は HTTP/1.1 フレームを使用します。HTTP/1.1 で gRPC ヘルスエンドポイントを公開しているバックエンド（gRPC-Web 互換）に対して動作します。

```toml
  [upstreams."grpc-servers".health_check]
  check_type = "grpc"
  interval_secs = 10
  path = "grpc.health.v1.Health"  # サービス名（空文字 = サーバー全体チェック）
  timeout_secs = 5
  unhealthy_threshold = 3
  healthy_threshold = 2
```

### TLSヘルスチェック

`use_tls = true` を設定すると、TLS接続を使用します。`http` と `grpc` の両チェック種別に適用できます。

```toml
  [upstreams."api-servers".health_check]
  check_type = "http"
  path = "/health"
  use_tls = true
  verify_cert = true   # 自己署名証明書の場合は false に設定
```

> **Note**: `verify_cert = false` に設定すると自己署名証明書が許可されます。本番環境では推奨されません。

### ログ出力

健康状態の変化はログに出力されます：

```
[INFO] Upstream api1.internal:8080 is now unhealthy
[INFO] Upstream api1.internal:8080 is now healthy
```

## サーキットブレーカー＆レジリエンス

サーバー単位のサーキットブレーカーとOutlier Detectionでカスケード障害を防止します。

### サーキットブレーカーの状態遷移

```
Closed ──(failure_threshold超過)──▶ Open
  ▲                                      │
  │                             (open_duration_secs秒後)
  │                                      │
  └──(success_threshold回成功)── HalfOpen ◀─(プローブ失敗)──┐
                                      │                      │
                                      └──(プローブ成功)───────┘
```

- **Closed**: 通常動作。失敗はスライディングウィンドウで追跡。
- **Open**: 全リクエストを即時拒否（ファストフェイル）。`open_duration_secs` 後にHalfOpenへ。
- **HalfOpen**: 限定数のプローブリクエストを通過させる。成功が `success_threshold` に達したらClosed、失敗したらOpen。

プール内の**全サーバー**のCBがOpenになった場合は、完全停止を避けるために healthy なサーバーへのフォールバックが動作します。

### 設定

```toml
[upstreams."api-pool"]
algorithm = "round_robin"
servers = ["http://api1:8080", "http://api2:8080"]

  [upstreams."api-pool".circuit_breaker]
  enabled = true
  failure_threshold = 5       # この失敗回数でOpen
  failure_window_secs = 60    # 失敗カウントのスライディングウィンドウ
  open_duration_secs = 30     # Open維持時間（経過後HalfOpen）
  half_open_probes = 3        # HalfOpenで許可するプローブリクエスト数
  success_threshold = 2       # HalfOpenでClosedに戻るための成功回数
  trip_on_timeout = true      # タイムアウトを失敗としてカウント
```

### 設定オプション

| オプション | 説明 | デフォルト |
|-----------|------|-----------|
| `enabled` | サーキットブレーカーを有効化 | `false` |
| `failure_threshold` | Openに移行する失敗回数 | `5` |
| `failure_window_secs` | スライディングウィンドウの時間 | `60` |
| `open_duration_secs` | Open状態の維持時間（秒） | `30` |
| `half_open_probes` | HalfOpenで通過させるリクエスト数 | `3` |
| `success_threshold` | HalfOpenでClosedに戻る成功回数 | `2` |
| `trip_on_timeout` | タイムアウトを失敗としてカウント | `true` |

### Outlier Detection（パッシブ排除）

サーキットブレーカーに加え、エラー率に基づいてサーバーを受動的に排除できます：

```toml
  [upstreams."api-pool".outlier_detection]
  enabled = true
  error_rate_threshold = 0.5    # エラー率が50%を超えたら排除
  interval_secs = 10            # 評価間隔（秒）
  base_ejection_time_secs = 30  # 基本排除時間（秒）
  max_ejection_percent = 50     # 最大排除割合（50%まで）
```

### Prometheusメトリクス（サーキットブレーカー）

| メトリクス | タイプ | 説明 |
|-----------|--------|------|
| `veil_circuit_breaker_open_total` | Counter | CB Openイベント数（upstreamラベル） |
| `veil_circuit_breaker_state` | Gauge | CB状態（0=Closed, 1=Open, 2=HalfOpen、upstreamラベル） |
| `veil_retry_total` | Counter | リトライ試行回数（upstream, resultラベル） |
| `veil_outlier_ejected` | Gauge | サーバー排除状態（1=排除中、upstream, serverラベル） |
