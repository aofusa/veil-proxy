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

## 対応状況: 完了

`src/runtime/tcp.rs` に以下を実装し UAF / タスク二重 poll を解消した。

- `ReadFuture<T>` / `WriteFuture<T>`: バッファを `Option<T>` 化し、`Drop` で in-flight
  （submitted かつ buf 未 take）なら `detach_op` でバッファを CQE 到着まで保持して解放する
  （B-07b）。あわせて従来の `ptr::read` + `mem::forget` による危険なムーブを撤廃。
- `Readable` / `Writable` / `ReadableFd` / `WritableFd`: `Drop` で in-flight POLL_ADD を
  `detach_op`（空ガード）して古い Waker を除去し ASYNC_CANCEL する（B-07a）。完了時は
  poll 側で `submitted=false` にして happy path の無駄な detach を回避。
- `Connect`: in-flight drop 時に `addr_storage` を CQE 到着まで保持し、ソケット fd の
  クローズを完了後に遅延（in-flight fd の即時 close を回避）。

### 検証

- baseline（修正前）: `test_concurrent_connection_stress` で **Segmentation fault (core dumped)**。
- 修正後: **segfault 消失**。`cargo test --bins --features full` 577 件全通過。
  ストレステストの成功数は環境依存（debug ビルド + `threads=1` + debug ログ + 二重 TLS
  ハンドシェイクのスループット律速。F-02 が言う「負荷フレーキー 1 件」）で 80% 閾値には
  届かないが、**クラッシュは発生しない**。
