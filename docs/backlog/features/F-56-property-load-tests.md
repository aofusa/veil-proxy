# F-56: プロパティベース・負荷テスト

## 目的

任意入力に対する不変条件（panic なし、backpressure 機能）と chaos 併用負荷を検証する。

## 改修案

- `proptest` / `quickcheck` で設定・ルーティング invariants
- `tests/load/` に wrk/k6/Artillery + Prometheus 監視
- chaos 前後の latency/error 率比較

## 実装済み（2026-07-05）

- `proptest` を dev-dependency に追加（テスト専用。データプレーン依存には含めない）。
- `tests/routing_proptest.rs`（512 ケース/プロパティ）で `veil::routing` の不変条件を検証:
  1. **`OptimizedRouter::get_candidates`**: 任意 Host/Path/送信元 IP で panic せず、
     結果が昇順ソート済み・重複なし・全インデックス `route_count` 未満・**決定的**。
  2. **Host サフィックスワイルドカード**（`*.example.com`）: ちょうど 1 ラベルの
     サブドメインだけにマッチし、ベースドメイン・多段サブドメインは非マッチ。
  3. **Path プレフィックスワイルドカード**（`/api/*`）: `/api` 配下のみにマッチし、
     語境界（`/apix`）を誤検出しない。→ 実行中に **[B-22](../bugs/B-22-path-wildcard-boundary-mismatch.md)**
     （`/api`・`/api/` 取りこぼし）を検出・修正。
  4. **`RouteCache`**（xxh3 キー + スレッドローカル LRU）: put した値を同一キーで
     get すると必ず一致（キャッシュ整合）。

## 残件

- `tests/load/`（wrk/k6）による負荷 + chaos 併用の latency/error 率比較
  （負荷ハーネスは `tools/container_security/` の既存 chaos と統合予定。P2）。
- 設定パーサ（`config.rs`）の proptest 化。

## 受け入れ条件

- [x] 代表 invariant テストが `cargo test` で通過（`tests/routing_proptest.rs` 5 プロパティ）。
- 負荷テスト手順が `docs/backlog` に固定（残件）。