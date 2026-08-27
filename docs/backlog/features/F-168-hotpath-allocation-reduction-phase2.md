# F-168: ホットパス残存アロケーションの削減（F-165 / F-166 の続き）

## 背景

F-165（`alloc-stats` によるアロケーション実測）と F-166（gRPC / epoll 最適化）で
HPACK 動的テーブル・ボディ中継・H2C クライアントのプール返却はゼロアロケーション化された。
しかし実測（2026-08-26 起点）では **`h2c_file` 3B 静的で 33.4 allocs/req**、
**`h2c_proxy` 54KB で 50.7 allocs/req** と、リクエスト固定費が支配するワークロードで
依然として 30 回超の確保が残っている。

`docs/artifacts/perf_bottleneck_review_and_improvement_plan.md` の調査を
**着手前にコード照合でレビューし**（結果は `docs/artifacts/f168_plan_review.md`）、
妥当と判断した案のみを実施する。

## 改修内容

| 段階 | 内容 |
| :-- | :-- |
| P1 | **HPACK Huffman エンコードのゼロアロケーション化**（`huffman_encode_into`）と、**HTTP/2 レスポンスヘッダ名小文字化のゼロアロケーション化**（`send_headers_internal` へ一元化し、既に小文字の名前は借用） |
| P2 | **HTTP/HTTPS コネクションプールのキーアロケーション排除**（`put(&str)` 化 + プールキーのスタックバッファ化） |
| P3 | **HTTP/3 のイベント収集バッファ再利用**と **per-request `Arc<str>` 確保の排除** |
| P4 | **`add_request_headers` のプレースホルダ事前解析** |
| P5 | **HTTP/1.1 `handle_requests` の `Bytes` ゼロコピー化** |

## 不採択（レビュー結果）

- **静的配信レスポンスヘッダの事前キャッシュ化**: 削れるのは外枠 `Vec` 1 回のみ。
  一方でキャッシュエントリに `Date` / `alt-svc` 等の動的ヘッダとの分離という不変条件を
  追加することになる。F-157 が「静的配信のコピー削減は L2/L3 residence のため効かない」と
  実測済みであり、測定でヘッドルームが見えるまで着手しない。
- **io_uring `IORING_OP_TIMEOUT` の coalescing**: B-72 の不変条件
  「live な `D_min` が存在するときアーム中 TIMEOUT の期限は必ず `D_min` 以下」を壊す。
  現行実装には既に `armed.deadline <= d_min` のスキップガードがあり、追加の余地が無い。

## 検証方法（AGENTS.md 準拠）

- `tools/perf/alloc_measure.sh`: 狙ったアロケーションが実際に消えたかの決定的証拠（allocs/req）。
- `tools/perf/h2c_proxy_lab.sh cpuab`: 交互 A/B での **veil の CPU/req**（クライアント・上流と
  4 コアを共有するため rps は veil 単体の改善に鈍い）。**3B と 54KB の両方**で測る。
- 効果が確認できなかった段階は**差し戻す**。
