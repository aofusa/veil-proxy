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

## 実装調査（2026-07 / 本ブランチ）

- キャッシュヒットのソケット送出は `runtime` の **`IORING_OP_SEND`** を使用しており、
  `IORING_OP_WRITE`/`WRITEV` は使っていない。ヘッダ + ボディを 1 syscall にまとめるには
  scatter-gather 送信＝ **`IORING_OP_SENDMSG`（iovec 付き msghdr）** の新規追加が必要。
- しかし `src/runtime/executor.rs` の `PROXY_ALLOWED_OPCODES` は F-28/F-38 で **意図的に
  最小化**（11 opcode）されており、`SENDMSG` を追加すると **io_uring のセキュリティサーフェスを
  拡大**する。AGENTS.md のホットパス/セキュリティ方針（オペコードを不用意に増やさない）に反する。
- 現状のキャッシュヒット経路は既に **ボディがゼロコピー（`Bytes`）**で、ヘッダ SEND + ボディ SEND の
  2 syscall。削減できるのは 1 syscall/ヒットのみで、**セキュリティサーフェス拡大に見合う利得が薄い**。
- 判断: **セキュリティ最優先の方針を優先し保留**。将来、SENDMSG 追加の是非（restriction 許可リストの
  レビュー含む）を独立チケットで検討する。partial-write 再送・kTLS 経路（kTLS は sendmsg 不可の
  ケースあり）の設計も前提。

## 対応状況: 保留（セキュリティサーフェス拡大とのトレードオフのため）