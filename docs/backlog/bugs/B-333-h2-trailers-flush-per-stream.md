# B-333: HTTP/2 のトレーラの送出が 1 ストリームごとに `send(2)` を 2 回呼ぶ

**状態: 修正済み（feat/v0120-perf、2026-11-07）**

## 事象

gRPC の unary で `send(2)` が 1 呼び出しあたり 2.0 本。256 接続 × 100 ストリームでも減らない（LD_PRELOAD で計数）。
ループバックの `send(2)` は相手側の受信処理まで含むので最も重いシステムコールで、CPU の大きな部分を占める。

## 調査メモ

`drive_h2_streams` の `H2RespMsg::HandlerTrailers`・`Trailers` は `conn.flush_write_buf().await` の後に `conn.send_trailers(..).await`（`write_all`）を呼ぶ。
HEADERS と DATA は `write_buf` に積むのに、トレーラだけ別に書き出している。100 本の応答が同時に揃っていても 200 回の `send(2)` になる。

## 改修案

トレーラも `write_buf` に積む（`send_headers_buffered_end` と同じ）。`flush_write_buf` は 1 周回に 1 回。

## 受け入れ条件

- `send(2)` が 1 呼び出しあたり 0.1 本未満（多重度に応じて減る）。
- gRPC の既存の試験（トレーラの順序・HPACK の順序）が通る。
