# F-59: メモリキャッシュヒットの `writev` scatter-gather 1-syscall 送出（F-31 残件）

## 出典

`docs/artifacts/completed_tasks_remaining_work_report.md`（F-31 残タスク）。

## 概要

メモリキャッシュヒット時のレスポンス送出は現状「ヘッダー `write_all` → ボディ `write_all`」の
2 syscall。`runtime/io.rs` に `writev`（scatter-gather）を実装し、ヘッダー + ボディ（`Bytes`）を
**1 syscall** で送出してゼロコピーのまま syscall 回数を削減する。

## 現状

- `src/runtime/io.rs` の `writev` は stub。
- キャッシュヒット経路（`src/proxy.rs`）はヘッダーとボディを別々に `write_all` している。

## 改修内容

1. `src/runtime/io.rs` に io_uring `IORING_OP_WRITEV`（またはベクタ書き込み）ベースの
   非同期 `writev` を実装（所有バッファ配列を保持したまま完了待機、ゼロコピー）。
2. キャッシュヒット送出をヘッダー + ボディの 2 スライスを 1 回の `writev` にまとめる。
3. kTLS ソケットではベクタ書き込みが使えないためフォールバック（従来の 2 回書き込み）を維持。

## 受け入れ条件

- [ ] キャッシュヒットの送出 syscall が 1 回化（非 kTLS 経路）。
- [ ] `cargo test --features full` のキャッシュ系・E2E が回帰なし。

## 依存・リスク

- kTLS 経路との整合。部分書き込み（short write）時の再送ロジック。

## 対応状況: 未着手