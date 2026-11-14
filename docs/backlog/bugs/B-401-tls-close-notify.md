# B-401: TLS の接続を close_notify 無しで閉じていたので、EOF まで読む OpenSSL 3 のクライアントがセッションを再開できなかった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

接続を張り直し続ける負荷（wrk に `Connection: close`、54KB、TLS。lab57）で veil は nginx の 0.43 倍だった
（3,931 rps 対 9,066 rps。サーバの命令数から、veil の接続の約 45% が完全なハンドシェイク）。その原因を調べる中で、
手元の Python（OpenSSL 3）で `Connection: close` の応答を EOF まで読んで接続を張り直すと、セッションの再開率は veil が 0%
（TLS 1.3・TLS 1.2 のチケット・TLS 1.2 の session ID のどれも）、`openssl s_server` は 98.6〜99.9% だった。
`suppress_ragged_eofs=False` で読むと、veil の接続の終わりは TLS 1.2・1.3 とも close_notify の無い EOF（ragged EOF）だった。

lab57 の差そのものは別の理由（負荷側の wrk の LibreSSL 2.7 の不具合）だった（下の「wrk の結果」）。

## 原因

veil は TLS の接続を閉じるときに close_notify を送っていなかった（RFC 8446 §6.1 の MUST 違反）。サーバの TLS ストリーム
（`KtlsServerStream`・`SimpleTlsServerStream`）も上流へのクライアントのストリーム（`KtlsClientStream`・
`SimpleTlsClientStream`・HTTP/3 の `TlsBackend`）も、`AsyncWriteRent::shutdown` は何もせず、`Drop` でソケットを閉じるだけ
だった。

OpenSSL 3 は close_notify 無しの EOF を読むと切り詰めの攻撃と見分けられないので、`SSL_R_UNEXPECTED_EOF_WHILE_READING` の
致命的な誤りとしてその接続のセッションを捨てる。次の接続は完全なハンドシェイクになる。応答を EOF まで読まないクライアント
（応答の長さで読み終える wrk 等）には効かない。rustls のクライアントはセッションを捨てないので、veil の E2E（rustls）では
見えなかった。

HTTP/2 の GOAWAY の後（`close_with_goaway`）は「送信側を閉じてから読み捨てる」（B-102）つもりで `shutdown()` を呼んでいたが、
TLS では何もしていなかった。HTTP/2・HTTP/3 の拡張 CONNECT の WebSocket のトンネルは、クライアントの END_STREAM で TLS の
上流の TCP だけを半分閉じていた（上流には切り詰めに見える）。

## 修正

- `tls_writev::send_close_notify_now`: rustls に close_notify を積み、`sendmsg(2)` の `MSG_DONTWAIT | MSG_NOSIGNAL`
  （macOS は `MSG_DONTWAIT`）で待たずに 1 回だけ書く。io_uring の CONNECT で作ったソケットはノンブロッキングとは限らず、
  veil は SIGPIPE を無視しないプロセスにライブラリとして組み込まれることがある。rustls は close_notify を 1 回しか積まず、
  致命的なアラートを送った後は積まない。macOS の送信は `MSG_DONTWAIT` を見ない（ブロッキングのソケットでは空きを待つ）が、
  macOS の veil のソケットは reactor が作るので常にノンブロッキング。
- `ktls::send_close_notify_alert`: kTLS で送信している接続は、`TLS_SET_RECORD_TYPE` の制御メッセージ（Linux は `SOL_TLS`、
  FreeBSD は `IPPROTO_TCP`）でカーネルにアラートのレコードを作らせる。2 回送らないようにストリームに印を持つ。
- サーバ・クライアントの TLS ストリームと `TlsBackend` の `Drop` で送る。HTTP/1.1 の `Connection: close`・キープアライブの
  満了・graceful shutdown・誤りの応答の後・上流のプールから捨てるときなど、どの経路で閉じても通る。接続ごとに `sendmsg(2)`
  が 1 本増える（要求ごとではない）。
- `AsyncWriteRent::shutdown`（HTTP/2 の GOAWAY の後）は close_notify を送り、送信バッファが空くのを 500ms まで待ってから
  TCP の送信側を閉じる（読み捨ては続けられる）。
- クライアントのストリームの `into_parts`（HTTP/3 の `TlsBackend` へ分解する）は `ManuallyDrop` で `Drop` を通さない
  （close_notify は `TlsBackend` が閉じるときに送る）。
- WebSocket のトンネルの半分閉じは `RelayIo::close_write`（TLS は close_notify を送ってから TCP の送信側を閉じる）。

## 確認

- 単体（`tls_writev::close_notify_tests`）: 送った close_notify を相手の rustls が「相手が閉じた」と読む・2 回目は送らない・
  ブロッキングのソケットの送信バッファが一杯でも戻る・相手が閉じていても戻る。
- E2E（`protocols::test_b401_*`）: `Connection: close` の応答（TLS 1.3・1.2）と 400 の応答の後、rustls のクライアントが
  `UnexpectedEof` でなく close_notify で読み終わる。修正前のコードでは 2 件とも `UnexpectedEof` で落ちる。io_uring・epoll・
  kTLS（`e2e_setup.sh test ktls`）で通る。
- 手元の Python（OpenSSL 3）、4 スレッド × 3 秒の張り直し: セッションの再開率は TLS 1.3 で 99.9%（チケットあり・なし）、
  TLS 1.2 で 98.6%（`openssl s_server` と同じ。修正前は 0%）。kTLS を有効にした構成でも同じで、`/proc/net/tls_stat` の
  `TlsTxSw` が増えた（カーネルの送信の経路でアラートを送った）。
- 上流: TLS の上流（Python）へ中継してから veil を止めると、上流は close_notify で閉じたと記録した（kTLS の設定の有無とも）。
- macOS（実機）: clippy（`full-macos`）・単体・自己完結型 E2E の HTTP/3 の 2 件。単体の「送信バッファが一杯でも待たない」は、
  macOS では veil のソケットと同じくノンブロッキングのソケットで確かめる（ブロッキングのままだと試験の詰め込みの送信が待つ）。

## wrk の結果（lab60・lab62、aofish のコンテナ間、54KB、100 接続、3 回の中央値）

lab57 の wrk（`williamyeh/wrk`。Alpine 3.8 + LibreSSL 2.7、TLS 1.2 のみ）は応答の長さで読み終え、EOF を読まずに自分の
close_notify を送って張り直すので、B-401 の前後で変わらなかった（lab60: 修正後 4,066 rps・修正前 4,255 rps・nginx 9,040 rps）。
この wrk は、再開した接続で新しいチケットを受けると、次の接続ではそのセッションで再開しない。veil（rustls）は Go の
crypto/tls と同じく TLS 1.2 の再開のたびにチケットを送り直すので、この wrk は 1 回おきにしか再開しない（F-183 の計測でも
見えていた）。nginx（OpenSSL）は再開の
ときにチケットを送り直さない。差は負荷側の CPU で、サーバの 1 要求あたりの CPU は veil の方が少なかった（lab60: veil 502 µs・
nginx 675 µs、wrk は veil 933 µs・nginx 466 µs で 4 スレッドを使い切っている）。

| 負荷 | nginx | veil（既定の `"tickets"`） | veil（`"cache"`） | サーバの CPU / 要求（nginx → veil） |
|---|---:|---:|---:|---|
| OpenSSL 3 の wrk（`local/wrk-openssl`、TLS 1.3） | 5,542 | **6,360（1.15×）** | | 874 → 464 µs |
| 同、再開しない（nginx は `ssl_session_tickets off`、veil は `"off"`） | 1,991 | **2,306（1.16×）** | | 1,270 → 661 µs |
| LibreSSL 2.7 の wrk（TLS 1.2） | 9,126 | 3,934（0.43×） | **11,076（1.21×）** | 581 → 519 / 247 µs |

`session_resumption = "cache"` は TLS 1.2 をチケット無しの session ID で再開するので、この wrk も毎回再開する。既定は変えない
（`"tickets"` は TLS 1.3 の再開にサーバの状態と共有のロックを持たず、Go のクライアントの TLS 1.2 はチケットでしか再開しない）。
古い TLS 1.2 クライアントに `"cache"` を使うことは、ガイド（`docs/guide/tls.md` の「TLS Session Resumption」の注記。F-183）に
書いてある。
