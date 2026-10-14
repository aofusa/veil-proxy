# F-205: HTTP/1.1 のキープアライブ待ちを 1 回の操作にする（io_uring の POLL_ADD → RECV・epoll の確認用 poll）

**状態: 未着手**

## 背景（F-202 の計測）

HTTP/1.1 の接続ループ（`proxy::handle_requests`）は、応答を返したあと次の要求を readiness 待ち
（`ServerTls::wait_next_request`）→ 読み取りバッファを借りて読む、の 2 段で待つ（B-101: アイドル接続に 64KB の
読み取りバッファを持たせない）。このため 1 要求あたり、io_uring は `POLL_ADD` と `RECV` の 2 操作（完了の往復が 1 回多い）、
epoll は確認用の `poll(2)`・`read(2)`・`sendto(2)` の 3 syscall になる。

F-202 の後、API ライブラリのネイティブハンドラは tokio 上の axum に対して epoll 98%・io_uring 93%。残りの差の大半はこの
2 段の待ちと見ている（axum + `veil::tokio` は epoll で `recvfrom` 0.9・`writev` 0.5・`poll` 0.5・`epoll_ctl` 0.5）。

## 案

- io_uring: provided buffers（`IORING_REGISTER_PBUF_RING` + `IOSQE_BUFFER_SELECT` の `RECV`）で、バッファを持たずに
  `RECV` を 1 回だけ出す。新しいオペコードは増えないが、io_uring の制限（restrictions）・seccomp との関係を確かめる。
  または小さな初期バッファ（nginx の `client_header_buffer_size` 相当、1〜2KB）で直接 `RECV` する（アイドル接続あたりのメモリと交換）。
- epoll: 直前の読み取りでソケットを読み切っていない（データが続いている）ときは確認用の `poll(2)` を飛ばして読む。
  FreeBSD（kqueue）では次の要求がまだ届いていないことが多い（F-141 の実測）ので、OS ごとに確かめる。

## 受け入れ条件

- `tools/api-bench` のネイティブハンドラが io_uring でも tokio 上の axum と同等以上。
- プロキシ（`tools/perf` の交互 A/B）に退行が無い。アイドル接続あたりのメモリ（B-101 の計測）が大きく増えない。

## 関連（2026-10-14）

API ライブラリのハンドラだけのリスナーについては F-206 が `cfg(feature = "handler")` の分岐で先に同じ問題を扱う
（プロキシのバイナリは変えない）。本チケットはプロキシと共通の経路が対象で、F-206 の結果（provided buffer ring の
`RECV` のリングの大きさ・`ENOBUFS` 時の戻し方・アイドル接続のメモリ）を持ち込む。
