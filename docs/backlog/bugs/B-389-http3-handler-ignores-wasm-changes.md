# B-389: HTTP/3 のネイティブハンドラのルートが Proxy-Wasm の書き換えたヘッダ・本文を捨てていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F12 から確認。

## 事象

- ハンドラのルートでも Proxy-Wasm の要求の段階は走り、書き換えたヘッダ・本文は `wasm_request_headers`・
  `wasm_request_body_override` に入る（B-317 は Proxy-Wasm を認可の層として扱う）。
- HTTP/3 の `Backend::Handler` の分岐は、QPACK で復号した元のヘッダと元の本文でハンドラの要求を作っていた。
  フィルタが上書き・削除したはずのヘッダ（`x-user-role: admin` 等）がハンドラに届く。HTTP/1.1・HTTP/2 は
  書き換えた値を渡していた（F-271）。

## 対応

- `http3_server::request_task::handler_request_parts` で、書き換えたヘッダ・本文があればそれを使う。本文を
  差し替えたら `content-length` も合わせる。`host` が無ければ `:authority` から足す（従来どおり）。

## テスト

- `http3_server::request_task::tests::handler_request_parts_use_wasm_changes`（`--features full,api`）。
