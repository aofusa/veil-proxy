# F-284: クライアントの TLS 接続を閉じるときに close_notify を送る（接続の終わりで区切る応答を厳格なクライアントが受け取れるようにする）

**状態: 完了（2026-11-22、feat/protocol-parity。2026-10-27 起票。閉じるときの close_notify は B-401 で先に入った）**

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

## 対応

- 閉じるときに close_notify を送る部分は [B-401](../bugs/B-401-tls-close-notify.md)（2026-11-14）で入った（TLS のストリームの `Drop`。
  rustls は `send_close_notify_now`、kTLS はカーネルにアラートのレコードを作らせる）。上流が close_notify で終えた、接続の終わりで
  区切る応答は、それで close_notify で終わるようになった。
- 残っていたのは逆の向き: B-401 の後は、本文が途中で切れた応答（上流が区切りの前に止まった・失敗した、接続の終わりで区切る
  応答を TLS の上流が close_notify 無しで閉じた）の後も `Drop` が close_notify を送るので、接続の終わりで区切る応答の切り詰めが
  クライアントからは正しい終わりに見えた。
- サーバ側の TLS のストリーム（`KtlsServerStream`・`SimpleTlsServerStream`）に印（`skip_close_notify`、`Cell<bool>`）を足し、立って
  いれば `Drop` で close_notify を送らない（nginx も誤りで閉じるときは送らない）。印を立てるのは本文が区切りどおりに終わらなかった
  中継だけ: `transfer_response_body_rest`（平文・TLS の上流の HTTP/1.1 の中継・圧縮しない経路・溜める経路の大きい本文。
  `AsyncWriter::response_truncated`）、kTLS の splice の経路（`Content-Length`・chunked の未達と、接続の終わりで区切る本文の読み取りの
  誤り・タイムアウト）、上流の HTTP/2 から HTTP/1.1 への中継（RST_STREAM・宣言の長さとの不一致）。要求ごとの経路に足したのは
  本文が終わったときの分岐 1 つ（切れたときだけ `Cell` に書く）。
- HTTP/2・HTTP/3 のクライアントは本文の終わりをフレームで伝える（切れたらストリームをリセットする。B-86・B-261）ので関係しない。
  平文の HTTP/1.1 のクライアントには接続の終わりで区切る応答の切り詰めを伝える手段が無い（TCP の FIN だけ。chunked に詰め直す
  代案は入れていない）。

### 試験

- 単体 `proxy::transfer::eof_tests::until_close_body_needs_clean_eof`: 切れた応答だけ `response_truncated` を呼ぶ。
- E2E `protocols::test_f284_close_notify_follows_upstream_eof_body`（`/echo-upload-tls/*`、上流は `x-eof-body: clean|abrupt`）: 上流が
  close_notify で終えた応答は rustls のクライアントが close_notify で読み終え、close_notify 無しで切れた応答は `UnexpectedEof` になる
  （どちらも本文は全部届く）。`close_notify` を名前に含む E2E（B-401・F-282・F-284 の 6 件）は既定の構成で通る。
- 否定の確認: `skip_close_notify` を何もしない版では、切れた応答も close_notify で終わり、F-284 の試験が落ちた。
- clippy（`full`・`full,api`・kTLS の無い `--no-default-features --features http2`）。

## 関連

- [B-262](../bugs/B-262-h1-buffered-close-delimited-response-never-ends.md)、[B-261](../bugs/B-261-h3-ktls-upstream-control-record-and-close-notify.md)、
  [F-282](F-282-security-review-followup-hardening.md)（KT-01）
