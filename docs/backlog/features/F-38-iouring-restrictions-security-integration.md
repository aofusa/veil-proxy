# F-38: io_uring オペコード制限の security.rs 統合と stale monoio スタブ解消

- **優先度**: P1
- **対応状況**: 未着手
- **出典**: `docs/artifacts/analysis_results.md` 改善案1

## 機能説明・現状

分析レポートは `src/security.rs::apply_io_uring_restrictions()` が未実装であり、独自ランタイムの `ring_fd` を使えば `IORING_REGISTER_RESTRICTIONS` を適用できるはずだと指摘している。

調査の結果、**実態はレポートと異なり、制限は既にカスタムランタイムで実装・適用済み**である:

- `src/runtime/ring.rs::apply_restrictions()` / `enable_rings()` が `IORING_REGISTER_RESTRICTIONS` を実装。
- `src/runtime/executor.rs` がワーカー起動時に「`IORING_SETUP_R_DISABLED` 付きでリング生成 → `apply_restrictions(PROXY_ALLOWED_OPCODES)` → `enable_rings()`」のシーケンスを実行し、データプレーンに必要なオペコードのみを許可している。
- 古いカーネル（`R_DISABLED` 非対応 <5.10）では制限なしの通常リングへフォールバック。

一方 `src/security.rs::apply_io_uring_restrictions()` は **monoio 時代の dead stub**（`#[allow(dead_code)]`、常に `Unsupported` を返し `error!` を出すだけ。どこからも呼ばれていない）として放置されており、security.rs 全体にも多数の stale な「monoio」コメントが残存している。これがレポートの誤認の原因。

## 改修内容

1. `security.rs` の dead stub `apply_io_uring_restrictions()` を、実態（ランタイムが起動時に適用済み）を反映した実装に置き換える。または削除し、stale な monoio 参照コメントを正す。
2. ランタイムの制限が正しく機能していること（許可オペコードは成功、禁止オペコードは拒否）を検証するテストを追加・拡充する。
3. `PROXY_ALLOWED_OPCODES` がデータプレーンの全 SQE 提出経路を過不足なくカバーしていることをレビューする。

## 改修案

- stub をランタイムの状態を参照する形に統合し、`report_security_status()` で「io_uring restrictions: applied by runtime」を正しく表示する。
- `ring.rs` に restricted ring で禁止オペコードが拒否されることを確認するテストを追加。

## 受け入れ条件

- security.rs に monoio 由来の誤った記述・dead stub が残らない。
- restricted ring 上で禁止オペコードが拒否されることをテストで実証。
- features full / no-default-features でビルド・テスト通過。

## 依存・リスク

- ランタイムの全 SQE 提出経路が `PROXY_ALLOWED_OPCODES` 内に収まっている必要がある。逸脱があると実行時に該当 I/O が失敗する。
- 主に cleanup + テストのため低リスク。
