# B-225: `SendBatch` に Drop が無く、in-flight の SENDMSG が解放済みの送信データを読み続ける。release では未完了スロットの再利用で op のスロットが漏れる

**状態: 未修正（2026-10-27 起票。セキュリティレビュー RT-03）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/runtime/uring/udp_send.rs` の読解）。

## 事象

- `send_mmsg_flush` は `chunk` の各 `SendmmsgEntry.data` を呼び出し側の `batch: &[u8]` のスライスとして渡し、`SendBatch::poll` が
  `iov_base = entry.data.as_ptr()` を in-flight の SQE に載せる。`SendBatch` に Drop が無く、`Pending` のまま drop されても何もしない。
  `UringUdpSend` の Drop は `OpGuard::Noop` で detach する。
- 送信がソケットバッファの満杯で待ったまま親タスクが落ちる（`shutdown_tasks`）と `batch` も解放され、カーネルが解放済みのメモリを読んで
  相手へ送る（終了時の情報漏えい）。
- release ビルドでは `submit_no_flush` の `debug_assert!(!self.submitted)` が消え、未完了のスロットを再 configure すると旧 op の `user_data` が
  失われて op テーブルのスロットが漏れる（長時間で op テーブルが枯渇し、SQE の確保が `WouldBlock` を返し続ける）。

## 改修案

- `SendBatch` に Drop を実装し、未完了のスロットを `cancel_op` して `OpGuard::Cleanup` で送信データを生かす（スロットが所有するバッファへ
  コピーしておくか、`Bytes` で所有権を持たせる）。`UringUdpSend` の Drop も `Noop` をやめる。
- `debug_assert!` を実行時の検査にするか、未完了のスロットを再 configure しない設計にする（ホットパスなので分岐 1 本に留める）。

## 受け入れ条件

- 単体: `socketpair` で送信バッファを満杯にして `SendBatch` を `Pending` のまま drop し、ASAN で報告が無いこと。
- `fuzz/fuzz_targets/io_uring_executor.rs` のモデルで、未完了スロットの再 configure が op のスロットを漏らさないこと。

## 関連

- [B-223](B-223-pipelined-udp-recv-drop-frees-inflight-buffers.md)、[F-111](../features/F-111-perf-regression-check-http3-gso-zerocopy.md)（GSO 送信）
