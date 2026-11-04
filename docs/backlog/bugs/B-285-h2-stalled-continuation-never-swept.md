# B-285: END_STREAM 付きの HEADERS の続き（CONTINUATION）を送らない HTTP/2 のクライアントを打ち切らない

**状態: 修正済み（v0.10.1）**

## 出典

[B-284](B-284-v0101-static-and-h2-per-request-cost.md) の改修のセキュリティレビュー（Fable 5.1、2026-11-04）。Medium。

## 事象

クライアントが END_STREAM 付き・END_HEADERS 無しの HEADERS を送り、CONTINUATION を送らずに止めると、そのストリームと接続を
いつまでも保持した。`client_body_timeout_secs`・`[http2] stream_idle_timeout_secs`（B-274・B-276）も、接続のアイドルタイムアウト（B-119）も
効かない。1 接続に 1 ストリームで足り、接続を増やせば fd・タスク・HPACK の状態を占有し続けられる（slowloris）。

## 原因

`Http2Connection` は END_HEADERS の前に `recv_headers(end_stream)` を呼ぶので、ストリームは `HalfClosedRemote` のままヘッダの続きを待つ。
受信の見回り（`StreamManager::collect_stalled_receiving`）は `Open`・`HalfClosedLocal` だけを見ていたので漏れ、接続の表にストリームが
残るためアイドルの待ち（`timeout_idle`）にも入らなかった。B-284 で見回りのタイマーを受信中のストリームがあるときだけ張るようにした
ため、この接続はタイマーを 1 本も持たなくなっていた（以前もタイマーは鳴ったが対象外だったので結果は同じ）。

## 修正（v0.10.1）

- `Stream::is_receiving_request`: 本文の途中（`Open`・`HalfClosedLocal`）か、ヘッダの続きを待っている（`receiving_headers`）なら受信中。
  見回りの対象とタイマーを張る判定の両方で使う（`src/http2/stream.rs`）。
- 単体試験 `stalled_continuation_after_end_stream_is_receiving`、統合試験 `b285_stalled_continuation_is_reset_on_h2`（生の h2c で
  HEADERS だけ送って止め、`stream_idle_timeout_secs = 2` の後に RST_STREAM か GOAWAY か切断が来る。修正前は 10 秒待っても何も来ない）。

## 関連

- [B-274](B-274-route-client-timeouts-h1-only.md)、[B-284](B-284-v0101-static-and-h2-per-request-cost.md)、B-119、B-276
