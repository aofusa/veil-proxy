# F-256: Grafana のダッシュボードの例

**状態: 設計（feat/distribution-tickets、2026-10-23）**

## 目的

- veil の Prometheus のメトリクスを見る Grafana のダッシュボードを、例として配る。
  - 読み込むだけで、トラフィック・遅延・上流・キャッシュ・gRPC・WASM・VDS の状態が分かるようにする。
- `docker compose up` だけで、veil + Prometheus + Grafana + ダッシュボードが立ち上がり、すぐ見られる例を置く。
- メトリクスの名前・ラベルが変わったら、試験でダッシュボードの壊れに気づけるようにする。

## 今の状態

- メトリクスは `docs/guide/observability.md` の表のとおり。
  - `veil_proxy_http_*`、`veil_proxy_cache_*`、`veil_proxy_vds_*`
  - `veil_circuit_breaker_*`・`veil_retry_total`・`veil_outlier_ejected`・`veil_connection_pool_*`
  - `veil_grpc_*`、`veil_wasm_*`
- ラベルの値は固定の集合（B-159）。`host` は、ルートの `host` 条件か `other`。
- プロセスのメトリクス（CPU・RSS・fd）は出していない。
- ダッシュボード・Prometheus の例の設定は無い。`docs/backlog/features/dashboard.md`（F-11）は、veil の内蔵の UI の話で別。

## 設計

### ダッシュボード

- **置き場所:** `examples/grafana/veil-overview.json`。
  - Grafana 11 以降の schema。
  - データソースは変数 `${datasource}`（Prometheus）。
- **変数:** `job`、`instance`、`host`、`upstream`（`label_values` で作る）。
- **行とパネル**（PromQL は、メトリクスの表と突き合わせて書く）:
  - **Traffic:**
    - 要求の速さ（状態コードの類ごと・`host` ごと）
    - 5xx の割合
    - 要求・応答の大きさの分布
  - **Latency:** `veil_proxy_http_request_duration_seconds` の p50・p95・p99（`host` ごと）、heatmap。
  - **Connections:** `veil_proxy_http_active_connections`。
  - **Upstream:**
    - 健康状態（`veil_proxy_http_upstream_health`）
    - circuit breaker の状態と開いた回数、再試行（結果ごと）
    - 外された server（outlier）
    - 接続プールの大きさと当たりの割合
  - **Cache:** 当たりの割合、保存・追い出し（理由ごと）、大きさ・件数（保存先ごと）。
  - **gRPC:** 要求の速さ（`status_code` ごと）、ストリームの長さの分布。
  - **WASM:** フィルタの実行時間の p99（`filter`・`phase` ごと）、fuel の消費。
  - **VDS:** 接続の状態、最後の更新からの経過時間、適用・拒否の数、配信中の版。
- 使っていない機能のパネルは「No data」のままでよい。行は既定で畳み、Traffic・Latency・Upstream だけを開いておく。
- **プロセスの資源:** node_exporter・cAdvisor のメトリクスがあれば使う、任意の行にする。
  veil にプロセスのメトリクスを足すかは別のチケットで決める。
- **内容の検査:** `dashboard-linter`（grafana/dashboard-linter）を CI で回す。

### すぐ試せる例

- `examples/grafana/docker-compose.yml` を置く。
  - **veil:** 公開イメージ。`[prometheus]` を有効にした設定と、上流の代わりの小さな HTTP サーバ。
  - **Prometheus:** veil の `/__metrics` を集める。
  - **Grafana:** データソースとダッシュボードを provisioning で読む。
  - **負荷:** 小さな要求を送り続けるコンテナ。グラフがすぐ動くようにする。
- **安全の既定:**
  - すべてのポートを `127.0.0.1` に bind する。
  - Grafana の管理者のパスワードは、`.env` の例に書いて変えてもらう。匿名の閲覧は無効。
  - イメージは digest で固定する。
- 手順は `examples/grafana/README.md`（英語）に書く。日本語の説明は `docs/guide/ja/observability.md` に書く。

### 配る先

- Helm chart（F-254）の `grafanaDashboard.enabled` で、同じ JSON を `grafana_dashboard: "1"` のラベル付きの ConfigMap にする。
  Grafana の sidecar が読み込む。
- grafana.com のダッシュボードの一覧への登録は任意（人が行う）。

### 試験

- **CI（nightly）:** compose を立ち上げ、負荷を数分かけた後に次のことを確かめる。
  - ダッシュボードの全パネルの PromQL を Prometheus の API で問い合わせる。
    compose の設定で有効にした機能のパネルは、結果が空でない。
    - 有効にする機能: HTTP・上流・キャッシュ・circuit breaker・再試行。gRPC・WASM・VDS は、compose の設定に含める範囲で。
  - Grafana の API で、ダッシュボードが読み込まれ、データソースの変数が解決している。
- メトリクスの名前・ラベルを変える PR では、この試験が落ちる。
  - AGENTS.md の「外部契約（メトリクス名）を変えたら文書を直す」に、ダッシュボードも足す。

### 文書

- `docs/guide/observability.md`（英日）に「Grafana dashboard」の節を足す。読み込み方、compose の例、Helm での使い方。

## 受け入れ条件

- `docker compose up`（`examples/grafana/`）の後、`http://127.0.0.1:3000/` のダッシュボードで、Traffic・Latency・Upstream・Cache のパネルが動く。
- 上の試験が CI で通る。

## 依存・リスク

- 依存は無い。Helm の部分は F-254 と一緒に入れる。
- Grafana の schema の版の変化に追随する。CI の Grafana のイメージの版を上げるときに、試験で確かめる。

## 関連

- [B-159](../bugs/B-159-metrics-label-cardinality.md)（ラベルの値の制限）
- [F-254](F-254-helm-chart.md)
- `docs/guide/observability.md`
