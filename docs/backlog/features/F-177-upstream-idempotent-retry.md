# F-177: プール接続が応答前に切れたとき冪等な要求を 1 回だけ再送する（B-93 の残り）

**状態: 完了（feat/v080-limitations）**

B-93 の生存確認はアイドル 1ms 以上の接続だけを対象にする（毎回確認すると syscall が増える）。
上流が応答直後に閉じ、その FIN がまだ届いていない 1ms 未満の窓で再利用すると 502 になる。

## 設計

冪等メソッド（GET / HEAD / OPTIONS / PUT / DELETE）で、プールから取り出した接続が「応答の 1 バイト目より前に
EOF / ECONNRESET / EPIPE」のときだけ、新規接続で 1 回だけ再送する（nginx の `proxy_next_upstream error` 相当）。
本文は保持している場合のみ。**既定で有効**。

## 結果（2026-10-09）

既定で有効（設定なし）。冪等メソッド（GET / HEAD / OPTIONS / PUT / DELETE / TRACE、`http_utils::is_idempotent_method`）で、
本文を受信し終えた要求だけを対象にする。

- HTTP/3 → HTTP/1.1 上流（ストリーミング・バッファ経路）: B-104 と同時に実装（応答 head を送る前の失敗）。
- 上流 HTTP/2（F-174）: REFUSED_STREAM・GOAWAY の範囲外は全メソッド、応答 head より前の接続断は冪等メソッドのみ。
- HTTP/2 → HTTP/1.1 上流（`h2_proxy_http` / `h2_proxy_https`）: プールの接続で応答の 1 バイト目より前に EOF / エラーなら
  下流へエラーを送らずに新しい接続で再送する（`h2_relay_backend_response` の `retry_on_empty`）。要求バッファは
  `write_all` が返すものを使い回す（コピーなし）。
- HTTP/1.1 → HTTP/1.1 上流（`proxy_http_pooled`、`proxy_https_pooled`）: 平文にも再送を追加し、HTTPS の既存の再送を
  冪等メソッドに限定した（POST 等は上流が処理済みかもしれないので再送しない。nginx の `proxy_next_upstream` の既定と同じ）。
  1 回目に送る複製は要求バッファのプールから取る（従来の HTTPS は `clone()` で毎回ヒープ確保していた）。
- E2E: `test_f177_idempotent_retry_on_dead_pooled_connection`（echo 上流が `Connection: close` なしで応答直後に閉じる。
  同じクライアント接続から続けて 30 回 GET）。旧実装は 2 回目で 502 になることを確認済み。
