# B-07: io_uring Future の Drop 未実装による UAF とタスク二重 poll

## 事象

200 同時接続ストレステスト（`test_concurrent_connection_stress`）で 200 接続中 4 件しか成功せず、テスト終了後に veil バイナリが Segmentation Fault でクラッシュする。

## 影響

- **B-07a（Readable/Writable）**：高負荷時に io_uring `POLL_ADD` が in-flight のまま Future が drop される（タイムアウト等）と、OP_TABLE に古い Waker が残る。後から CQE が届くと完了済みタスクが再ポーリングされ UB → クラッシュ。
- **B-07b（ReadFuture/WriteFuture）**：`IORING_OP_RECV` / `IORING_OP_SEND` が in-flight の状態で Future が drop されると、バッファ（SafeReadBuffer/Vec）が解放されるが io_uring はまだそのアドレスを参照している。後続の受信でカーネルが解放済みアドレスに書き込み → UAF → セグフォルト。

## 根本原因

`src/runtime/tcp.rs` の `Readable<'a>`・`Writable<'a>`・`ReadableFd`・`WritableFd`・`ReadFuture<T>`・`WriteFuture<T>` のすべてに `Drop` 実装がない。  
タイムアウト等で `select_biased!` が逆側の arm を選んだとき、負けた Future は drop されるが OP_TABLE のエントリは残り続ける。

## 再現手順

```bash
./tests/e2e_setup.sh test
# → test_concurrent_connection_stress FAILED: 4/200
# → Segmentation fault (core dumped)
```

## 調査メモ

1. `timeout(200ms, stream.readable())` タイムアウト発火 → `Readable` drop → OP_TABLE に `(Pending, Some(waker))` が残存。
2. 後で POLL_ADD CQE が届く → `on_cqe` → `waker.wake()` → タスクがキューに再挿入。
3. タスクが完了後（`Poll::Ready` 返却済み）に再 `poll` → 未定義動作。
4. kTLS 有効時は `timeout(200ms/30s, stream.read(buf))` タイムアウト時に `ReadFuture<T>` が drop → `buf` 解放 → io_uring RECV の `buf_ptr` がダングリング → カーネルが解放済みメモリに書き込み → セグフォルト。

## 改修案

### B-07a: Readable / Writable / ReadableFd / WritableFd に Drop を追加

```rust
impl<'a> Drop for Readable<'a> {
    fn drop(&mut self) {
        if self.submitted {
            // POLL_ADD が in-flight の場合、完了時に古い Waker を呼ばないよう detach
            detach_op(self.user_data, Box::new(|_| {}));
        }
    }
}
```

`Writable`・`ReadableFd`・`WritableFd` も同様。

### B-07b: ReadFuture / WriteFuture の buf を Option<T> に変更

```rust
pub struct ReadFuture<T: IoBufMut> {
    fd: RawFd,
    buf: Option<T>,  // Option にして Drop で take() できるよう
    user_data: u64,
    submitted: bool,
}

impl<T: IoBufMut> Drop for ReadFuture<T> {
    fn drop(&mut self) {
        if self.submitted {
            if let Some(buf) = self.buf.take() {
                let ud = self.user_data;
                // バッファを CQE 到着まで guard に保持。カーネルが buf_ptr を使い終わってから解放。
                detach_op(ud, Box::new(move |_| drop(buf)));
            }
        }
    }
}
```

`WriteFuture<T>` も同様（`IoBuf` バッファを保持）。

## 関連コミット

- 2c7eb6e: fix(runtime): サーバが接続を処理できない 3 つの致命的バグを修正（Accept の Drop は実装済み）
- c61ba32: fix(proxy): プロトコル検出が TLS ハンドシェイクを破損する致命的バグを修正
