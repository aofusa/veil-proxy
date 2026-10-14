# F-214: HTTP/3 の io_uring バックエンドが epoll より 2 割遅い（API ライブラリのハンドラ）

**状態: 未着手（2026-10-14 に F-206 の計測で発見）**

## 事象

`tools/api-bench --proto h3`（h2load-h3、64 接続、サーバ 2 スレッド）で、veil のネイティブハンドラの HTTP/3 は
io_uring 69,039〜70,082 req/s・epoll 83,638〜85,044 req/s（plaintext。json も同じ比）。CPU µs/req も io_uring 29〜30・
epoll 24。main（566010fc）でも同じ差で、B-128（multishot の受信が有効になった）の前後で変わらない
（`docs/artifacts/b128_h3_ab.tsv`）。HTTP/1.1・h2c では io_uring と epoll はほぼ同じか io_uring が速い。

## 調べること

- HTTP/3 のメインループ（`http3_server.rs`）の受信・送信が io_uring でどの操作になっているか（`mmsg_batch_size`・
  multishot・GSO）と、1 イテレーションあたりの `io_uring_enter` の数（strace -c / perf が使えないホストなので strace）。
- reactor の `recv_drain_max` との違い（epoll は 1 回の起床で読めるだけ読む）。
- タイマーの張り方（B-72: カーネルの `IORING_OP_TIMEOUT` は park の直前に 1 本）と、HTTP/3 の起床通知のコアレッシング（F-161）。
- プロキシ（`tools/perf` の `h3_*` の構成）でも同じ差があるか。

## 受け入れ条件

- 原因の特定と、io_uring の HTTP/3 が epoll と同等以上（`tools/api-bench --proto h3` と `tools/perf` の h3 構成）。
