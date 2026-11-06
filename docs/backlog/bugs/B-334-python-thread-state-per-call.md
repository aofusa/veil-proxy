# B-334: Python の呼び出しごとに thread state を作って壊す

## 事象

Python のハンドラの 1 要求ごとに `mmap` + `munmap` + `gettid` が 1 本ずつ走る（strace、4680 要求で mmap 4754・munmap 4694・gettid 4680）。

## 調査メモ

`Python::attach` は Python を知らないスレッドでは毎回 `PyGILState_Ensure` が `PyThreadState_New` を呼び、`PyGILState_Release` が
`PyThreadState_Clear` + `DeleteCurrent` で壊す。thread state の data stack のチャンクが仮想メモリの確保で、これが `mmap`/`munmap` になる。

## 改修案

スレッドごとに 1 回だけ `PyGILState_Ensure` して（カウンタを 1 に保つ）すぐ `PyEval_SaveThread` で GIL を返す。以後の
`PyGILState_Ensure` は同じ thread state を再利用する。スレッドの終わりに壊す（インタプリタが生きているときだけ）。

## 受け入れ条件

- strace で要求ごとの `mmap`/`munmap`/`gettid` が消える。
- `veil.run` を抜けた後も Python が正常に動き、終了で落ちない。
