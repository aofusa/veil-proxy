# B-417: HTTP/3 の中継等が `head` を HEAD と見て上流の本文を読まず、本文の残った上流の接続をプールへ戻し得た

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「HTTP/3 streaming proxy treats any case variant of HEAD as bodiless but forwards the method verbatim」・
「HTTP/3 buffered proxy path has the same case-insensitive HEAD detection with verbatim method forwarding」から確認。

## 事象

- HTTP/3 の `:method` はトークンであることしか検査せず（`h3_fields.rs`）、上流の HTTP/1.1 の要求行へそのまま写す。
  一方で `http3_server::Http3Handler::classify`（ストリーミング）と `H3BufferedTask::handle_proxy`（バッファ経路）は
  `eq_ignore_ascii_case(b"HEAD")` で `no_response_body` を決めていた。
- メソッドは大文字小文字を区別する（RFC 9110 §9.1）。`head` を HEAD でないメソッドとして扱う上流（Go の net/http・
  h11 等）は本文を返すが、veil は `Content-Length: 0` として読まず、その接続をワーカーの共有プールへ戻し得た
  （平文で 1ms 以内の再利用・TLS で本文が遅れて届く場合は未読のデータでは捨てない）。次にその接続を使う別の利用者が
  残りの本文を自分の応答として読む（応答の取り違え・攻撃者が形を決めた応答の注入）。
- 同じ大文字小文字を区別しない判定が、HTTP/1.1 で受けて HTTP/2 の上流へ送る経路（`proxy::forward` の 3 箇所）、
  静的な応答（`static_response::StaticRequest::from_headers`）、キャッシュのキー（`cache::key::CacheableMethod`）にも
  あった（HTTP/1.1・HTTP/2 の中継の経路は `== b"HEAD"` で比べている）。キャッシュは `get` の要求の応答（上流は GET と
  別の応答を返し得る）を GET のキーに保存し得た。

## 対応

- 上の各箇所で `HEAD`・`GET` を大文字小文字を区別して比べる。`head` は本文のある未知のメソッドとして扱い、
  `get`・`head` の要求はキャッシュしない（`[cache] methods` の設定の照合は従来どおり大文字小文字を問わない）。

## テスト

- `cache::key::tests::method_is_case_sensitive`・`static_response::tests::head_flag`。
