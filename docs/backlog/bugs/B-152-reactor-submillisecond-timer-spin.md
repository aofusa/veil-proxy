# B-152: reactor が 1ms 未満の先のタイマーで poller を空回りさせていた

**状態: 完了（feat/v080-release、2026-10-18）**

## 症状

reactor（epoll・kqueue・WSAPoll）の実行器は、次のタイマーまでの時間を `as_millis()` で**切り捨てて**poller に渡していた。
期限が 1ms 未満の先にあると待ち時間が 0 になり、poller はすぐ戻り、タイマーはまだ満了しないので、期限まで空回りした。
FreeBSD の HTTP/3（54KB）で、1 スレッドが 5 秒に 268 万回の空の `kevent` を出していた（要求 1 つに約 80 回）。

## 修正

`next_timeout()` は `Duration` を返す。kqueue はナノ秒の `timespec` で渡し、epoll・WSAPoll はミリ秒へ**切り上げる**。
空の kevent は 80 → 0.2 回/要求。TLS 中継 3B の CPU/要求も −5%。

## 関連

F-240、F-151。
