# B-436: WAF の例が UTF-8 として読めない要求の本文を検査せずに通していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「WAF skips body inspection, and so lets the request through, whenever the body is not valid UTF-8」から確認。

## 事象

- examples/wasm-filters/waf-filter/src/lib.rs の `on_http_request_body` は、本文を `String::from_utf8` で読めたときだけ `RuleEngine::inspect` へ渡し、読めなければ何も検査せずに通していた。
- `inspect_body = true` の構成で、SQLi 等の本文の末尾に 0xFF を 1 バイト足すだけで本文のルールを全部外せた。バイナリのファイルを含む multipart も検査されず、隣のテキストの欄に攻撃を置けた。

## 対応

- 本文は `String::from_utf8_lossy` で読み（不正なバイトは U+FFFD）、常に検査する。README に書いた。

## テスト

- 既存の `cargo test`（`examples/wasm-filters/waf-filter`）。本文の経路は Proxy-Wasm のホストが要るので単体試験は無い（読み方の変更だけ）。
