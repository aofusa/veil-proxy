# B-309: kqueue（macOS・BSD）で WebSocket の中継が片方向の読み取りに止まり、反対方向を中継しなくなる（古い readiness ヒント）

**状態: 修正済み（v0.11.0）**

## 事象

macOS（ssh llm、`full-macos`）の E2E で `test_b306_websocket_relay_keeps_every_byte`（HTTP/1.1）と
`test_f288_h2_websocket_extended_connect_echo`・`test_f288_h2c_websocket_extended_connect_echo`（HTTP/2 の拡張 CONNECT）が
エコーを受け取れずに時間切れになった。Linux の io_uring・epoll では通る。

## 原因

reactor の kqueue は、起床のイベントの `EVFILT_READ` の量を fd ごとのヒント（`read_hint`）に残し、`TcpStream::readable` は
ヒントがあれば確認の `poll(2)` を省いて `Ready` を返す（F-141）。ヒントは消費されるまで残るので、イベントの後に別の
読み取りがデータを読み切っていても、次の `readable` は読めないのに `Ready` を返す（`executor::take_read_hint` の doc は
「直後の非ブロッキング I/O の `WouldBlock` で気付く」前提と書いている）。

B-306 で書き直した中継（`websocket::websocket_relay`・F-288 の `ws_tunnel::relay`）は、読み取り可能を見てから
時間切れ（アイドルの上限、無ければ 1 時間）付きで `read` する。偽の `Ready` を受けた側の `read` で止まり、その間は
反対方向を中継しない。上流のエコーサーバはクライアントのフレームを待っているので、両側が止まる。

## 修正

- `TcpStream::readable_confirmed` を足した（reactor の unix はヒントを捨てて必ず `poll(2)` で確かめる。io_uring・WSAPoll は
  `readable` と同じ）。WebSocket の中継（HTTP/1.1・HTTP/2・HTTP/3）はこれで待つ。
- 他の `readable` の呼び出し側（読み取りの前の待ちで、読み取りのほかに待つものが無い）は従来のまま。

## 確認

- macOS の E2E（`VEIL_E2E_NO_DEFAULT_FEATURES=1 VEIL_E2E_FEATURES=full-macos ./tests/e2e_setup.sh test`）で上の 3 件が通ること。
- FreeBSD・OpenBSD・NetBSD の VM の E2E。

## 関連

- B-306（中継の書き直し）、F-288（拡張 CONNECT の中継）、F-141（kqueue のヒント）
