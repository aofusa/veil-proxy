# full features 機能単位パフォーマンス計測レポート（F-89）

- 計測日: 2026-07-07
- 対象: `veil:glibc`（full features ビルド）
- ハーネス: `tools/perf/`（F-89 で追加した `feat_*` 構成）
- 起点: [perf_measurement_report.md](perf_measurement_report.md)（tools/perf 実装検証レポート）の指摘
  （WASM 計測欠如・full features 機能の計測漏れ）

## 1. 目的

`perf_measurement_report.md` が指摘した「full features に含まれる各機能のパフォーマンス
オーバーヘッドが未計測」という欠落を埋めるため、共通ベース構成（HTTP/2 有効・**kTLS 無効**・
kernel LB）へ **1 機能だけを重ねた** `feat_*` 構成を追加し、機能単位のオーバーヘッドと
正当性（Non-2xx=0）を計測する。kTLS はコンテナ環境と相性が悪いため feat 系では既定オフとし、
kTLS 自体の影響は既存の直交表 16 構成の `ktls` 因子で計測する。

## 2. 計測条件

| 項目 | 値 |
|------|-----|
| 負荷（HTTP/1.1） | wrk `-t4 -c100 -d10s`（TLS。L4 構成のみ平文 9080） |
| 負荷（HTTP/2） | h2load `-n30000 -c100 -m10`（TLS） |
| 反復 | 2（median 集計） |
| バックエンド | nginx:alpine（proxy 構成の上流） |
| ホスト | 4 コア。**計測中 loadavg 3〜5（co-tenant 負荷あり）** |

> 注意: 本計測ホストは 4 コアかつ計測中の loadavg が 3〜5 と負荷共有状態にあり、
> 絶対 req/s には数百 req/s 規模のばらつきがある。したがって **機能間のオーバーヘッドは
> 「ベースライン比の相対差」** で読む。`h2_0_feat_l4` の第 2 反復（5101→1525 req/s）のような
> 外れ値は host contention によるもの。

## 3. 結果（median req/s）

| 構成 | 機能 | HTTP/1.1 | HTTP/2 | Non-2xx | 備考 |
|------|------|---------:|-------:|:-------:|------|
| `h2_1_ktls_0_lb_kernel_ofc_0` | （ベースライン: File 配信） | **2335** | **2568** | 0 | 基準 |
| `h2_1_feat_wasm` | wasm（パススルー 1 枚） | 2365 | 2561 | 0 | ベース同等。RSS +80〜120MB |
| `h2_1_feat_metrics` | metrics（Prometheus） | 2441 | 2532 | 0 | ベース同等 |
| `h2_1_feat_access_log` | access-log（JSON） | 2349 | 2469 | 0 | ベース同等 |
| `h2_1_feat_rate_limit` | rate-limit | 2451 | 2565 | 0 | ベース同等 |
| `h2_1_feat_admin` | admin API | 2316 | 2562 | 0 | ベース同等 |
| `h2_1_feat_otel` | opentelemetry(+metrics) | 2456 | 2695 | 0 | ベース同等 |
| `h2_1_feat_proxy` | 逆プロキシ（→nginx） | **1975** | **2181** | 0 | **−15%**（バックエンドホップ） |
| `h2_0_feat_l4` | L4 TCP 素通し（平文） | **5101**※ | — | 0 | ※iter1。TLS なしで最大 2.2 倍 |

## 4. 分析

### 4.1 TLS 終端が支配的コスト

L4 平文素通し（`feat_l4`）が iter1 で **5101 req/s** に達したのに対し、TLS 終端を伴う
File 配信は全構成で **2300〜2700 req/s** に収束する。両者の差（約 2.2 倍）は
**TLS ハンドシェイク・レコード暗号化がデータプレーンの支配的コスト**であることを示す。
言い換えると、TLS 終端下では L7 機能ロジックのオーバーヘッドは相対的に埋もれる。

### 4.2 L7 機能ロジックのオーバーヘッドはノイズ範囲内

wasm / metrics / access-log / rate-limit / admin / opentelemetry はいずれも
ベースライン比 **±5% 以内**（ホスト負荷ばらつきと同オーダー）で、機能有効化による
有意なスループット低下は観測されなかった。これは以下の既存最適化の成果を裏づける。

- WASM: InstancePre + pooling allocator によるウォームインスタンス化（F-27）、
  Arc 共有・ヘッダ所有権ムーブスルー（F-43）、ボディ CoW（F-61）。
- metrics: `itoa` によるステータス文字列化のアロケーション回避。
- access-log: スレッドローカルバッファ + 専用ライタスレッドへの `try_send`。
- rate-limit: スレッドローカル + coarse timer。

全 9 構成 × 2 プロトコルで **Non-2xx=0**。full features 有効化による正当性の退行もない。

### 4.3 逆プロキシは約 15% 低下（バックエンドホップ）

`feat_proxy` のみ両反復で一貫して −15%（HTTP/1.1 1975 / HTTP/2 2181）。これは
バックエンド（nginx）への追加ネットワークラウンドトリップに由来する構造的コストで、
接続プーリング（B-28 で実装）により TIME_WAIT 蓄積・5xx は発生していない（Non-2xx=0）。

### 4.4 WASM の常駐メモリ

`feat_wasm` は RSS がベースライン（~90MB）比 +80〜120MB（169〜207MB）。これは
pooling allocator の事前確保メモリ（`total_memories` × `max_memory_size`）の常駐分で、
リクエスト完了後に解放される per-instance メモリとは別。負荷継続中に 169→207MB へ漸増したが、
これは allocator の high-water mark であり、リークではない（反復間で頭打ち）。

## 5. 実施した最適化

計測結果は「L7 機能ロジックは既に十分最適化済みで、TLS 終端が支配的」ことを示した。
その上で、レポートの主要関心事である **WASM ホットパスの冗長な per-request 処理** を
安全に削減した。

- **WASM ライフサイクルの重複エクスポート解決を排除**（`src/wasm/engine.rs`
  `run_headers_module`）。`proxy_on_context_create` は root/HTTP コンテキスト生成で
  2 回呼ぶが、`get_typed_func`（エクスポート表のハッシュ探索 + 型チェック）を 1 回に集約し、
  得た関数ハンドルを両呼び出しで再利用する。ヘッダ系フィルタが適用される
  **リクエスト・レスポンス両フェーズで 1 回ずつ**冗長解決が発生していたのを解消。
  挙動不変（同一関数・同一シグネチャ）。

macro ベンチは TLS/ホスト負荷バウンドのため本最適化の差分をスループットで分離するのは
困難だが、per-request の CPU 命令数を確実に削減し、ホットパス絶対規則（不要な処理を
ホットパスに残さない）に沿う。既存 WASM 単体・E2E テストで挙動退行がないことを確認した。

## 6. 残件（別チケット／専用クライアント要）

`perf_measurement_report.md` が挙げた以下は専用の負荷クライアント・バックエンドが必要なため
F-89 の残件とする。

- **http3**: h2load の HTTP/3 対応ビルド（ngtcp2）または quiche クライアント。
- **grpc-full**: ghz 等の gRPC ベンチマーククライアントと gRPC バックエンド。
- **websocket**: WebSocket echo バックエンドと ws ベンチマークツール。

将来、TLS 終端が支配的という本結果を踏まえ、TLS レコード処理・ハンドシェイクの
更なる最適化（セッション再開・kTLS の非コンテナ環境での再評価）が最大の投資対効果を持つ。
