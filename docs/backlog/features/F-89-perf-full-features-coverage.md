# F-89: パフォーマンス計測ハーネスの full features 網羅拡充

## 背景・機能説明

`docs/perf/history.md` §6（tools/perf 実装検証レポートの要約）で、
パフォーマンス計測ハーネス `tools/perf/` の計測対象に以下の不足が指摘された。

- **WASM（Proxy-Wasm）拡張のパフォーマンス計測が一切存在しない**（wasmtime ランタイム呼び出し・コンテキストスイッチのオーバーヘッドが不可視）。
- full features に含まれる `metrics` / `access-log` / `rate-limit` / `admin` / `opentelemetry` / `l4-proxy` /
  `http3` / `grpc-full` / `websocket` の各機能有効化時のオーバーヘッドが計測されていない。

default features（`http2` / `ktls`）は既存の 2⁴=16 直交表で網羅済み。
full features のうち `compression` / `cache` / `buffering` / 逆プロキシ経路（proxy）は
`h2_1_feat_*` ショーケース構成で計測済み。

## 改修内容

ベース構成（http2=on / ktls=off / kernel LB。kTLS はコンテナ環境と相性が悪いため
feat 系構成では既定オフとし、kTLS 自体の影響は直交表 16 構成の ktls 因子で計測する）に
**1 機能だけを重ねた** `feat_*` バリアントを
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

### 残件だった 3 プロトコル（第 3 弾で実装・完了）

専用の負荷クライアント・バックエンドを導入し計測可能にした。

| 構成 | クライアント | 上流 | 計測内容 |
|------|--------------|------|----------|
| `h2_1_feat_http3` | QUIC 対応 h2load（`local/h2load-h3`, `--alpn-list=h3`。`tools/perf/h2load-http3/` でビルド） | なし（静的配信） | HTTP/3 (QUIC/UDP) スループット |
| `h2_1_feat_grpc` | grafana k6 gRPC（`tools/perf/k6/grpc.js`） | `moul/grpcbin`(h2c) | gRPC unary 中継（TLS h2 → h2c）のフレーミング/中継コスト |
| `h2_1_feat_websocket` | grafana k6 WebSocket（`tools/perf/k6/websocket.js`） | `jmalloc/echo-server` | WebSocket Upgrade + フレーム転送コスト |

- **既定 `local/h2load` は QUIC 非搭載**のため、ngtcp2/nghttp3/quictls を組み込んだ
  `local/h2load-h3` ビルド用 Dockerfile（`tools/perf/h2load-http3/`）を新設。未ビルド時は http3 をスキップ。
- gRPC の h2c フルパス保持・`use_h2c` 尊重・HPACK 小文字化は **B-40（F-92）で修正済み**であり、
  本ハーネスはその修正後イメージ前提で Non-2xx=0 を確認（当初 grpcbin/tonic ともに 502 になる
  現象を再現し、B-40 修正が有効であることを検証）。
- k6 gRPC は完了ストリームごとに RST_STREAM を送るため、grpc 構成で
  `[http2] max_rst_stream_per_second` を大きく設定し Rapid Reset 対策（CVE-2023-44487）の誤検知を回避。

## 受け入れ条件

- [x] `gen_configs.sh` が上表 7 構成を追加生成する（既存 16+4 構成は不変。合計 27 構成）。
- [x] `run_perf.sh` で全構成がエラーなく計測完了する（feat 系 9 構成 × HTTP/1.1・HTTP/2 で **Non-2xx=0**、ready 失敗なし）。
- [x] 計測結果（feat 構成のベースライン比オーバーヘッド）を
  [docs/perf/protocol_extended_results.md](../../perf/protocol_extended_results.md) に記録する。
- [x] 計測で顕著なオーバーヘッドが判明した場合、最適化チケットを起票または同一作業で修正する
  → 計測は **TLS 終端が支配的・L7 機能ロジックはノイズ範囲内**であることを示した。レポート主要関心事の
  WASM ホットパスの冗長な per-request エクスポート解決を同一作業で排除（`src/wasm/engine.rs`）。
- [x] **http3 / grpc / websocket 計測を専用クライアントで実装**（第 3 弾。合計 30 構成）。
  gRPC の h2c 中継バグは B-40（F-92）で修正済みであることを本ハーネスで再検証。

## 完了

- 完了日（第 1・2 弾）: 2026-07-07 / 完了日（第 3 弾 http3/grpc/websocket）: 2026-07-11
- 主要成果: feat 系 9 構成 + http3/grpc/websocket 3 構成の計測ハーネス化（合計 30 構成）、
  パススルー WASM モジュール、機能単位オーバーヘッドの定量化（全構成 Non-2xx=0）、
  WASM ライフサイクルの重複エクスポート解決排除、QUIC 対応 h2load / k6 クライアントの導入。
- 計測結果は [docs/perf/protocol_extended_results.md](../../perf/protocol_extended_results.md) を参照。

## 依存・リスク

- veil:glibc / veil:musl イメージは従来どおり full features ビルドを前提とする。
- rate-limit はレートリミット上限に達すると 429 が混ざり req/s が意味を失うため、上限は負荷を大きく上回る値に設定する。
- otel エクスポート先は実コレクタではなく perf-backend（404 応答）を使う。エクスポート I/O 自体の成功は計測対象外で、収集・直列化・送信スレッドの干渉のみを見る。
