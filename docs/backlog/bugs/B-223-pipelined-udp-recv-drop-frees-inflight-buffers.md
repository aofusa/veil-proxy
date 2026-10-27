# B-223: `PipelinedUdpRecv` の Drop と失敗の経路が、in-flight の RECVMSG が参照するバッファを `OpGuard::Noop` で解放する

**状態: 未修正（2026-10-27 起票。セキュリティレビュー RT-01・RT-04）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/runtime/uring/udp_recv.rs`・`executor.rs` の読解）。

## 事象

- `PipelinedUdpRecv`（HTTP/3 の UDP 受信の既定経路）は `batch` 個の `RecvSlot`（`buf`・`addr`・`cmsg`・`msg`・`iov` はすべて `Box`）に
  `IORING_OP_RECVMSG` を常に in-flight に保つ。
- `impl Drop for PipelinedUdpRecv`（L318 付近）は各スロットを `detach_op(slot.user_data, OpGuard::Noop)` で detach し、直後に `slots` を解放する。
  `Noop` は「カーネルがユーザメモリを参照しない op」（splice・POLL_ADD）向けで、RECVMSG には合わない。
  - `ASYNC_CANCEL` は非同期で、キャンセルの CQE が来るまでカーネルは `msghdr`・`iovec`・バッファを参照し続ける。
  - graceful shutdown（F-187）は停止中も HTTP/3 のメインループを回したあとにこの構造体を drop し、`shutdown_tasks` の drain 中も CQE を回収し続ける。
    その間に届いたデータグラム（任意の送信元から届く）は**解放済みのヒープへ書かれる**。
- `PipelinedUdpRecv::new`（L190 付近）の `arm_no_submit(fd)?` / `submit_sqes()?` の失敗の経路では、`RecvSlot` に Drop が無いので detach すら行われず、
  SQ に残った SQE が次の submit で解放済みのアドレスを指したまま発行される。
- （RT-04）`executor::shutdown_tasks` は drain の期限を過ぎても pending の op が残るとスレッドを抜け、`OP_TABLE` の TLS デストラクタが
  `Detached` のガード（バッファを所有）を drop する。リングの close のキャンセルはカーネル側で非同期（`io_ring_exit_work`）なので、
  短い窓でカーネルが解放済みのバッファへ書き得る（終了時に限る）。

`MultishotUdpRecv` は `OpGuard::Cleanup` に所有権を移していて正しい（ただし B-224）。TCP 系の Future（`ReadFuture`・`WriteFuture`・
`SendMsgFuture`・`Accept`・`Connect`）も `Cleanup` で正しい。

## 影響

終了時・失敗時の use-after-free の書き込み（書く内容は到着したデータグラム）。通常運転中には起きない。

## 改修案

- `MultishotUdpRecv` と同じく、Drop で各スロットの `Box` 群を取り出して `OpGuard::Cleanup` の閉包へ移す（CQE が来るまで生かす）。
- `RecvSlot` に Drop を実装し、`new()` の失敗の経路も同じ処理を通す。`downgrade_to_pipelined_if_einval` で `Multishot` を置き換える経路も同じガードで守る。
- RT-04: drain の期限を過ぎたときはリングを先に閉じて `io_uring_exit` の完了を待ってから `OP_TABLE` を落とすか、期限を過ぎた pending の
  ガードを意図的に漏らす（解放しない）。

## 受け入れ条件

- ASAN の E2E（`tools/container_security/chaos/e2e_sanitizer_chaos.sh` に追加。F-279）: HTTP/3 のワーカーが in-flight の RECVMSG を持ったまま
  graceful shutdown する間に UDP を打ち続けても ASAN が報告しない。
- `new()` の失敗（SQ を枯渇させる）の単体試験で、SQ に残った SQE が解放済みのアドレスを指さない（`fuzz/fuzz_targets/io_uring_executor.rs` のモデルに足す）。

## 関連

- [B-224](B-224-multishot-udp-recv-cleanup-reenters-ring.md)、[B-225](B-225-send-batch-no-drop.md)、[B-118](B-118-shutdown-abort-with-remaining-connections.md)（終了時の op テーブル）、
  [F-279](../features/F-279-security-test-gaps.md)
