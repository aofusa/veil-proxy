# B-294: reactor（epoll）の `SpliceFuture` が `EAGAIN` で空回りし、時間切れも効かない。splice のパイプの fd を reactor から外さずに閉じる

**状態: 未修正（2026-11-05 起票。コードレビューで発見、実機での確認は未実施）**

## 事象（コードの読み取り）

1. **空回り**: `src/runtime/reactor/splice.rs:111-116` は `EAGAIN` のとき `fd_in` の読み取りと `fd_out` の書き込みの両方を同じ Waker で登録して
   `Pending` を返す。片方は必ずパイプで、パイプはすぐ準備できている（空のパイプは書ける、データのあるパイプは読める）。`EPOLLONESHOT` のレベル
   トリガなので即座に起き、また `EAGAIN` → 登録 → 起床を繰り返す（CPU を 1 コア使い続ける）。
   - `WouldBlock` を返さないので、`splice_body_transfer` の `READ_TIMEOUT`/`WRITE_TIMEOUT` も L4 の `idle_timeout`（`src/l4/proxy.rs:686-702`）も効かない。
   - `WakerSlot::push` が空回りのたびに Waker を 1 つ積むので、待つ間メモリが増える。
   - 対象: epoll ビルドの L4 TCP 中継（全部）と kTLS の HTTP/1.1 プロキシ。公開イメージは io_uring が使えないと epoll ビルドに切り替わる
     （F-237。Docker の既定の seccomp・Kubernetes の RuntimeDefault）ので、本番で踏み得る。
2. **パイプの fd**: `SplicePipe`（`src/ktls_rustls.rs:1860-1865,1906-1911`、`src/pool.rs:709-727`）は fd を閉じるときに reactor の `unregister` を呼ばない。
   1 の経路でパイプの fd がスレッドの FD 表に入る。転送が途中で止まって残データのあるパイプ・プールが満杯（65 本目以降）・`per_stream_pipe_enabled = true`
   では閉じられ、同じ番号の fd が同じワーカーで再利用されると `EPOLL_CTL_MOD` が `ENOENT` で失敗し、`ADD` へ戻らない
   （`src/runtime/reactor/executor.rs:236-255`、`epoll.rs:64-74`）。その接続の待ちが武装されず止まる。

## 改修案

- `SpliceFuture` は `EAGAIN` で `Err(WouldBlock)` を返す（io_uring 版と同じ意味。呼び出し側は既に `readable()`/`writable()` + 時間切れで待つ）。
  あるいは呼び出し側を `splice_now` に寄せる（[F-292](../features/F-292-ktls-h1-zero-copy-same-as-plain.md)）。L4 の `forward_direction_splice` も同じ扱いにする。
- `SplicePipe` の Drop で reactor の登録を外す（`veil_rt_reactor` のときだけ）。`EPOLL_CTL_MOD` の `ENOENT` は `ADD` でやり直す（防御）。
- 確認: epoll ビルドで L4 の接続を 1 本張り、少し送って止め、`top -H`・`strace -c` で空回りしないこと、`idle_timeout` で閉じること。
  `per_stream_pipe_enabled = true` で kTLS のプロキシを途中で切る要求を繰り返し、以後の接続が止まらないこと。epoll の E2E（F-145）。

## 関連

- [F-292](../features/F-292-ktls-h1-zero-copy-same-as-plain.md)、[F-145](../features/F-145-hotpath-syscall-reduction.md)、[B-75](B-75-epoll-et-ktls-hang.md)
