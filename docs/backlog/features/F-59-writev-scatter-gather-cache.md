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

## 実装（2026-07-05 / 本ブランチ）

セキュリティサーフェス拡大（restriction 許可リスト +1）を許容する判断のもと、
`IORING_OP_SENDMSG` ベースの scatter-gather 送出を実装した。

1. **`IORING_OP_SENDMSG` を `PROXY_ALLOWED_OPCODES` に追加**（`src/runtime/executor.rs`）。
   F-38 の io_uring restrictions 許可リストは 11 → 12 opcode。
2. **`SendMsgFuture`**（`src/runtime/tcp.rs`）: 2 つの不連続バッファ（ヘッダ + ボディ）を
   1 SQE / 1 CQE で送出。カーネル参照領域（`msghdr` + `iovec[2]`）は
   `Box<SendMsgState>` でヒープ固定し、スレッドローカルプール（上限 64）で再利用して
   ホットパスの確保を排除。in-flight のまま Future が drop された場合は
   msghdr / iovec / バッファの 3 者すべてを detach ガードへ移して CQE まで延命
   （B-07 ガードの拡張）。UAF・ダングリング iovec を構造的に防止。
3. **`TcpStream::writev2` / `write_all_vectored`**: 部分送信（short write）時は
   連結視の送信済みオフセット `skip` を進めて iovec を再構築・再発行（コピーなし）。
4. **`KtlsServerStream::write_all_vectored` / `SimpleTlsServerStream::write_all_vectored`**:
   平文（`TlsMode::Plain`）は 1 回の SENDMSG、kTLS はカーネル/NIC 依存の挙動差異が
   あるため、rustls はユーザー空間レコード化のため、従来の 2 回書き込みへフォールバック。
5. **配線**: メモリキャッシュヒット送出・stale-if-error 送出・HTTP/HTTPS プロキシ応答の
   ヘッダ + 初期ボディ送出（`transfer_response_with_compression` /
   `transfer_https_response_with_compression`）を `write_all_vectored` へ集約。

テスト: `src/runtime/tcp.rs::tests`（scatter-gather 送出の正確性 / 空ボディ /
4MB ボディでの short-write 継続と順序保証）、E2E 全 415 件 + cache 構成で回帰なし。

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

## 対応状況: 完了（2026-07-05。SENDMSG 追加を許容する判断により実装）