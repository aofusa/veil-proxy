# B-439: JWT の例がクレームを写すヘッダをクライアントの送った値を消さずに足し、X-User-ID 等を偽れた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「JWT filter appends claim-derived identity headers without removing client-supplied copies, so any client can spoof X-User-ID / X-User-Roles to the upstream」から確認。

## 事象

- examples/wasm-filters/jwt-filter/src/lib.rs の `add_claims_headers` は検証したクレームを `add_http_request_header` で足すだけで、クライアントが送った同じ名前のヘッダを消していなかった。
- 自分のトークンと `X-User-ID: alice`・`X-User-Roles: admin` を送ると、上流は攻撃者の値を先頭の値（トークンにクレームが無ければ唯一の値）として受け取った（Go の `Header.Get` 等は先頭の値を読む）。`skip_paths` の要求ではそのまま届いた。

## 対応

- すべての要求で、認証を飛ばすかを決める前に `claims_to_headers` に挙げたヘッダを取り除き（取り除けなければ 500）、検証した値は置き換え（`proxy_replace_header_map_value`）で入れる。README に書いた。

## テスト

- `cargo test`（`examples/wasm-filters/jwt-filter`）。ヘッダの書き換えはホストが要るので単体試験は無い（ホストの置き換え・削除は同じ名前のヘッダを全部消す。`src/wasm/host/headers.rs`）。
