# F-284: クライアントの TLS 接続を閉じるときに close_notify を送る（接続の終わりで区切る応答を厳格なクライアントが受け取れるようにする）

**状態: 未着手（2026-10-27 起票）**

## 今の状態

- veil はクライアントの TLS 接続を閉じるとき close_notify を送らない（`KtlsServerStream`・`SimpleTlsServerStream` の `shutdown` は何もしない）。
- ストリーミングの経路で、上流が接続を閉じて本文を終える応答（`Content-Length` も chunked も無い）を HTTP/1.1 のクライアントへそのまま流すと、
  本文の終わりは接続を閉じることでしか伝わらない。close_notify が無いので、厳格な TLS クライアント（rustls・hyper 等）は切り詰めとして誤りにする
  （curl は通す）。上流が close_notify を送って正しく終えた応答も、切れた応答も、クライアントからは見分けられない。
- バッファ経路は B-262 で `Content-Length` を付けて解消した。HTTP/2・HTTP/3 は本文の終わりをフレームで伝えるので問題無い。

## 改修内容

- 接続を閉じる前に close_notify を送る: rustls は `send_close_notify` + 書き出し、kTLS は `sendmsg` の `TLS_SET_RECORD_TYPE`（アラート 21）で送る。
  どちらも閉じる経路だけ（要求ごとの経路に syscall を足さない）。
- 代案: HTTP/1.1 のクライアントへは接続の終わりで区切る応答を chunked に詰め直し、上流が close_notify 無しで切れたら終端のチャンクを送らずに閉じる
  （切り詰めをクライアントが検出できる）。

## 受け入れ条件

- E2E: ストリーミングのルートで上流が close_notify 付きで閉じた応答を、rustls のクライアントが誤り無しで全文受け取る（既定と kTLS の構成）。
  close_notify 無しで切れた応答はクライアントが不完全と分かる。

## 関連

- [B-262](../bugs/B-262-h1-buffered-close-delimited-response-never-ends.md)、[B-261](../bugs/B-261-h3-ktls-upstream-control-record-and-close-notify.md)、
  [F-282](F-282-security-review-followup-hardening.md)（KT-01）
