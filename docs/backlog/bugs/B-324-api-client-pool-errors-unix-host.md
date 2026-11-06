# B-324: API クライアントの方式を見ないプール・利用者へ返すエラーの詳細・ホスト名 unix の UDS 化

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F31・F43・F54）で判明。

- **F31**: `veil::api::Client` のプールの鍵に方式が無く、`http://H:P` で張った平文の接続（HTTP/1.1 のプール・h2c の
  多重化接続）を `https://H:P` の要求が使い、Authorization 等を平文で送り得た。
- **F43**: `From<ClientError> for Response` がエラーの文をそのまま 502/504 の本文に入れ、URL のクエリ（API キー）・
  内部の `host:port`・OS のエラーを利用者へ返していた。
- **F54**: `http://unix/` はホスト名 `unix` の TCP として読まれ、接続先表記が `unix:80` になって接続で UDS
  （相対パス `80`）と取り違えた。`unix_sockets(false)`（既定）の検査を越えられた。

## 修正

- プールの鍵に方式（`https` / `h2c` / `http`）を入れる。
- 変換後の本文は `Bad Gateway` / `Gateway Timeout` だけにし、エラーの文の URL はクエリ・フラグメント・userinfo を外す。
- `ProxyTarget::parse` はホスト名 `unix`（大文字小文字を問わない）を拒否する（設定のバックエンドも同じ）。

## 試験

- `api::client::tests::b324_pool_scheme_unix_host_and_error_details`、`config::backend::tests::host_named_unix_is_rejected`。
- 文書: `docs/guide/library.md`・`library-kit.md`（英日）。
