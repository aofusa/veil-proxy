# B-212: 既定の features・`wasm` 単体・`veil-proxy-kit` の `jwt` 単体のビルドが警告（dead code）・未定義で通らない

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の警告の検査（F-276 1.1、feature の組み合わせごとの `cargo clippy -- -D warnings`。2026-10-27）。

## 事象

- 既定の features（`handler` 無し）: `H2RequestCtx::tls`（B-204）を読むのがハンドラだけで、`field tls is never read`。
- `--no-default-features --features wasm`: Wasm の本文フェーズ（F-271）とローカル応答が使う `decode_chunked_body`・
  `status_reason_phrase` が `http2` のときだけ定義されていて、コンパイルできない。
- `veil-proxy-kit` の `--no-default-features --features jwt`: `crypto::hmac_sha256` を使うのは cookie だけで、dead code。

## 修正

- `H2RequestCtx::tls`・`ProxyH2Service::tls`・`H2Service::tls` を `cfg(feature = "handler")` にする。
- `decode_chunked_body`・`status_reason_phrase` を `http2`・`handler`・`wasm` のどれかで定義する。
- `hmac_sha256` を `cfg(feature = "cookie")` にする。

## 試験

- `docs/artifacts/v0100/warnings/sweep.sh` の全組み合わせが rc=0（`docs/artifacts/v0100/warnings.md`）。
