# B-293: HTTP/1.1 のプロキシの splice の経路が要求の本文の途中で失敗すると、通常の経路が同じ上流の接続へ要求の head を書き直す（要求の密輸）

**状態: 未修正（2026-11-05 起票。コードレビューで発見、再現は未実施）**

## 事象（コードの読み取り）

- `src/proxy/splice.rs` の `proxy_http_request_splice`（:299-331）は、上流へ要求の head・読み取り済みの本文・splice した本文の一部を書いたあと、
  本文が長さに届かない（クライアントが `READ_TIMEOUT`＝30 秒止まる、splice のエラー、上流の EOF）と `None` を返す。
- 呼び出し側（`src/proxy/forward.rs:934-953`）は `None` を「splice できなかった」とみなし、`proxy_http_request_with_compression`（`compress.rs:41-95`）で
  **同じ上流の接続へ head と最初の本文をもう一度書き**、残りの本文をクライアントから読む。
- 上流から見ると 1 つ目の要求の本文の途中に 2 つ目の要求の head が入り、区切りがずれる。本文を 30 秒止めてから再開するクライアントは、本文の残りを
  上流の別の要求として解釈させられる（veil のルーティング・JWT・レートリミットを通らない）。接続がプールへ戻ると他の利用者の要求とも混ざり得る。
- 以前は kTLS の接続だけだったが、[B-286](B-286-plaintext-h1-copies-file-and-proxy-bodies.md)（v0.10.1）で平文の HTTP/1.1 にも広がった。

## 影響

- P0 候補（要求の密輸・認可の迂回）。悪用には本文の途中で 30 秒止める等の操作が要るが、攻撃者が自分で制御できる。

## 改修案

- splice の経路は **上流へ 1 バイトでも書いたら `None` を返さない**。本文が届かなければ `Some((408 か 502, total, false, true))`（上流もクライアントも閉じる、
  プールへ戻さない）を返す。`None` は何も書く前（パイプが取れない、未消費データがある等）だけにする。
- 応答の段（`splice_transfer_response_ktls`）は既に `Some` だけを返すことを確認する。
- 回帰試験: 本文の途中で `READ_TIMEOUT` を超えて止めてから残りを送り、上流（模擬サーバ）が受け取る要求がちょうど 1 つで、接続がプールへ戻らないこと。
  平文・kTLS、io_uring・epoll。

## 関連

- [F-292](../features/F-292-ktls-h1-zero-copy-same-as-plain.md)、[B-286](B-286-plaintext-h1-copies-file-and-proxy-bodies.md)、[B-244](B-244-h1-chunked-timeout-upstream-conn-pooled-mid-response.md)（区切りどおりに読み切った接続だけ戻す）
