# F-48: Proxy-Wasm ベンチマークの拡充（F-08 残件）

- **優先度**: P3
- **対応状況**: 完了（2026-07-03。プール枯渇 + fuel/RSS 計測を実装。「HTTP コールあり」はホスト側 Pause/resume がプロキシ本経路に未配線のため対象外として明記）
- **出典**: `docs/artifacts/remaining_tasks_analysis.md`（proxy-wasm-benchmarks.md 残件）

## 機能説明・現状

`benches/wasm.rs` は `/wasm/*` 適用 vs 非適用のレイテンシ差計測のみ。以下が未計測:

- インスタンスプール枯渇時の挙動（プールサイズ超の並行リクエスト）。
- WASM fuel 消費量・RSS の自動計測。
- 外部バックエンドスタブを要する「HTTP コールあり」フィルタのベンチ。

## 改修内容

- プール枯渇シナリオ（並行度 > プールサイズ）のベンチ / テストを追加。
- fuel 消費・RSS をベンチ実行時に収集して出力。
- HTTP コールを行うフィルタ用のスタブバックエンドを `tests/test_backends` に追加しベンチ化。

## 受け入れ条件

- 上記シナリオが `cargo bench` またはベンチスクリプトで再現・計測可能。

## 依存・リスク

- ベンチのみでデータプレーン本体には触れない（低リスク）。

## 実装内容（2026-07-03 完了）

1. **プール枯渇シナリオ**: `benches/wasm.rs` に `wasm_pool_exhaustion` グループを追加。
   並行度 2 / 8 / 32 のスレッドで `/wasm/*` へ同時 Keep-Alive リクエストを行い、
   wasmtime pooling allocator のスロット競合・枯渇時のスループットを計測する。
   枯渇時でも全リクエストが成功する（エラーにならない）ことを assert で検証する。
2. **fuel / RSS の自動計測**:
   - Prometheus メトリクス `veil_wasm_fuel_consumed_total{filter,phase}` を新設
     （`metrics.rs` + `engine.rs::run_headers_module` で `fuel_limit - store.get_fuel()`
     を記録。metrics + wasm feature 有効時のみ）。
   - ベンチ末尾の `report_fuel_and_rss` がプロキシの HTTPS `/__metrics` から fuel /
     フィルタ実行回数を、`/proc/<pid>/status` から RSS を収集して stderr へ要約出力する。
   - 実測例: header_filter 適用 474 リクエストで request/response 各フェーズ
     fuel ~270 万、RSS ~85MB。
3. **付随修正**: 既存ベンチの `WASM_PATH` が `/wasm/bench`（バックエンドに実在しない
   ファイル → 404）でルート可用性判定に失敗し**全シナリオがスキップされていた**のを
   `/wasm/`（実在パス）へ修正。E2E 環境でベンチが実際に走るようになった。

**「HTTP コールあり」フィルタのベンチは対象外**: ホスト側の `dispatch_http_call` →
`FilterResult::Pause` → resume の再開処理がプロキシのリクエスト経路に未配線
（`proxy.rs` は Pause を warn して続行する）ため、HTTP コールを行うフィルタは
現状 end-to-end で機能しない。ベンチ以前にホスト実装（Pause/resume 配線）が必要で、
実装時に本ベンチへシナリオを追加する。

## 受け入れ条件の検証

- 上記シナリオが `cargo bench` で再現・計測可能
  → `cargo bench --bench wasm --features full`（E2E 環境起動時）で
  プール枯渇 3 並行度 + fuel/RSS レポートが出力されることを確認 ✓
