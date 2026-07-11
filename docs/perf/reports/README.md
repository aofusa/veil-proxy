# docs/perf/reports — パフォーマンス計測結果アーカイブ

`tools/perf/` ハーネスおよび各種手動計測で得たパフォーマンス計測結果レポートを集約する
（旧 `docs/artifacts/` から移設）。最新の集計は親ディレクトリ
[`docs/perf/results_summary.md`](../results_summary.md) を参照。

| ファイル | 内容 |
|----------|------|
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
