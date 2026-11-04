# B-266: HEAD の要求に、静的配信・固定応答・エラー応答が本文を付けて返す（HTTP/1.1 は次の応答がずれ、HTTP/2 は PROTOCOL_ERROR）

**状態: 修正済み（v0.10.1）**

## 出典

v0.10.0 の公開後、HttpArena の `static-tls compression` の SKIP を調べたとき、プロトコル間で同じ機能の挙動が違うところをまとめて洗い出した
（F-269 の提出の前）。v0.10.0 のソースのデバッグビルドを手元で立て、HTTP/1.1・h2c・HTTP/2（TLS）を curl と生のソケットで叩いて確かめた。
HTTP/3 は手元にクライアントが無いのでコードで確かめた。

## 事象

HEAD の応答は本文を持たない（RFC 9110 §9.3.2）。プロキシの経路は B-104・B-245 で対応済みだが、veil が自分で作る応答は HEAD を見ていない。

| 応答 | HTTP/1.1 | HTTP/2 | HTTP/3 |
|---|---|---|---|
| `type = "File"`（`handle_sendfile`・`h2_sendfile`・`handle_sendfile` (H3)） | **本文を送る**（実機） | **DATA を送る**（実機） | **本文を送る**（コード） |
| `mode = "memory"`（`Backend::MemoryFile`） | **本文を送る**（実機） | **DATA を送る**（実機） | **本文を送る**（コード） |
| `type = "Respond"` | 正しい（`h1.rs` が HEAD で本文を切る） | **DATA を送る**（実機） | **本文を送る**（コード） |
| `type = "Redirect"` | 正しい（本文 0） | 正しい | 本文 0 |
| 404 などのエラー応答（`h2_emit_error`・`send_error_response`） | 正しい（本文 0） | **`Not Found` を DATA で送る**（実機） | **本文を送る**（コード） |

- HTTP/1.1: `handle_sendfile` は要求のメソッドを受け取らず、`MemoryFile` の分岐もヘッダの後に本文を書く。HEAD の後に同じ接続で次の要求を送ると、
  クライアントは HEAD の応答の本文を次の応答の先頭として読む（`HEAD /static/a.txt` → `GET /static/index.html` で、2 つ目の応答の位置に
  `hello world …` が来た）。
- HTTP/2: curl は `HTTP/2 stream 1 was not closed cleanly: PROTOCOL_ERROR`（終了コード 92）で失敗する（HEAD の応答に DATA があるのは不正）。
- HTTP/3: `RequestTask::send_response` は本文があれば `RespMsg::Body` を送る。HEAD を見るのはプロキシのストリーミング（`no_response_body`）だけ。

## 影響

- P1（静的配信・`Respond` のルートへの HEAD。死活監視・リンクチェッカー・CDN の再検証で普通に来る）:
  - HTTP/1.1 の keep-alive・パイプラインで応答の区切りがずれる。前段のプロキシ・キャッシュが接続を使い回していると、別の要求の応答として
    本文が読まれる（応答の取り違え・キャッシュの汚染になり得る）。
  - HTTP/2・HTTP/3 では HEAD が失敗する（厳格なクライアントはストリームエラー、HTTP/2 の接続ごと落とすクライアントもある）。

## 修正方針

- HEAD かどうかを要求の文脈に 1 回だけ持たせ、ローカルの応答を送る箇所で本文だけを落とす（`Content-Length` は GET と同じ値を残す）。
  - HTTP/1.1: `handle_sendfile` に HEAD を渡し、ヘッダだけ書いて `sendfile`/`splice`/書き込みをしない。`MemoryFile` も同じ。
  - HTTP/2: `h2_emit_full`・`h2_emit_error` 等の出口で、HEAD なら HEADERS に END_STREAM を付けて DATA を送らない。
  - HTTP/3: `send_response`・`send_error_response` で HEAD なら `RespMsg::Body` を送らずに fin。
- 要求ごとの経路に確保を足さない（比較 1 回）。HEAD のときファイルを開く・読む・圧縮する処理も飛ばす。
- 試験: E2E で HTTP/1.1（平文・TLS・パイプライン）・h2c・HTTP/2・HTTP/3 の HEAD を File・memory・Respond・404 に送り、
  本文が無いこと・`Content-Length` が GET と同じこと・同じ接続の次の要求が正しく返ること。HTTP/2 は h2spec と同じ厳格さのクライアント
  （nghttp2）で、HTTP/3 は h2load ではなく本文の有無を見られるクライアントで確かめる。

## 修正（v0.10.1）

- 静的配信（File の sendfile・memory）の応答を `static_response::plan` 1 か所で決め、HTTP/1.1・HTTP/2・HTTP/3 で同じ形にする。HEAD は
  ヘッダだけ（`Content-Length` は GET と同じ値）を返し、本文を送らない。`Respond` の固定応答・ルートの無い要求等のエラー応答も HEAD には
  本文を付けない（HTTP/1.1 の keep-alive のずれ・HTTP/2 の PROTOCOL_ERROR が無くなる）。
- 統合試験 `b266_f286_f287_static_responses_match_across_protocols`・`b267_b271_header_policy_alt_svc_and_server_header_match_across_protocols`
  （4 プロトコルで HEAD の後ろに本文が来ないこと、keep-alive の次の要求が正しく返ること）。

## 関連

- [B-104](B-104-http3-upstream-no-keepalive.md)・[B-245](B-245-h1-head-204-304-upstream-body-wait.md)（プロキシの HEAD）、
  [B-188](B-188-wasm-local-response-head-body.md)（Wasm のローカル応答の HEAD）
- 同じ調査で起票: [B-267](B-267-response-header-policy-missing-on-proxy-paths.md)〜[B-274](B-274-route-client-timeouts-h1-only.md)、
  [F-285](../features/F-285-wasm-response-body-h1-h2.md)〜[F-288](../features/F-288-websocket-over-h2-h3.md)
