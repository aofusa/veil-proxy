# docs/perf/reports — パフォーマンス計測結果アーカイブ

`tools/perf/` ハーネスおよび各種手動計測で得たパフォーマンス計測結果レポートを集約する
（旧 `docs/artifacts/` から移設）。最新の集計は親ディレクトリ
[`docs/perf/results_summary.md`](../results_summary.md) を参照。

| ファイル | 内容 |
|----------|------|
| [perf_http3_grpc_websocket_report.md](perf_http3_grpc_websocket_report.md) | HTTP/3(QUIC)/gRPC/WebSocket 計測（F-89 第 3 弾）。HTTP/3 は HTTP/2 比 約 1/7（userspace QUIC コスト）等。 |
| [perf_f106_h2c_pooling_report.md](perf_f106_h2c_pooling_report.md) | H2C バックエンド接続プーリング（F-106）。gRPC 中継 +133%（同一セッション A/B）。 |
| [perf_f111_regression_http3_gso_zerocopy_report.md](perf_f111_regression_http3_gso_zerocopy_report.md) | perf 回帰確認（F-111）。HTTP/1.1・HTTP/2・gRPC(F-106) は退行なし。HTTP/3 GSO 送信のゼロコピー化 + 再調査（ストリーム上限/ペーシング/ワーカー分散はいずれも主因でないことを確認、構造的コストの結論を追認）。 |
| [perf_full_features_report.md](perf_full_features_report.md) | full features 機能単位オーバーヘッド計測（F-89: wasm/metrics/access-log/rate-limit/admin/otel/l4）。 |
| [perf_measurement_report.md](perf_measurement_report.md) | `tools/perf` 実装検証レポート（F-89 起点。full/default features・WASM 計測の網羅性評価）。 |
| [benchmark_summary.md](benchmark_summary.md) | ベンチマーク要約。 |
| [veil_performance_report.md](veil_performance_report.md) | Veil パフォーマンス総合レポート。 |
| [performance_report.md](performance_report.md) | 初期パフォーマンスレポート。 |
| [performance_report_veil_vs_nginx.md](performance_report_veil_vs_nginx.md) | Veil vs nginx 比較（v1）。 |
| [performance_report_veil_vs_nginx_v2.md](performance_report_veil_vs_nginx_v2.md) | Veil vs nginx 比較（v2）。 |
| [performance_report_veil_vs_nginx_v3.md](performance_report_veil_vs_nginx_v3.md) | Veil vs nginx 比較（v3）。 |
| [performance_metrics_ecdsa_and_parallel.md](performance_metrics_ecdsa_and_parallel.md) | ECDSA・並列性メトリクス。 |
| [perf_results_v2_raw.tsv](perf_results_v2_raw.tsv) | v2 計測の生データ。 |

> 計測スクリプト・生ログの大量ダンプ（旧 `perf_benchmark/` / `perf_test/` 等）は
> リポジトリを肥大化させるため追跡対象外（`docs/artifacts/` 配下のスクラッチに保持）。
> 再現手順は [`tools/perf/`](../../../tools/perf/) を参照。
