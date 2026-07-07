# F-89: パフォーマンス計測ハーネスの full features 網羅拡充

## 背景・機能説明

[docs/artifacts/perf_measurement_report.md](../../artifacts/perf_measurement_report.md)（tools/perf 実装検証レポート）で、
パフォーマンス計測ハーネス `tools/perf/` の計測対象に以下の不足が指摘された。

- **WASM（Proxy-Wasm）拡張のパフォーマンス計測が一切存在しない**（wasmtime ランタイム呼び出し・コンテキストスイッチのオーバーヘッドが不可視）。
- full features に含まれる `metrics` / `access-log` / `rate-limit` / `admin` / `opentelemetry` / `l4-proxy` /
  `http3` / `grpc-full` / `websocket` の各機能有効化時のオーバーヘッドが計測されていない。

default features（`http2` / `ktls`）は既存の 2⁴=16 直交表で網羅済み。
full features のうち `compression` / `cache` / `buffering` / 逆プロキシ経路（proxy）は
`h2_1_feat_*` ショーケース構成で計測済み。

## 改修内容

ベース構成（http2=on / ktls=on / kernel LB）に **1 機能だけを重ねた** `feat_*` バリアントを
`tools/perf/gen_configs.sh` に追加し、機能単位のオーバーヘッドを定量化する。

### 本チケットで実装する構成

| 構成名 | 機能 | 計測内容 |
|--------|------|----------|
| `h2_1_feat_wasm` | `wasm` | パススルーフィルタ 1 枚適用時の wasmtime 呼び出しオーバーヘッド |
| `h2_1_feat_metrics` | `metrics` | Prometheus カウンタ/ヒストグラム更新のホットパスコスト |
| `h2_1_feat_access_log` | `access-log` | JSON 構造化ログのフォーマット + 非同期ファイル出力コスト |
| `h2_1_feat_rate_limit` | `rate-limit` | スライディングウィンドウ判定（上限に達しない負荷）のコスト |
| `h2_1_feat_admin` | `admin` | Admin API 有効化時のルーティング判定オーバーヘッド |
| `h2_1_feat_otel` | `opentelemetry`(+`metrics`) | OTLP エクスポートスレッド動作時のデータプレーン干渉 |
| `h2_0_feat_l4` | `l4-proxy` | L4 TCP ストリームプロキシの素通し転送スループット（HTTP/1.1 負荷を平文で通す） |

付随する改修:

- **パススルー WASM モジュール**: `examples/wasm-filters/passthrough-filter/`（proxy-wasm SDK、
  何も変更せず `Action::Continue` を返すだけ）を新設し、ビルド済み
  `docker/assets/wasm/passthrough_filter.wasm` をコミットする（tests/wasm と同じ方針）。
- **`tools/perf/run_perf.sh`**: `docker/assets/wasm` を `/etc/veil/wasm:ro` にマウント。
  `feat_l4` 構成では負荷 URL を `http://<container>:9080/`（平文 L4 リスナー）へ切り替える。

### 残件（本チケット範囲外・別途対応）

以下は専用の負荷クライアント・バックエンドの導入が必要なため、実装状況を本表に記録した上で残件とする。

1. **`http3`**: h2load の HTTP/3 対応ビルド（ngtcp2 統合）または quiche クライアントの導入が必要。
2. **`grpc-full`**: ghz 等の gRPC ベンチマーククライアントと gRPC バックエンドが必要。
3. **`websocket`**: WebSocket echo バックエンドと ws ベンチマークツールが必要。

## 受け入れ条件

- [ ] `gen_configs.sh` が上表 7 構成を追加生成する（既存 16+4 構成は不変）。
- [ ] `run_perf.sh` で全構成がエラーなく計測完了する（Non-2xx=0、ready 失敗なし）。
- [ ] 計測結果（feat 構成のベースライン比オーバーヘッド）を `docs/artifacts/` のレポートに記録する。
- [ ] 計測で顕著なオーバーヘッドが判明した場合、最適化チケットを起票または同一作業で修正する。

## 依存・リスク

- veil:glibc / veil:musl イメージは従来どおり full features ビルドを前提とする。
- rate-limit はレートリミット上限に達すると 429 が混ざり req/s が意味を失うため、上限は負荷を大きく上回る値に設定する。
- otel エクスポート先は実コレクタではなく perf-backend（404 応答）を使う。エクスポート I/O 自体の成功は計測対象外で、収集・直列化・送信スレッドの干渉のみを見る。
