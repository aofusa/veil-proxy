# F-56: プロパティベース・負荷テスト

## 目的

任意入力に対する不変条件（panic なし、backpressure 機能）と chaos 併用負荷を検証する。

## 改修案

- `proptest` / `quickcheck` で設定・ルーティング invariants
- `tests/load/` に wrk/k6/Artillery + Prometheus 監視
- chaos 前後の latency/error 率比較

## 受け入れ条件

- 代表 invariant テストが `cargo test` で通過
- 負荷テスト手順が `docs/backlog` に固定