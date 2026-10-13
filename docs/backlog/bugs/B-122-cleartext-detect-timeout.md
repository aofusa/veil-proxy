# B-122: 平文リスナーが最初のバイトを 200ms しか待たず、HTTP/1.1 のクライアントへ HTTP/2 の GOAWAY を返していた

**状態: 完了（feat/api-bench-axum-fastapi。API ライブラリのベンチマーク中に発見）**

## 事象

`[server].listen_cleartext` に接続し、0.5 秒以上たってから `GET` を送ると、HTTP/1.1 の応答ではなく
HTTP/2 の GOAWAY(PROTOCOL_ERROR) の 17 バイトが返って切られた。ブラウザの事前接続（preconnect）や接続プールは
接続してから要求を送るまで間を空けることがあるので、平文 HTTP/1.1 の利用者全般（プロキシ・API ライブラリ）に影響する。
あわせて、何も送らずに閉じる接続（Kubernetes の `tcpSocket` プローブ・wrk の終了時など）のたびに
`WARN [Cleartext] Unknown protocol from ...` が出ていた（相手が任意に発生させられる接続ごとの WARN）。

## 原因

`proxy::detect_protocol_with_buffer`（MSG_PEEK でプロトコルを判別）が、最初のバイトの到着を含めて 200ms で打ち切り、
1 バイトも届いていなくても `Unknown` を返していた。`Unknown` は h2c の不正プリフェースとして GOAWAY を返す。
F-200 以前は、この判別を通るのは旧 h2c リスナー（`h2c_http1 = true` のときの HTTP/1.1 を含む）だけで、平文の HTTP/1.1
（旧 `http`）は判別を通らなかった。F-200 で平文の HTTP/1.1・h2c を `listen_cleartext` の 1 本にまとめたため、
平文 HTTP/1.1 のすべての接続が対象になった。

また、判別に必要なバイト数（TLS・HTTP/1.1 は 5、h2c のプリフェースは 24）に満たない断片が届いたときは、
`readable()` が覗き見のぶん即座に成立し続けるので、200ms の間 `recv(MSG_PEEK)` を空回りしていた。

## 修正

- 最初のバイトまでは、キープアライブで次の要求を待つのと同じアイドル待ち（`timeout_idle(IDLE_TIMEOUT, true, ..)`。
  60 秒、graceful shutdown で即座に満了。F-187）にした。
- 判別に足りない断片の続きは 200ms を上限に、1ms の sleep で覗き直す（空回りしない）。HTTP/2 プリフェースの途中
  （`PRI * ...` の 24 バイト未満）も続きを待つ。
- 1 バイトも届かずに閉じた・満了した接続は新しい `ProtocolType::Closed` として何も送らずに閉じ、ログは debug。
  判別できないバイト列（`Unknown`）のログも debug にした（従来どおり GOAWAY を返して閉じる）。

## 試験

- `tests/standalone_e2e.rs` の `b122_cleartext_waits_for_late_and_fragmented_first_bytes`: 接続 1 秒後の要求・3 バイトの断片の
  あとの続き・何も送らない接続 5 本のあとの要求がすべて 200、ログに `Unknown protocol` が出ない。修正前のコードでは
  1 つ目（遅れた要求）で失敗することを確認。
- 回帰: lib 1053・integration 60・standalone_e2e 16（io_uring / epoll）・E2E 565（io_uring / epoll）が通過。
