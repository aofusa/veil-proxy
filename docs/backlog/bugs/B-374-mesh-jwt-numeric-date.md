# B-374: メッシュの jwt_authn が小数・文字列の `exp`・`nbf` を無いものとして扱っていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx のセキュリティスキャン（`CLAUDE-SECURITY-20261113-030705` の F8。LOW）で判明。

## 事象

B-322 で TOML の `[route.jwt]`（`jwt_verify::verify`）は直したが、メッシュの `mesh::jwt::verify_token` は `exp`・`nbf` を
`as_u64()` で読んだままだった。小数（RFC 7519 の NumericDate は小数を取り得る。PyJWT の `time.time()` 等）・文字列・負の値は
`None` になり、`exp` が無いのと同じに扱う。`require_expiration = false`（xDS の既定）では期限の切れたトークンを
いつまでも通していた。`exp + skew` は桁あふれも見ていなかった。

## 対応

- `jwt_verify::times_ok` に `exp`・`nbf` の検査をまとめ、TOML とメッシュの両方で使う（小数は読む。あるのに負でない数で
  ないものは拒否。加算は飽和）。

## テスト

- `mesh::jwt::tests::numeric_dates_fractional_and_invalid`（`jwt_verify` の同名の試験と同じ組）。
- 文書: `docs/guide/mesh.md`（英日）。
