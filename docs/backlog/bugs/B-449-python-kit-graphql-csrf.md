# B-449: Python の kit の GraphQL が GET の mutation を実行し、POST は Content-Type を見ず、別のサイトから mutation を実行させられた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「GraphQL endpoint runs mutations sent by GET (cross-site request forgery through a link)」・
「GraphQL POST accepts any Content-Type, so cross-site text/plain form posts run mutations」から確認。

## 事象

- `veil_kit.graphql`（Python の kit）は GET のクエリ文字列の操作を種類を見ずに `schema.execute` へ渡していた。
  トップレベルの GET の遷移には `SameSite=Lax` の Cookie（veil の `set_cookie`・`veil_kit.sessions` の既定）が付くので、
  別のサイトのリンク・リダイレクトから利用者として mutation を実行できた。Rust の kit は B-196 で直していた。
- POST は Content-Type を見ずに本文を JSON として実行していた。`enctype="text/plain"` のフォームは CORS の事前確認の
  要らない要求で、JSON の形の本文を送れるので、SameSite の無い Cookie・Basic 認証等の資格情報で mutation を実行できた。
- `examples/api/python-graphql` も同じだった。

## 対応

- GET（と HEAD）は問い合わせと購読だけを実行し、mutation は `Allow: POST` 付きの 405、解析できない・操作を 1 つに
  選べない文書は 400（graphql-core で文書を解析して判定する）。
- POST は `Content-Type` が `application/json`・`application/graphql-response+json`・`application/graphql+json`
  （引数は見ない）のときだけ受け、他は 415。GET・POST 以外は 405。
- 例（`examples/api/python-graphql`）も同じ規則にした。利用者ガイド（`library.md` の英日）に書いた。

## テスト

- `crates/veil-python-kit/tests/test_kit.py` の `test_csrf_rules`。
- `tests/api/library_e2e.sh` の「kit GraphQL non-JSON is 415 (strawberry)」・「py GraphQL GET mutation is 405」。
