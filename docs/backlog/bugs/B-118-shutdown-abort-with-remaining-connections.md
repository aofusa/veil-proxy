# B-118: graceful shutdown の待ちが切れて接続が残っていると、終了時に abort（コアダンプ）する

**状態: 完了（feat/f185-memory-leak-check）**

## 事象

SIGTERM / SIGINT のあと、`graceful_shutdown_timeout_secs`（既定 30 秒）が過ぎても接続が残っていると、
`Drain timeout ... (forcing shutdown)` のあとプロセスが abort する（終了コード 134、コアダンプ）。

```
panicked at .../std/src/thread/local.rs:429:25:
cannot access a Thread Local Storage value during or after destruction: AccessError
...
panic in a destructor during cleanup
thread caused non-unwinding panic. aborting.
```

キープアライブ中のクライアント（ロードバランサ・ブラウザ）が 1 本でもあれば起きる。TLS ハンドシェイクを始めて
いない TCP 接続 1 本でも再現する。B-115 で SIGTERM が効くようになったことで、`systemctl stop` 等でも通る経路になった
（SIGINT では以前から起きていた）。

## 原因

ワーカースレッドは drain の待ちが切れるとトップレベルの Future を終えてスレッドを抜ける。残った接続タスクは
スレッドローカルのエグゼキュータ（`EXEC_STATE`）が持ったままで、スレッドローカルの破棄の中で drop される。
タスク内の I/O Future の Drop（`runtime::uring::tcp::Readable` → `executor::detach_op`）が、先に破棄された
`OP_TABLE` に触れて panic し、デストラクタ中の panic なので abort する。

## 改修

- `runtime::shutdown_thread()`（io_uring / reactor の `executor::shutdown_tasks`）を追加。残っているタスクを
  スレッドローカルが生きているうちに drop する。io_uring 版は、drop で detach（キャンセル要求）した op と
  park 用のカーネル TIMEOUT の完了を最大 500ms 刈り取る（カーネルが参照し得るバッファを op テーブルに
  残したまま、リングより先に解放しないため）。
- HTTPS・h2c・リダイレクトの各ワーカーと HTTP/3 サーバのスレッドが `block_on` を抜けた直後に呼ぶ。
- 統合テスト `sigterm_with_open_connection_exits_cleanly`: 接続を 1 本残したまま SIGTERM を送り、drain の
  待ちが切れても終了コード 0 で終わること（修正を外すと失敗することを確認済み）。

io_uring・epoll の両ビルドで、seccomp・Landlock を有効にしたまま HTTP/1.1（TLS）・HTTP/2（TLS）・h2c の接続を
残して SIGTERM を送り、`Server shutdown complete` まで進んで終了コード 0 になることを確認した。
