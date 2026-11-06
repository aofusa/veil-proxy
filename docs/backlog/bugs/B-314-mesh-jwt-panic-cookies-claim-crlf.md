# B-314: メッシュの JWT 認証: 接頭辞の境界の多バイト文字で panic・`from_cookies` を読まない・クレームの CR/LF をヘッダに書く

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F4・F27・F53）で判明。どれもメッシュ（`mesh` feature）の
`RequestAuthentication`（Envoy の `jwt_authn`）の実装 `src/mesh/jwt.rs`。

1. **認証なしで遠隔からワーカーを落とせる（F4）。** `token_from` が Authorization 等の値を文字列にしてから
   `s[..prefix.len()]`（既定の接頭辞 `Bearer ` は 7 バイト）で切っていた。その位置に多バイト文字がまたがる値
   （例: `Beare` + `あ` + …）で panic する。メッシュの接続タスクは panic を止めずに spawn していたので、
   ワーカーのスレッドとその上の全接続が落ちる。数回の要求でサイドカー・ゲートウェイ全体が止まる。
2. **Cookie のトークンを検証しない（F53）。** 設定の `from_cookies` を、既定の取り出し場所（Authorization・
   `access_token`）を外す判断にだけ使い、Cookie を読んでいなかった。Cookie だけのプロバイダではトークンが常に
   「無い」扱いになり、`allow_missing` と組むと偽造・期限切れのトークンでも検証されずにアプリへ届いた。
3. **クレームの CR/LF を上流のヘッダに書く（F27）。** `claim_to_headers` がトークンの文字列のクレーム（JSON の
   エスケープで `\r\n` を含められる）をそのままヘッダにし、HTTP/1.1 の上流への要求の頭にそのまま書いていた。
   正しく署名されたトークンを持つ利用者が、上流へヘッダ・要求を注入できる。

## 修正

1. 接頭辞はバイト列で大文字小文字を区別せずに比べ、残りだけを文字列にする。あわせてメッシュの接続タスク
   （accept・HBONE の内部接続）と HTTP/2 のストリームのタスクを `spawn_*_with_panic_catch`（`CatchUnwindFuture`）で
   spawn する（データプレーンの accept と同じ）。
2. `from_cookies` の名前を Cookie ヘッダから取り出し、ヘッダのトークンと同じく検証する。
3. フィールド値として正しくない値（CR・LF・NUL・制御文字・前後の空白。`is_valid_h2_field_value`）のクレームは
   ヘッダにしない。

単体試験 `mesh::jwt::tests::{multibyte_at_prefix_boundary_does_not_panic, from_cookies_token_is_verified,
cookie_value_parsing, claim_with_crlf_is_not_copied_to_headers}`（`--features full,mesh`）。

## 関連

- docs/artifacts/security/v0110-claude-security/（F4・F27・F53）
