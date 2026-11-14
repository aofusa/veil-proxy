# B-394: メッシュの要求のミラーがクライアントのヘッダをそのまま送り、本線で消す識別のヘッダ・トークンが届いていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F28・F29 から確認。

## 事象

- `mesh::mirror::spawn_h1` は呼び出し側（`http1.rs`・`h2.rs`）から下流の生のヘッダを受け、`Host`・疑似ヘッダ・
  ホップバイホップだけを除いて写していた。本線の `l7::upstream_request_headers` を通らないので、XFCC の
  SANITIZE・SANITIZE_SET、Linkerd の `l5d-client-id` の除去、JWT の `forward = false` のトークンの除去、ルートの
  `request_headers_to_remove` がミラーでは効かず、クライアントが送った識別のヘッダが影のクラスタに届いた
  （Envoy は整えた後のヘッダから影の要求を作る）。

## 対応

- ミラーのヘッダは `mirror_headers`（= `l7::upstream_request_headers(ctx, f, headers, 1, false, chunked)`）で作る。
  送るミラーがあるときだけ 1 回作り、要求の先頭は `mirror_head` で組み立てる。

## テスト

- `mesh::mirror::tests::mirror_sends_sanitized_headers`（`--features full,mesh-istio,mesh-linkerd`）。
- kind での E2E は回していない。
