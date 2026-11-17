# B-418: ライブラリの `Json<T>`（と C の本文のスキーマ・Python の本文）が Content-Type を見ずに本文を JSON として読み、別サイトの text/plain のフォームで JSON のハンドラを動かせた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Json<T> body extraction ignores Content-Type, so text/plain cross-site requests can drive JSON endpoints (CSRF)」から確認。

## 事象

- `#[veil::route]` の `Json<T>` の引数が使う `api::extract::json_body` は、要求の Content-Type を見ずに本文を
  `serde_json` で読んでいた。CORS の事前確認の要らない別サイトからの要求（`enctype="text/plain"` のフォーム、
  型の無い Blob・`ArrayBuffer` を送る `fetch`・`navigator.sendBeacon`）は、利用者の Cookie（SameSite の無い
  `CookieJar` の Cookie）や Basic 認証の資格情報を付けて届き、JSON の形の本文で状態を変えるハンドラを動かせた
  （FastAPI の CVE-2021-32677 と同じ）。同じライブラリの GraphQL は JSON 以外を 415 で断っている。
- 同じ弱さが C の `veil_register_route_ex2` の `body_schema_json`（スキーマに合えばハンドラへ渡す）と、Python の
  本文の束縛（`Body`・pydantic のモデル・`body_constraints`）にもあった。

## 対応

- Rust の `Json<T>`: Content-Type が `application/json` か `application/*+json`（引数は見ない）でなければ、無い
  ときも含めて 415（axum の `Json` と同じ）。判定は `api::extract::is_json_content_type`。
- C の本文のスキーマを付けたルート: 同じ判定で 415。
- Python: FastAPI と同じく、Content-Type が付いていて JSON でなければ 415。無いときは従来どおり JSON として読む
  （FastAPI の互換のため。型の無い Blob の `fetch` は FastAPI と同じく残る）。
- 利用者ガイド（`library.md` の英日）、`veil.h`・`veil-capi` の README、例（`python-hello`）と試験の要求に
  Content-Type を付けた。

## テスト

- `api::extract::tests::json_body_requires_a_json_content_type`（JSON・`+json`・大文字・引数付きは通り、無い・
  text/plain・form・multipart・`application/jsonx`・`text/json` は 415）。
- `crates/veil-python/tests/test_binding.py` の `test_body_requires_json_content_type_when_given`。
- `examples/api/c-kit/kit.c` の自己試験（型の無い要求は 415）。
- `tests/api/library_e2e.sh` の「POST text/plain 415」「C body schema form 415」。
