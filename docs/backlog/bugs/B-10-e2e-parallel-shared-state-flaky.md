# B-10: E2E テストの並列実行による共有状態競合（ロードバランシング系フレーキー）

- **優先度**: P2
- **対応状況**: 完了（2026-07-02）

## 完了メモ

- `tests/e2e_setup.sh` に専用プール `rr-isolated-pool` と専用ルート `/rr-test/*`（+fixtures）を追加し、RR 分散を Assert する 2 テスト（`test_round_robin_distribution`, `test_load_balancing_round_robin_distribution`）を隔離パスへ移行。
- 併せて同種の共有状態競合だった `cache::revalidation` の単体テスト 3 件をミューテックスで直列化（並列実行時の偽陽性を解消）。
- 修正後の features full E2E で `test_round_robin_distribution` 通過を確認。
- 調査中に別の実バグ（Expect: 100-continue 間欠ハング）を発見 → [B-11](B-11-expect-100-continue-intermittent-hang.md) として起票。
- **出典**: `docs/artifacts/analysis_results.md` 改善案4

## 事象

Cargo テストはデフォルト並列実行される。`tests/e2e_tests.rs` の `test_round_robin_distribution` は共有パス `/` に 10 回リクエストして分散カウントを Assert するが、同時に走る他テスト（`test_proxy_basic_request` 等）も同じ `/`（同じアップストリームプール・共有 Round Robin ステート）へリクエストするため、順序が干渉して Assert が失敗し得る（Flaky Test）。

## 影響

- E2E の信頼性低下。環境負荷と併せて偽陽性の失敗が発生（関連: features-full E2E の負荷フレーキー）。

## 調査メモ

- Round Robin のカーソルはプロキシプロセス内でアップストリームプールごとに共有。テスト間で論理的に隔離されていないことが根本原因。

## 改修案

- ロードバランシング分布を検証するテスト専用のルート（例: `/rr-test/`）と専用アップストリームプールを `tests/e2e_setup.sh` の設定に追加し、他テストとステートを共有しないようにする。
- もしくは分布 Assert を「合計一致 + 各バックエンド最低 1 回」など干渉に頑健な条件へ緩和する（隔離が優先）。

## 受け入れ条件

- `--test-threads` 既定の並列実行で当該テストが安定して通過する。
