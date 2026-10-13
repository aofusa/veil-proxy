# F-202: API ライブラリのハンドラ経路の性能改善

**状態: 未着手**

## 背景（F-201 の計測）

`tools/api-bench` の `/plaintext`（HTTP/1.1 キープアライブ、64 接続、サーバ 2 スレッド）で、veil のネイティブハンドラは
1 要求あたり 47.6 µs（io_uring）/ 41.4 µs（epoll）の CPU を使い、tokio 上の axum（29.8 µs）の 63% / 72% のスループット。
axum の `Router` を `tokio_service` で載せると 61.1 / 55.6 µs。プロキシのデータプレーンではなく、API ライブラリの
ハンドラ経路（`Backend::Handler`）の問題。

strace（5 秒、32 接続）で見た 1 要求あたりの syscall:

| 対象 | syscall / 要求 |
|---|---|
| ネイティブハンドラ（io_uring） | `io_uring_enter` 2.3 |
| ネイティブハンドラ（epoll） | `sendto` 1.0・`poll` 0.6・`read` 0.5・`epoll_ctl` 0.1 |
| axum + `veil::tokio`（epoll） | `recvfrom` 0.9・`writev` 0.5・`poll` 0.5・`epoll_ctl` 0.5 |

## 調べること

- io_uring で 1 要求あたり `io_uring_enter` が 2 回を超える理由（受信と送信を別々に submit + wait している、
  キープアライブの待ちで POLL_ADD → RECV の 2 段になっている等）。プロキシの HTTP/1.1 経路と同じ投入のまとめ方ができないか。
- 要求ごとのアロケーション（`Request` の組み立て・ヘッダ・パスパラメータ・`Response` の変換・ハンドラの `Box<dyn Future>`）を
  数えて減らす（`alloc-stats` ビルドで allocs/req）。
- `tokio_service` の `http::Request`/`http::Response` 変換（本文の `Full<Bytes>` 化）の複製を減らす。
- reactor の `poll(2)` の確認（B-75）がハンドラ経路で毎要求走っていないか。

## 受け入れ条件

- `tools/api-bench` でネイティブハンドラが tokio 上の axum と同等以上（同じ条件の交互計測、io_uring・epoll）。
- プロキシの性能（`tools/perf` の交互 A/B）に退行が無い。
