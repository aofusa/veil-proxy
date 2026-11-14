# B-387: 相対の転送先のリダイレクトが `//evil.example`（プロトコル相対）の `Location` を返せた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F23 から確認。

## 事象

- 既定の正規化（`base`）はスラッシュをまとめないので、`/legacy//evil.com` は `/legacy/*` のルートに一致し、接頭辞を
  除いた残りは `//evil.com` になる。B-322 は残りが `/` で始まることだけを確かめていた。
- `redirect_url = "/"` と `preserve_path = true` では `//evil.com`、`redirect_url = "$request_uri"` でも同じく
  プロトコル相対の `Location` になり、ブラウザは別のホストへ移る（オープンリダイレクト）。`\` も多くのブラウザで
  `/` と同じに扱われる。
- 変数は `str::replace` を 2 回重ねて置き換えていたので、差し込んだ値の中の `$path` が置き換え直されていた。

## 対応

- `http_utils::build_redirect_url` で、差し込むパスの先頭の `/`・`\` の並びを 1 つの `/` にまとめる
  （`single_leading_slash`）。変数は 1 回の走査で置き換える。設定そのものがプロトコル相対（`//`）でなければ、結果の
  先頭の並びも 1 つにする。
- 利用者ガイド（`routing.md`・`security.md` の英日）に書いた。

## テスト

- `http_utils::tests::redirect_never_becomes_protocol_relative`。
