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


## 実測結果（詳細は `docs/artifacts/f168_measurements.md`）

| 構成 | 変更前 | 変更後 | 削減 |
|---|---|---|---|
| `h2c_file` 3B（静的） | 20.367 | **18.324** allocs/req | **-10.0%** |
| `h2c_proxy` 3B | 42.613 | **33.566** allocs/req | **-21.2%** |
| `h2c_proxy` 54KB | 38.693 | **29.667** allocs/req | **-23.3%** |
| HTTP/1.1 proxy 3B | 39.547 | **29.552** allocs/req | **-25.3%** |
| HTTP/1.1 proxy 54KB | 38.753 | **28.752** allocs/req | **-25.8%** |

**スループット・CPU/req は交互 A/B（6 ラウンド）で有意差なし。** 削減量 9〜10 allocs/req は
mimalloc の数十 ns で処理されるため、24µs/req の経路に対して理論値でも 1% 未満であり、
計測系のノイズ（±5%）に埋もれる。**変更を保持した根拠**は (1) アロケーション削減自体は
実測で確定、(2) AGENTS.md のホットパス絶対規則に沿う、(3) コードはむしろ単純化されている
（小文字化の重複 4 箇所 → 1 箇所、プールキーの自由関数 → 型に集約）の 3 点。

## P4 の挙動変更（README に明記済み）

`add_request_headers` の値展開が `String::replace` の 3 連鎖から 1 パス走査になったため、
**展開後の値が再度プレースホルダとして解釈されなくなった**（旧実装ではクライアント IP が
`$host` を含むと二重置換されていた）。1 パス方式が正しい挙動であり、回帰テストで固定した。
