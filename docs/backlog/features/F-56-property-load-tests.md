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

## 追加実装（2026-07-06）

- **設定パーサの proptest 化**: `tests/config_proptest.rs`（512 ケース/プロパティ）で
  `veil::config` の不変条件を検証。
  1. `ProxyTarget::parse` は任意文字列で panic せず `Some`/`None` を **決定的**に返す。
  2. well-formed な `scheme://host[:port][/path]` は必ず `Some` になり、
     host / port / use_tls / path_prefix が入力どおりに **ラウンドトリップ**する。
  3. スキーム欠落入力は必ず `None`。
  4. 明示ポートなしの既定ポート意味論（http=80 / https=443）と `is_default_port` の整合。
  5. `test_config_file` は任意バイト列でも panic せず `Ok`/`Err` を返す（決定的）。
- **負荷ハーネス**: `tests/load/`（`run_load.sh` = wrk/k6、`k6_load.js` = 閾値付き、
  `compare.sh` = baseline vs chaos 比較、`README.md` = 手順）。docker のみで完結し、
  `tools/container_security/` の chaos と併用して chaos 前後の latency/error 率を比較する。

## 残件

- 実 docker 環境での負荷 + chaos フル計測レポート（環境依存のため CI/計測環境側で実施）。

## 受け入れ条件

- [x] 代表 invariant テストが `cargo test` で通過（`tests/routing_proptest.rs` 5 プロパティ
      + `tests/config_proptest.rs` 4 プロパティ）。
- [x] 負荷テスト手順が固定（`tests/load/README.md`、手順・chaos 併用・合否目安を記載）。