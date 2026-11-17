# B-420: HTTP/1.1 で受けて上流の h2c へ送る要求が、見出しと一緒に読めた本文だけを送り、残りの本文を次の要求として読んでいた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「HTTP/1.1 -> h2c upstream path forwards only the already-read prefix of the request body; the unread remainder is
parsed as the next request (request smuggling / desync)」から確認。

## 事象

- `proxy::forward` の h2c の経路（`use_h2c = true` のルート）は、HTTP/1.1 の接続の見出しと一緒に読めた本文
  （`initial_body`）だけを `proxy_h2c` → `upstream_mux::request_h2c_buffered` へ渡して END_STREAM にしていた。
  HTTPS の HTTP/2 の上流の経路のような「本文を受信済みか」の確認が無く、残りの本文はクライアントの接続から読まず、
  応答の後に接続をキープアライブへ戻していた。
- 残りの本文は HTTP/1.1 の接続の次の要求として読まれる（`Expect: 100-continue` なら最初の本文は空）。前段の
  キープアライブのプロキシの後ろでは、本文の中に隠した要求が前段の検査を通らずに届き、応答の列もずれる。
- `Transfer-Encoding: chunked` の要求は、チャンクのフレーミングごと DATA として送っていた。

## 対応

- h2c の経路で本文を受信し終えていなければ（chunked、または Content-Length に足りない）、残りを読んでから送る
  （`proxy::forward::read_h2c_request_body`。chunked は復号し、`Expect: 100-continue` には 100 を返す）。上限は
  Content-Length の要求が `max_request_body_size`、chunked が `max_chunked_body_size`、gRPC は `MAX_GRPC_BODY_SIZE`。
  超えれば 413、壊れた本文は 400 で閉じる。
- 本文の後ろまで読んだ分（次の要求）はこの経路から HTTP/1.1 の接続へ返せないので、応答の後に接続を閉じる。
- 利用者ガイド（`routing.md` の英日の H2C）に書いた。

## テスト

- E2E `test_b420_h2c_late_body_is_not_a_second_request`（見出しの 300ms 後に要求の形の本文を送り、応答が 1 つ
  （200）だけであること）。本文の読み取りは `proxy::h1::read_full_request_body` の既存の単体テスト。
