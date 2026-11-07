# B-334: Python の呼び出しごとに thread state を作って壊す

**状態: 修正済み（feat/v0120-perf、2026-11-07）**

## 事象

Python のハンドラの 1 要求ごとに `mmap` + `munmap` + `gettid` が 1 本ずつ走る（strace、4680 要求で mmap 4754・munmap 4694・gettid 4680）。

## 調査メモ

`Python::attach` は Python を知らないスレッドでは毎回 `PyGILState_Ensure` が `PyThreadState_New` を呼び、`PyGILState_Release` が
`PyThreadState_Clear` + `DeleteCurrent` で壊す。thread state の data stack のチャンクが仮想メモリの確保で、これが `mmap`/`munmap` になる。

## 改修案

スレッドごとに 1 回だけ `PyGILState_Ensure` して（カウンタを 1 に保つ）すぐ `PyEval_SaveThread` で GIL を返す。以後の
`PyGILState_Ensure` は同じ thread state を再利用する。スレッドの終わりに壊す（インタプリタが生きているときだけ）。

## 修正

`crates/veil-python/src/lane.rs` の `pin_thread_state`。最初の `PyGILState_Ensure` を対にせず残すので、以後の `Python::attach` は同じ
thread state を使う。

- スレッドの終わりでも消さない（改修案から変更）。veil のスレッドはプロセスと同じ長さ生き、終了処理の中で Python を呼ぶのは
  安全でない。インタプリタの終了で片付く。
- thread state を残すと `contextvars` の文脈もスレッドに残るので、要求は毎回新しい `Context` で呼ぶ（`call_in_new_context`）。
- `threading.local` の値は thread state に付くので、以前は要求ごとに空になっていた。残すようになったことで、利用者の値がスレッドに
  残り（ガイドに書いた）、`_run_coro`（ハンドラのスレッドで動く `async def`）のイベントループも要求ごとに作り直さなくなった。

## 受け入れ条件

- strace で要求ごとの `mmap`/`munmap`/`gettid` が消える。
- `veil.run` を抜けた後も Python が正常に動き、終了で落ちない。
