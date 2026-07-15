# F-118: perf ハーネス改善（L4 readiness・TSV 出力順の明文化）と v0.5.0 計測更新

## 背景・起点

`docs/artifacts/perf_improvement_plan.md`（tools/perf ベンチマーク現状確認と改修指示書）の
ギャップ分析を受けた改修。ただし依頼時の追加指示により、指示書のうち以下は **対象外** とする。

- **nginx ベースラインの拡充（指示書ステップ 1〜3）は行わない**。nginx は現状どおり
  `nginx.conf` 単一構成（HTTP/1.1・HTTP/2 のファイル配信ベースライン）のままでよい。
- **gRPC over HTTP/1.1 のベンチマーク（指示書ステップ 5）は不要**。gRPC は仕様上
  HTTP/2 / HTTP/3 前提であり、`grpc_h1*` 構成は生成しない（現状も存在しない）。

## 現状

- Veil 側は HTTP/1.1・HTTP/2・HTTP/3・gRPC/H2・gRPC/H3（NA フェイルセーフ）・L4 の
  全プロトコル × 機能マトリクスを網羅済み（F-114）。
- `wait_ready` は `https://<container>:443/` の 200 のみで readiness を判定しており、
  L4 構成（`h2_0_feat_l4`）が実際に計測する **平文 9080 リスナーの readiness を確認していない**
  （443 の HTTPS リスナーが先に上がると L4 リスナー未起動でも計測が始まり得る）。
- TSV 出力順（nginx → veil_glibc 各構成 → veil_musl 各構成)は現状の実装順に依存しており、
  要件として明文化されていない。

## 改修内容

1. **`wait_ready` の L4 対応（指示書ステップ 4）**: `*feat_l4*` 構成では 443 に加えて
   実際に負荷をかける `http://<container>:9080/` の疎通も確認してから計測を開始する。
2. **TSV 出力順の保証**: `results_raw.tsv` は必ず「nginx ベンチマーク → glibc 版の各
   ベンチマーク → musl 版の各ベンチマーク」の順に出力する（現行の実行順を要件として
   コメント・README に明文化し、維持する）。
3. **gRPC over HTTP/3 の NA フェイルセーフは現状維持**（k6 が非対応のため）。
4. 改修後に **フルスイートを実測**し、`docs/perf/README.md`（サマリ）と
   `docs/perf/results_raw.tsv`（生データのコミット済みコピー）を v0.5.0 向けに最新化する。

## 受け入れ条件

- [ ] `h2_0_feat_l4` の計測開始前に 9080 の疎通確認が行われる（未起動なら NA を emit）。
- [ ] `results_raw.tsv` の行順が nginx → veil_glibc → veil_musl である。
- [ ] nginx ベースラインは従来どおり `base` 構成（http1.1 / http2）のみ。
- [ ] `grpc_h1*` 構成は存在しない。
- [ ] http1.1・http2・http3・grpc/h2・grpc/h3・l4 × 各機能の生 TSV を取得し、
      `docs/perf/README.md` にサマリ、`docs/perf/results_raw.tsv` に生データを反映。

## 関連

- 設計: `docs/artifacts/f118_perf_improvement_design.md`
- 起点: `docs/artifacts/perf_improvement_plan.md`
- 先行: F-114（全プロトコル×全機能マトリクス）、F-89（full features 網羅）
