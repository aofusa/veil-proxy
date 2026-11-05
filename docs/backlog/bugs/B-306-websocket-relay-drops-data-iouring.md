# B-306: HTTP/1.1 の WebSocket の中継が io_uring で受信済みのデータを捨て得る（読み取りを時間切れで drop）。交互のポーリングが重く、書き込みに時間の上限が無い

**状態: 未修正（2026-11-05 起票。コードレビューで発見、再現は未実施。行番号は f8dc80de）**

## 事象（コードの読み取り）

1. **データを失う**: 中継（`src/proxy/websocket.rs:466-552`。平文の上流 :472,501、TLS の上流 :579,606）は両側を交互に
   `timeout(poll_timeout, read)` で読む。時間切れが先に来ると io_uring の `ReadFuture` が drop され、その Drop（`src/runtime/uring/tcp.rs:1092-1106`）は
   カーネルが既に読んだデータごとバッファを捨てる（ランタイム自身のコメント `tcp.rs:497-500`「`ReadFuture` を drop すると失う。F-116」）。既定の
   poll の時間切れは 1ms から始まるので、競合の窓は毎ミリ秒開く。平文・kTLS の上流、平文・kTLS のクライアントで起きる（rustls のクライアントと reactor は起きない）。
   WebSocket のフレームが途中で欠け、相手はプロトコルエラーで切る（あるいはメッセージが黙って欠ける）。
2. **重い**: アイドルの接続でも約 100ms ごとに RECV 2 本と取り消し 2 本を出す。逆方向のデータは最大 100ms 待たされる。
3. **書き込みに時間の上限が無い**（`write_all`、:482,511,589,616）: 読まない相手で中継が永久に止まり、`websocket_idle_timeout_secs` にも届かないので
   遅い読み手が両方の接続を握り続けられる。

## 影響

- P1（データの欠落。負荷・往復の多いアプリで顕在化）。

## 改修案

- 両方向を同時に待つ（`FuturesUnordered` か `fuse()` した `select!`。`futures::join!` は使わない。F-173）。読み取りは `readable()` + 非ブロッキングの read、
  または `read_within`（F-205。取り消しと行き違いのデータも失わない）。平文・kTLS の組は splice（[F-292](../features/F-292-ktls-h1-zero-copy-same-as-plain.md) の
  `splice_now`）。書き込みは `WRITE_TIMEOUT`。アイドルは `websocket_idle_timeout_secs` で両方向の最後の活動から数える。
- 作った中継の部品を [F-288](../features/F-288-websocket-over-h2-h3.md)（HTTP/2・HTTP/3）でも使う。
- 試験: 数 KB のフレームを数万往復させて欠けないこと（io_uring・epoll、平文・kTLS・rustls、上流は平文・TLS）、読まないクライアントで時間切れ後に閉じる、
  アイドルの接続で syscall が出ない（`strace -c`）。

## 関連

- [F-288](../features/F-288-websocket-over-h2-h3.md)、[F-116](../features/F-116-http2-stream-multiplexing.md)（`ReadFuture` の drop）、[B-307](B-307-websocket-upgrade-handling.md)
