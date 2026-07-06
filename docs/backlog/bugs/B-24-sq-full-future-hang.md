# B-24: SQ リング満杯時に I/O Future が永久ハングする

- 優先度: **P1**
- 対応状況: **完了**
- 検出: [F-68](../features/F-68-resource-exhaustion-tests.md)（リソース枯渇テスト）の設計中に
  io_uring SQ/CQ リング溢れ経路を精査して発見。

## 事象

独自 io_uring ランタイム（`src/runtime/`）の全 I/O Future（`Accept` / `Connect` /
`ReadFuture` / `WriteFuture` / `SendMsgFuture` / `Readable` / `Writable` /
`ReadableFd` / `WritableFd` / `SpliceFuture` / `Sleep`）は、初回 poll で
`ring.get_sqe()` から SQE スロットを取得し、opcode 等を書き込んで提出していた。

`get_sqe()` は **SQ が満杯のとき `None` を返す**（`tail - head >= sq_ring_entries`）。
ところが各 Future は次の形で `None` を握り潰していた:

```rust
with_ring(|ring| {
    if let Some(sqe) = ring.get_sqe() {
        // ... SQE を埋める ...
        sqe.user_data = user_data;
    }
    // None のときは何もしない（SQE は未投入）
});
let _ = submit_sqes();
self.submitted = true;              // ← SQE 未投入でも submitted 扱い
// ... peek_op_result → None → set_op_waker → Poll::Pending
```

SQE を 1 件も投入していないのに `submitted = true` として `Poll::Pending` を返すため、
`alloc_op()` で確保した `user_data` に対応する **CQE は永久に到着しない**。Waker は
決して起きず、当該 Future（= 接続 / リクエスト / タイムアウト）が**恒久的にハング**し、
`OP_TABLE` スロットもリークする。

## 影響

- SQ 満杯は次の条件で発生する:
  1. 提出前 SQE が瞬間的に `sq_ring_entries`（既定 4096）個溜まる高並行負荷。
  2. **CQ リング溢れ**で `io_uring_enter` の提出が `-EBUSY` になり SQ が消費されない
     リソース枯渇状態（F-68 の対象）。
- `Sleep` がハングすると `timeout()` が発火しなくなり、タイムアウト保護そのものが
  効かなくなる二次被害がある。
- graceful degradation（接続拒否 / エラー応答）に留まるべき過負荷時に、
  代わりに**無応答ハング**を起こす。F-68 受け入れ条件（「過負荷分は接続拒否/エラー応答で
  吸収」）に反する。

## 改修

1. `src/runtime/ring.rs` に `get_sqe_or_submit()` を追加。SQ 満杯時は **未提出 SQE を
   カーネルへ提出**（非 SQPOLL では `io_uring_enter(to_submit=pending)` が SQE を同期的に
   消費し SQ ヘッドを前進させる）してスロットを解放し、再取得する。通常はこれで必ず
   スロットが得られる。
2. 全 I/O Future の呼び出し箇所を `get_sqe()` → `get_sqe_or_submit()` へ変更し、さらに
   **スロットを確保できたかを bool で受けて**、確保失敗時は `submitted` を立てず、
   `alloc_op()` した op スロット・確保済み fd・バッファ等を解放し
   `io::ErrorKind::WouldBlock`（`Sleep` は満了扱いで完了）で **graceful に失敗**する
   ように修正。永久ハングを排除。
3. `submit_cancel`（ベストエフォート）も `get_sqe_or_submit()` に統一。

## テスト

- `src/runtime/ring.rs::tests::get_sqe_or_submit_drains_full_sq`（回帰）:
  SQ 容量ぶんの NOP で SQ を満杯にし、旧経路 `get_sqe()` が `None` を返すこと、
  新経路 `get_sqe_or_submit()` が pending を提出してスロットを確保し `Some` を返すこと、
  投入した 5 件の NOP がすべて完了（op が失われずハングしない）することを検証。

## 関連

- [F-68](../features/F-68-resource-exhaustion-tests.md)（親：リソース枯渇テスト）
- [B-07](B-07-iouring-future-drop-uaf.md)（同じく Future ライフサイクルに起因する UAF）
</content>
</invoke>
