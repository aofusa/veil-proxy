# B-224: `MultishotUdpRecv` の後始末が `consume_cqes` の中で `with_ring` を再入し、`BorrowMutError` で HTTP/3 のワーカーが落ちる

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/runtime/uring/udp_recv.rs`・`executor.rs` の読解）。`VEIL_H3_BUFRING=1`（提供バッファリングの
multishot 受信、opt-in）のときだけ通る経路。

## 事象

- `executor::poll_completions` は `with_ring(|ring| ring.consume_cqes(|cqe| process_cqe(cqe)))` で、`RING` を**可変借用したまま**各 CQE の
  `on_cqe` → `guard.run(res)` を呼ぶ。`shutdown_tasks` の `poll_completions()` も同じ。
- `impl Drop for MultishotUdpRecv` の `OpGuard::Cleanup` の閉包は `let _ = with_ring(|ring| ring.unregister_buf_ring(bgid))` を呼ぶ。
  ガードは multishot の終端の CQE で実行されるので、`RING.borrow_mut()` の再入になり `BorrowMutError` の panic でワーカースレッドが死ぬ。
  HTTP/3 のワーカーは 1 本なので HTTP/3 が全停止する。

## 改修案

- ガードからはリングに触らない。`unregister_buf_ring` を「保留中の後始末」としてスレッドローカルの `Vec` に積み、`consume_cqes` を抜けた後に
  実行する。または buf ring の登録解除は `IoUring` の Drop に任せ、ガードでは munmap だけ行う。
- 同じ再入が無いか、`OpGuard::Cleanup` の全閉包を確認する（`with_ring`・`submit_sqes`・`alloc_op` を呼ばない）。

## 受け入れ条件

- `fuzz/fuzz_targets/io_uring_executor.rs` のモデルに「ガードが `with_ring` を呼ぶ」ケースを足し、`consume_cqes` 中の再入を検出する。
- `VEIL_H3_BUFRING=1` で HTTP/3 の E2E を回し、graceful shutdown・`downgrade_to_pipelined_if_einval` の経路でワーカーが落ちない。

## 修正（v0.10.0）

- `OpTable::on_cqe_take_guard` は detach 済みの op のガードを走らせずに返し、`process_cqe` はスレッドローカルの `DEFERRED_GUARDS` に積む。`poll_completions` が `consume_cqes`（RING の可変借用）を抜けた後に `run_deferred_guards` で走らせるので、ガードの閉包が `with_ring`・`alloc_op` を呼んでも二重借用にならない。

## 関連

- [B-223](B-223-pipelined-udp-recv-drop-frees-inflight-buffers.md)、[F-115](../features/F-115-http3-recvmmsg-sendmmsg-batching.md)（提供バッファリング）
