# F-112: perf ハーネスの全プロトコル×全機能 網羅計測

## 機能説明

`tools/perf`（`gen_configs.sh` / `run_perf.sh`）が生成・計測するベンチマーク構成を、
**全プロトコル（HTTP/1.1・HTTP/2・HTTP/3・gRPC over H2・gRPC over H3・L4）× 全アプリ層機能
（cache / compression / wasm / metrics / access_log / rate_limit / otel / buffering / admin）** の
組み合わせへ拡張する。起点は `docs/artifacts/perf_coverage_report.md`（網羅性評価レポート）。

## 現状（拡張前）

`docs/artifacts/perf_coverage_report.md` のレビュー結果:

- **HTTP/1.1 & HTTP/2**: File アクションに対しては直交表 16 + 各機能 feat の単体計測あり。
  ただし **Proxy アクション + 各機能** の組み合わせが欠落。
- **HTTP/3**: File のベース構成 1 種のみ（機能重ね・Proxy 経路が欠落）。
- **gRPC over H2**: ベース中継 1 種のみ（機能重ねが欠落）。
- **gRPC over H3**: 計測自体が欠落。
- **L4**: TCP 素通し 1 種のみ。

## 改修内容（tools/perf のみ・src 非変更）

`gen_configs.sh` をヘルパー関数化してマトリクスをループ生成へリファクタリングし、
不足構成を追加する。命名はプロトコル・アクション・機能が一意に特定できる規則へ統一。

- **グループA**: `h2_1_proxy_<feat>`（H1/H2 + Proxy + cache/compression/wasm/metrics/access_log/rate_limit/otel）
- **グループB**: `h3_file_<feat>` / `h3_proxy` / `h3_proxy_<feat>`（HTTP/3 + File/Proxy + 各機能）
- **グループC**: `grpc_h2_<feat>` / `grpc_h3` / `grpc_h3_<feat>`（gRPC + H2/H3 + 各機能）

`run_perf.sh` は構成名からクライアント（wrk / h2load / h2load-QUIC / k6-gRPC）を
汎用パターンで判定するようディスパッチを更新する。

## 改修案・判明した制約

- **gRPC over HTTP/3**: k6（`grafana/k6`）は gRPC over QUIC/H3 をネイティブ非対応。
  レポート指示どおり **フェイルセーフでスキップ（NA を emit して次へ）** する。将来 h2load/
  専用クライアントが対応したら差し替える。
- **L4 + metrics/access_log/rate_limit（グループD）**: `L4ListenerConfig`（`src/config.rs`）に
  per-listener の metrics/access_log/rate_limit フィールドは **存在しない**（これらは L7 の
  `[route.*]` / グローバル `[prometheus]` 設定）。したがって L4 単体へこれらを重ねる構成は
  **生成対象外（N/A）** とし、本チケットでは L4 ベース 1 種を維持する。
- 既存の直交表 16 構成・nginx ベースライン計測・`results_raw.tsv` の出力フォーマット互換は維持する。
- 構成数が大幅に増える（60+）ため、`run_perf.sh` に `CONFIG_GLOB` 環境変数を追加し、
  scoped 計測（例: HTTP/3 と gRPC のみ）を可能にする（既定は従来どおり全構成）。

## 受け入れ条件

- `gen_configs.sh` がグループ A/B/C を規則的命名で生成し、`run_perf.sh` が各構成を
  適切なクライアントへディスパッチする（gRPC over H3 はフェイルセーフ NA）。
- 既存 16 直交表 + nginx ベースライン + 既存 feat 構成が従来どおり動作する。
- `docs/perf/` に拡張後の計測サマリを残す。

## 依存・リスク

- 構成数増加により full sweep は長時間化（4 コア + co-tenant 負荷では数時間）。`CONFIG_GLOB` で緩和。
- 一部機能重ね（otel の実コレクタ非存在など）は既存 feat 構成と同じ前提（干渉のみ計測）。
- 関連: [F-89](F-89-perf-full-features-coverage.md)（feat 網羅第1〜3弾）、
  [F-111](F-111-perf-regression-check-http3-gso-zerocopy.md)（回帰確認 + H3 GSO ゼロコピー）、
  [F-113](F-113-http3-recvmmsg-sendmmsg-batching.md)（H3 バッチ syscall 削減）。
</content>
</invoke>
