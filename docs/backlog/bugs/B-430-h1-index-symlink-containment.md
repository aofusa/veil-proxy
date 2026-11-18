# B-430: HTTP/1.1 の静的配信がディレクトリの index のファイルに封じ込めの検査を掛けず、ルートの外を指すシンボリックリンクを返し得た

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「HTTP/1.1 static serving does not apply the root-containment check to a directory's index file on the
canonicalize() fallback path」から確認。

## 事象

- `proxy::sendfile::handle_sendfile` はディレクトリへの要求で、ディレクトリ自体にだけ `sendfile_base_contains` を
  当て、`<dir>/<index>` は `get_file_info_with_config` で解決した正規化のパスをそのまま返していた。
- 解決が `canonicalize()` に回る場合（openat2 の無い環境・Linux 以外・リロードで足したルート、Linux でも絶対の
  シンボリックリンクは B-89 で従来経路に回る）、`d/index.html` がルートの外のファイルを指すシンボリックリンクだと
  `GET /static/d/` がその中身を返し得た（直接の `/static/d/index.html` と HTTP/2 は 403）。

## 対応

- index のファイルにもディレクトリと同じ封じ込めの検査を掛け、外なら 403 にする。

## テスト

- 自己完結型 E2E `b430_index_symlink_outside_root_is_not_served`（HTTP/1.1・h2c とも返さない）。
