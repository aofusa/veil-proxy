# B-196: veil-kit の GraphQL が GET で mutation を実行し、POST の Content-Type を見ない

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の API・Wasm のレビュー（docs/artifacts/v0100/review-api-wasm.md） #2（2026-10-26）。

## 事象

- `veil_kit::graphql` は GET のクエリ文字列の操作をそのまま実行していたため、mutation も GET で走った
  （SameSite=Lax の Cookie はトップレベルの GET に付くので、リンク 1 本で CSRF になる）。
- POST は Content-Type を見ずに本文を JSON として解析していたため、`text/plain` の単純要求（プリフライト無し）で
  別オリジンから mutation を送れた。

## 修正

- GET は query の操作だけを実行し、mutation（とサブスクリプション）は 405（`Allow: POST`）を返す
  （`parsed_query()` で操作の種類を見る）。
- POST は `application/json`・`application/graphql-response+json`・`application/graphql+json` だけを受け、それ以外は 415。
- `examples/api/rust-graphql` の同じ実装も同様に直した。

## 試験

- `crates/veil-kit/src/graphql.rs` の `get_runs_queries_only`・`post_requires_json`。

## 実施内容

- `crates/veil-kit/src/graphql.rs`、`examples/api/rust-graphql/src/graphql.rs`。
- 文書: `docs/guide/library-kit.md`・`docs/guide/library.md`（英日）。
