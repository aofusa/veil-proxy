# 計測結果アーカイブ（要約・時系列）

`tools/perf` 導入初期からの計測レポート群の要約。生の詳細レポート（フル計測ログ含む）は
リポジトリ肥大化を避けるため `docs/artifacts/perf_reports/`（git 管理外）に退避済みで、
本ページはその内容の一次情報を失わないための要約インデックス。**現行の代表計測は
[`results_summary.md`](results_summary.md)（静的配信 HTTP/1.1・HTTP/2 完全直交表）と
[`protocol_extended_results.md`](protocol_extended_results.md)（HTTP/3・gRPC・WebSocket + F-106/F-111）
を参照**（古い数値は環境やコード修正前の値のため、現状の性能特性としては参照しないこと）。

## 時系列

1. **初期ベンチマーク（wrk のみ、host network, 10 同時接続）**: kTLS 無効時に nginx を上回る
   スループット（~168 req/s）を確認した最初の発見。glibc が musl よりわずかに高速、mimalloc
   が有効な方が高速、という傾向が最初に観測された。100 同時接続では 502 が多発し安定計測できず。
2. **B-13/B-14/B-15 発覚前の計測（`performance_report_veil_vs_nginx.md`）**: seccomp に
   `faccessat2` が未許可で静的配信が 404、`cache` feature 無効時のファイル解決スタブ問題等が
   未修正のまま計測したため、HTTP/1.1 が Non-2xx 100%・musl 版が配信不能という異常値が出た
   （後に修正され v2 で解消）。
3. **B-13/B-14/B-15 修正後の再計測 v2（`performance_report_veil_vs_nginx_v2.md`）**: 全 24 計測
   で Non-2xx=0 に回復。kTLS 無効の方がコンテナ環境で明確に高速（+36〜70%）、
   `reuseport_balancing=kernel` は単一クライアント IP 負荷で cbpf より有利だが HTTP/2 を著しく
   劣化させる（多重ストリームとの相性が悪い）ことを確認。最良構成 `no_ktls_ofc` で nginx 比
   HTTP/1.1 94%・HTTP/2 75〜79%。
4. **HTTP/2 送信ゼロコピー最適化 v3（`performance_report_veil_vs_nginx_v3.md`、F-73）**:
   v2 で最大ギャップだった HTTP/2（nginx比75%）を、`src/http2/connection.rs` の
   `write_all` が per-frame で二重確保 + 二重コピーしていたバグを解消して改善。
   HTTP/2 +11.6%（1577→1761 req/s、nginx 比 75%→84%）、HTTP/1.1 は不変（経路対象外）。
5. **完全直交表 + full features 計測（`veil_performance_report.md`、`benchmark_summary.md`）**:
   2⁴=16 の完全直交（http2×ktls×reuseport_balancing×open_file_cache）と full features
   ショーケースを整備。musl がやや高速な傾向、nginx 比で最大 1.6 倍のスループットを達成する
   構成を確認。Full features 有効化によるスループット低下は見られず。
6. **tools/perf 実装検証（`perf_measurement_report.md`）**: ハーネスが `http3`/`grpc-full`/
   `wasm`/`websocket` 等の計測を欠いていることを指摘。F-89 の起点となった。
7. **full features 機能単位オーバーヘッド計測（`perf_full_features_report.md`、F-89）**:
   wasm/metrics/access-log/rate-limit/admin/opentelemetry の各機能はベースライン比 ±5%以内
   （ノイズ範囲）で有意なオーバーヘッドなし。**TLS 終端が支配的コスト**（L4 平文素通しは
   TLS 経由の 2.2 倍）と判明。逆プロキシ経路のみバックエンドホップで -15%。
8. **HTTP/3・gRPC・WebSocket 追加計測 + F-106/F-111**: 詳細は
   [`protocol_extended_results.md`](protocol_extended_results.md) を参照。
9. **E2E プロファイリング（`performance_metrics_ecdsa_and_parallel.md`）**: `strace -c` で
   `io_uring_enter` がシステムコール時間の 44%を占めることを確認したが、これは
   thread-per-core + io_uring の設計上想定内のコスト。ゼロアロケーション設計（`mmap`呼び出し
   ごく僅少）を実測で裏付け。SO_REUSEPORT マルチワーカー化を提案し、後日実装済み
  （現行コードは全プロトコルで SO_REUSEPORT 対応済み）。

## 教訓（現行の計測方針に反映済み）

- **コンテナ（veth/bridge）では kTLS が不利**。デフォルト計測は kTLS オフで実施
  （`tools/perf/gen_configs.sh` の feat 系ベース）。
- **`reuseport_balancing=cbpf` は単一クライアント IP 負荷で 1 ワーカーに集約**され、
  ベンチマークでは `kernel` 分散の方が高スループットに見えるが、実運用（多数クライアント IP）
  では cbpf でも自然に分散する。ワークロードに応じて選択。
- **ホスト負荷（co-tenant のビルド等）が計測ノイズの支配的要因**になり得るため、静穏ウィンドウ
  （1 分 loadavg 目安 < 1.5、理想は <0.5）を確認してから計測する。
