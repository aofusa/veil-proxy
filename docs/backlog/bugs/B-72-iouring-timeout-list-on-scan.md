# B-72: io_uring の `timeout()` がカーネルの timeout リストを溜め込み ASYNC_CANCEL が O(n) 走査になる

| 項目 | 内容 |
|---|---|
| 優先度 | **P1** |
| 状態 | 対応中 |
| 対象 | Linux io_uring バックエンド（既定）。reactor（BSD/macOS/`--features epoll`）は**非該当** |
| 影響 | 逆プロキシ経路の **CPU の約 40% がカーネルのリスト走査**に消える |
| 発見 | Linux h2c proxy 性能調査（`docs/artifacts/h2c_proxy_bottleneck_investigation.md` 5 章） |

---

## 事象

Linux x86_64（io_uring）の `h2c_proxy`（平文 HTTP/2 逆プロキシ）で、
veil プロセスの CPU の **40.6%** がカーネルの io_uring タイムアウトリスト処理に消えている。

`perf record -F 999 -g`（`cargo build --release`、20 秒、負荷 `h2load -c100 -m10`）:

| self% | シンボル |
|---|---|
| **36.75%** | `[k] io_cancel_req_match` |
| **3.89%** | `[k] io_timeout_extract` |

コールグラフは一意:

```
io_uring_enter → io_submit_sqes → io_issue_sqe → io_async_cancel
  → io_try_cancel → io_timeout_cancel → io_timeout_extract → io_cancel_req_match
```

同一ホスト・同一条件で静的配信（`h2c_file`）にはこの項が**まったく出ない**。

---

## 原因

**2 つの実装が噛み合って O(n) になる。**

### 1. `Sleep` が満了まで `IORING_OP_TIMEOUT` を居座らせる

`src/runtime/uring/timer.rs` の `Sleep` は 1 本ごとに `IORING_OP_TIMEOUT` を張る。
`timeout(READ_TIMEOUT, backend.read(buf))`（`src/proxy.rs` に 154 箇所）で
内側の `read` が勝つと `Sleep` は in-flight のまま drop され、
`Drop` は `detach_op_no_cancel` を呼ぶ ＝ **キャンセルを投げない**。

```rust
// src/runtime/uring/timer.rs（現状）
impl Drop for Sleep {
    fn drop(&mut self) {
        if self.submitted && take_op_result(self.user_data).is_none() {
            // TIMEOUT はカーネル参照バッファを持たず自然完了するため、キャンセルは投げず
            // Noop ガードで detach し、満了 CQE の到着時にスロットを解放する。
            detach_op_no_cancel(self.user_data, OpGuard::Noop);
        }
    }
}
```

このコメントは「キャンセル SQE + 即時 submit のシステムコールを節約する」意図だが、
**節約したコストよりはるかに大きい代償**を後段に生んでいた。
`READ_TIMEOUT = 30` 秒 × 数千 rps × 読み取り回数 ＝ カーネルの `ctx->timeout_list` に
**10 万オーダー**のエントリが常時載る（op テーブルのスラブも同数まで伸びる）。

### 2. in-flight op の drop が `IORING_OP_ASYNC_CANCEL` を投げる

`h2_select_readable_or_notify`（`src/proxy.rs`）は「ソケット可読（POLL_ADD）」と
「タスクからの notify」を race させる。**notify が勝つたびに負け arm の
`Readable` が in-flight のまま drop され**、`detach_op` →
`submit_cancel` → `IORING_OP_ASYNC_CANCEL` が出る。
per-stream タスクは Head / Body ごとに notify するため、
**1 プロキシリクエストにつき 2〜3 回**発生する。

### 3. カーネルの cancel はタイムアウトリストを線形走査する

`io_try_cancel()` は poll ハッシュで対象が見つからない（＝既に完了済み）と
`io_timeout_cancel()` へフォールバックし、**`ctx->timeout_list` を先頭から
線形に走査**して `io_cancel_req_match` で照合する。
1 で 10 万件溜まったリストを 2 の頻度で毎回走査するため O(n) が爆発する。

### 静的配信がなぜ無傷なのか

`h2c_file` は `timeout()` を 1 度も通らない（`src/cache/` にも
`src/http2/connection.rs` にも `timeout(` は存在しない）。
リストが空なら 2 の ASYNC_CANCEL は即座に空振りして返るため O(1) で済む。
**同じ ASYNC_CANCEL を払っていても、リストが空かどうかで桁が変わる。**

---

## 修正方針

**`runtime::uring::timer` を reactor バックエンドと同じ
「スレッドローカルのデッドライン最小ヒープ」実装にする。**

`src/runtime/reactor/timer.rs` に**動作実績のある同一 API の実装が既にある**
（スロット + 世代 + `BinaryHeap`。登録・キャンセルとも syscall ゼロ、
`Sleep::drop` はスロットを即解放し、ヒープ上の stale エントリは
`next_deadline()` が先頭から遅延パージする）。io_uring 版はこれを踏襲し、
**park との接続だけ**を io_uring 用にする:

- `Sleep::poll` / `Sleep::drop`: ユーザ空間のヒープ操作のみ。**SQE も syscall も出さない。**
- park（`wait_for_completions`）の直前に、**最近接の live デッドラインに対してだけ
  `IORING_OP_TIMEOUT` を 1 本アームする**（既にアーム済みでその期限が
  最近接デッドライン以下なら何もしない）。
- park の後に `fire_expired(Instant::now())` で満了分の Waker を起こす。

これで `ctx->timeout_list` の長さは **常に 0 か 1** になり、
1 が消えることで 3 も自動的に消える（2 はそのままでよい）。

**不変条件**: live なデッドライン `D_min` が存在するとき、
アーム中の TIMEOUT の期限は必ず `D_min` 以下であること
（崩れると park がタイムアウトを取りこぼす）。

---

## 影響範囲

- io_uring バックエンドの `timeout()` / `sleep()` 利用箇所すべて
  （`src/proxy.rs` 154 / `src/grpc/headers.rs` 18 / `src/http3_server.rs` 16 /
  `src/l4/proxy.rs` 15 / `src/upstream.rs` 10 ほか）。
- 公開 API（`runtime::timer::{sleep, timeout, Sleep, Elapsed}`）は不変。
- reactor バックエンドは無変更。
- 新しい io_uring オペコードは増やさない（`IORING_OP_TIMEOUT` は既に使用中）。
  seccomp 許可リストの変更も不要。
