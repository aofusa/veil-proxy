# B-337: 取り消しの提出が失敗すると、閉じた fd の番号へ SQE が届く

**状態: 修正済み（feat/v0120-perf、2026-11-07）**

親: [F-300](../features/F-300-iouring-batched-submission.md)（提出を park へ遅らせたので窓が広がった）。[B-242](B-242-io-uring-enter-transient-errors-not-retried.md) の一時的なエラーの扱いと組み合わさる。

## 事象

カーネルが SQE の fd を引くのは提出のとき。F-300 は「実行中の Future の drop は `detach_op` → `submit_cancel` が SQ を全部流すので、fd が閉じられる前にカーネルへ渡る」を
不変条件にしたが、この提出は失敗し得る:

- `io_uring_enter` の `EBUSY`・`EAGAIN`・`ENOMEM`（B-242 は一時的なエラーを「まだ提出していない」として成功を返す）。
- 途中までの提出（`enter` の戻り値が `to_submit` より小さい。`submit_cancel` は見ていない）。
- SQ が満杯で提出にも失敗し、取り消しの SQE が積めない（`get_sqe_or_submit` が `None`）。

このとき SQE は SQ に残ったまま `TcpStream` の drop が `close(2)` し、次の park で提出される。その間に別のスレッドの accept が同じ番号を受け取っていると、
前の接続の応答がその相手へ届く（または受信が別の接続のデータを横取りする）。接続をまたいだ漏洩になる。

コードを読み直して見つけた（2026-11-07）。実際の発生は確認していない。メモリ不足か CQ の溢れと、Future の drop（切断・タイムアウト）と、番号の再利用が揃う必要がある。

## 修正

`detach_op` は取り消しのあと SQ にまだ SQE が残っていたら、その op の SQE の fd を -1 へ書き換える（`IoUring::disarm_pending`）。

- カーネルは `EBADF` で完了するので、CQE は普段どおり届き、バッファの解放・ソケットの close のガードもそのまま走る。ダミーの完了を作らない。
- `NOP` に書き換えない（`res = 0` の完了で、accept のガードが fd 0 を閉じる）。
- `cancel_op`（F-205 の `ReadWithin`）は Future が生きていて fd も閉じないので書き換えない（書き換えると読めるはずのデータが `EBADF` になる）。
- 普段は `sq_pending()` を 1 回読むだけ（`detach_op` は取り消しのシステムコールを既に呼ぶ経路）。

## テスト

`runtime::uring::tcp::tests::test_b337_a_failed_flush_never_reaches_a_reused_fd`: `io_uring_enter` に一時的なエラーを注入して SEND の SQE を残し、stream を閉じて同じ番号へ
別のソケットを `dup2` する。park のあとで相手に何も届かないことを見る。修正を外すと落ちることを確かめた。
