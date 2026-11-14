# B-397: ACME のサーバが返した URL の CR・LF で、送る HTTP/1.1 の要求行へ別の要求を足せた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F32 から確認。

## 事象

- ディレクトリより後の URL（`newNonce`・`newAccount`・`newOrder`・`authorizations[]`・チャレンジ・`finalize`・
  `certificate`・注文の `Location`）はすべて ACME のサーバが決める。`acme::client` はそれを `HttpClient::request`
  へそのまま渡していた。
- `ctrl_http::parse_url` は最初の `/` より後ろを検査せずにパスにし、`request` は `format!` でそのまま要求行に
  書いていたので、CR・LF で別の要求（Redis 等の内部のサービスへのコマンド）を足せた。`https://` のディレクトリが
  `http://127.0.0.1:6379/...` を返すと、平文で内部の宛先へも送っていた。

## 対応

- `parse_url` は空白・制御文字（0x20 以下・0x7F）を含む URL を断る。`request` は名前・値に CR・LF・NUL を含む
  ヘッダを断る。
- `AcmeClient` はディレクトリを `https://` で取ったら、サーバが返す URL も `https://` に限る（`check_server_url`）。
- 利用者ガイド（`acme.md`・`ja`）に書いた。

## テスト

- `ctrl_http::tests::parse_urls`（追加の検査）・`acme::client::tests::server_urls_do_not_downgrade`。
