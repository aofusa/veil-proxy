# B-204: ASGI の scheme がクライアントの X-Forwarded-Proto を無条件に信じ、TLS の HTTP/1.1・2 は http になる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #3（2026-10-26）。

## 事象

- `veil.asgi` は `scope["scheme"]` を `X-Forwarded-Proto` から決めており、どのクライアントのヘッダも信じた（https の判定・
  リダイレクト先・Cookie の Secure の判断を偽れる）。ヘッダが無いと HTTP/3 以外は `http` で、veil 自身が TLS で受けた
  HTTP/1.1・HTTP/2 も `http` になった。

## 修正

- 要求に TLS で受けたかの旗（`Request::is_tls()`、Python の `Request.is_tls`）を足し、HTTP/1.1 は TLS のストリーム、
  HTTP/2 は接続（`H2Service::tls`）、HTTP/3 は常に TLS から設定する。データプレーンでは bool 1 個のコピーだけ。
- `scheme` は既定でこの旗から決め、`X-Forwarded-Proto` は接続元が信頼するプロキシ（`ASGIApp(..., forwarded_allow_ips=...)`・
  `veil.asgi.run`・`App.mount` の引数、無ければ環境変数 `FORWARDED_ALLOW_IPS`、既定 `127.0.0.1`。`*` で全て、ネットワーク表記可）
  のときだけ使う（uvicorn と同じ既定）。

## 試験

- `crates/veil-python/tests/test_fastapi_compat.py` の `test_scheme_from_tls_and_trusted_proxies`。
- `cargo test --lib --features full,api -- api::` と library_e2e の python（HTTP/1.1・HTTP/2 の経路）。

## 実施内容

- `src/api/request.rs`、`src/api/dispatch.rs`、`src/proxy/h1.rs`、`src/proxy/h2.rs`、`src/proxy/detect.rs`、
  `crates/veil-python/src/native.rs`・`python/veil/`（`asgi.py`・`__init__.py`・`_veil.pyi`）。
- 文書: `crates/veil-python/docs/fastapi.md`・`requests-responses.md`、`docs/guide/library.md`（英日）。
