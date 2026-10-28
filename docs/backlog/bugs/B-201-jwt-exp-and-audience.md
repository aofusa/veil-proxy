# B-201: JWT: veil-kit は exp を必須にせず数値でない exp を黙って飛ばし、Python の veil_kit は audience を指定しないと aud を検査しない

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の API・Wasm のレビュー（docs/artifacts/v0100/review-api-wasm.md） #9、v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #7（2026-10-26）。

## 事象

- `veil_kit::auth::JwtVerifier` は `exp` の無いトークンを無期限として受け、`exp`・`nbf` が数値でないと検査を飛ばした。
- Python の `veil_kit.security.JWT` は `audience` を渡さないと PyJWT の `verify_aud` を切っていたため、別の受け手向けの
  （`aud` 付きの）トークンを受けた。

## 修正

- veil-kit: `exp` を既定で必須にし（無ければ `JwtError::MissingClaim("exp")`）、`JwtVerifier::require_exp(false)` で外せる。
  数値でない `exp`・`nbf` は Malformed。
- Python: `verify_aud` を常に有効にし（PyJWT は `audience` 未指定で `aud` 付きのトークンを拒否する）、
  `JWT(..., verify_audience=False)` で外せる。

## 試験

- `crates/veil-kit/src/auth/jwt.rs` の `exp_is_required_and_numeric`。
- `crates/veil-python-kit/tests/test_kit.py` の `TestJWTAudience`。

## 実施内容

- `crates/veil-kit/src/auth/jwt.rs`、`crates/veil-python-kit/src/veil_kit/security.py`。
- 文書: `docs/guide/library-kit.md`（英日）、`crates/veil-python/docs/kit.md`。
