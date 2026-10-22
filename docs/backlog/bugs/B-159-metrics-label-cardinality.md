# B-159: クライアントが決めた Host・メソッド・gRPC のパスがメトリクスのラベルになり、系列が際限なく増える

**状態: 完了（release/v0.8.4、2026-10-22。v0.8.4 で公開）**

## 現象

`[prometheus] enabled = true` のとき、要求の Host ヘッダがそのまま `host` ラベルになっていた。
- 対象のメトリクス: `veil_proxy_http_requests_total`・`veil_proxy_http_request_duration_seconds`・
  `veil_proxy_http_active_connections`・`veil_proxy_cache_*`・`veil_proxy_buffering_used_total`
- 系列は消えないので、でたらめな Host を送るだけでプロセスのメモリと `/__metrics` の応答が増え続ける（リモートからのメモリ枯渇）。
  - 手元の再現: Host を 5000 種類送ると、`/__metrics` の応答が 10.8MB、コンテナの RSS が 27MB → 63MB。
  - 1 種類あたり約 2KB（ヒストグラムのバケットを含む）。上限は無い。
- 同じ形の問題がほかに 2 つあった。
  - `method` ラベル: 要求のメソッドがそのまま入る。
  - `veil_grpc_requests_total`・`veil_grpc_stream_duration_seconds` の `method` ラベル: 要求のパスがそのまま入る。
    上流はでたらめなパスにも `UNIMPLEMENTED` のトレイラーを返すので、そのたびに記録される。
- キャッシュ・バッファリングの記録は `[prometheus] enabled = false` でも動いていて、無効でも系列が増えていた。

container_security の `metrics_probe` は、この状態で失敗していた（`metrics_ok: 0/100`）。それまでの試験で溜まった系列で応答が
大きくなり、試験の `curl | grep -q` が pipefail の下で偽の失敗になった（grep が先に終わり、curl の書き込みが失敗する）。

失敗は集計に出なかった。`lib/report.sh` は「レポートに `: ok` の行が 1 つでもあれば passed」と判定していて、同じレポートの
`post_flood_health: ok` で passed に数えた。`full_features_security` も同じ理由で passed になっていた。

## 修正

- `host` ラベル: 要求の Host が当たったルートの `host` 条件を、設定に書いたまま使う。
  - 例: `api.example.com`。`*.example.com` に当たった Host は `*.example.com`。
  - どれにも当たらなければ `other`。
  - 表（`metrics::HostLabels`）は設定のロード・リロード（VDS を含む）で作り直す。分類は `routing::HostRouter` と同じ。
  - 要求ごとの処理は `ArcSwap` の読み出しと、スタック上の小文字化だけ（アロケーションしない）。
  - 接続数のゲージは表の `Arc<str>` を持つ。以前は接続ごとに `String` を確保していた。
- `method` ラベル: RFC 9110 §9 のメソッドと PATCH のどれか。ほかは `OTHER`。
- gRPC: 次のものは `method` を `other` にする。
  - 上流が `UNIMPLEMENTED`（12）を返したもの
  - `/<service>/<method>` の形でない（256 バイトまでの表示可能な ASCII でない）もの
  - 1024 種類を超えた新しいもの
  `status_code` は `0`〜`16`、ほかは `other`。
- キャッシュ・バッファリングの記録も `[prometheus] enabled` を見る。
- 文書: `docs/guide/observability.md`（英日）・`examples/config.toml` にラベルの値の範囲を書いた。

**試験**

- 単体: `host_labels_are_bounded_by_config`・`method_labels_are_bounded`・`grpc_method_labels_are_bounded`。
- container_security の `metrics_probe`:
  - 本文を変数に受けてから調べるようにした。
  - でたらめな Host を 200 種類送り、`veil_proxy_http_requests_total` の系列が増えないことを確かめる
    （`metrics_host_label_bounded`）。
- `lib/report.sh`: 明示的な失敗の印（行頭の `FAIL`・`: FAILURES`・`: FAIL`・`: fail`）を `: ok` より先に見る。
  今回の結果に当てはめると、印を持つのは本当に失敗した 2 つ（`metrics_probe`・`full_features_security`）だけだった。
- `tools/release/image-smoke.sh` の `/quickstart.html` の確認も、本文を変数に受けるようにした。
  - v0.8.4 の準備で 1 回だけ失敗した。その後の 12 回、新しいコンテナでの 80 回（負荷あり・なし）では再現しなかった。
  - 失敗したら、curl の終了コード・状態・大きさを出す。

## 変わること（利用者向け）

`host` ラベルの値が、要求の Host から、設定の `host` 条件に変わる。`host` 条件の無い設定では、すべて `other` になる。
Host ごとに集計していたダッシュボードは、ルートに `host` 条件を書けば同じ粒度になる。
