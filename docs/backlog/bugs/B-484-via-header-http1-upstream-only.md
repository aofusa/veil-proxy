# B-484: `[performance] via_header_enabled` が HTTP/1.1 の上流への要求にしか効かない

**状態: 修正済み（feat/protocol-parity。2026-11-21 起票・2026-11-22 修正。機能 × プロトコルの網羅性の確認）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md` の #4）。

## 事象

- `via_header_enabled = true` の `Via: 1.1 <via_header_hostname>` は、HTTP/1.1 のクライアントの要求を上流へ送るとき
  （`proxy::forward::write_upstream_request_headers`）だけ付く。HTTP/2・HTTP/3 のクライアントの要求（HTTP/1.1・h2c・h2 の
  上流とも）と WebSocket の upgrade には付かない。ループの検出・経路の調査に使えない。
- `examples/config.toml` は「リクエスト/レスポンスに Via ヘッダーを追加」「未指定ならシステムのホスト名」と書くが、
  実装は要求だけで、未指定のときは `veil`。ガイド（`configuration.md`）に項目が無い。

## 改修案

- 上流への要求を組み立てる共有の `write_h1_forward_headers`（HTTP/2・HTTP/3 → HTTP/1.1 の上流、WebSocket）と、HTTP/2 の
  上流へのヘッダ（`upstream_mux` へ渡す一覧）でも付ける。受けたプロトコルの版を入れる（RFC 9110 §7.6.3 の
  received-protocol: HTTP/1.1 は `1.1`、HTTP/2 は `2`、HTTP/3 は `3`）。
- 値は設定のロード時に作って持つ（要求ごとに `CURRENT_CONFIG` を読まない・確保しない）。
- 文書: 要求だけに付ける（応答には付けない）、既定のホスト名は `veil`。`configuration.md`（英日）に行を足す。

## 対応（2026-11-22）

- `Via` の値は設定の保存のとき（`init_via_header`。起動・リロード）にプロトコルごと（`1.1`・`2`・`3`）に作って持ち、
  要求ごとには参照カウントの複製だけにした（HTTP/1.1 の経路の要求ごとの `CURRENT_CONFIG.load()` も消えた）。
- HTTP/2・HTTP/3 の上流への要求のヘッダ（`AddedHeaders`。HTTP/1.1・h2c・h2 の上流、WebSocket のトンネル）に、
  受けたプロトコルの版で付ける。HTTP/1.1 は従来の位置で `1.1`。
- 設定の検証: `via_header_hostname` は空・トークンと `:`・`[]` 以外の文字を拒否する（ヘッダの注入を防ぐ）。
- 文書: `configuration.md`（英日）に 2 つのキーの行を足し、`examples/config.toml` を実装に合わせた（要求だけ、既定は `veil`）。
- 自己完結型 E2E: `b484_via_header_on_every_protocol`（4 経路の GET・本文を後から送る POST、HTTP/1.1 の upgrade・HTTP/2 の
  拡張 CONNECT で、`Via` が 1 つだけ付き、版が合う）。

## 受け入れ条件

- 単体・統合: `via_header_enabled = true` で HTTP/1.1・HTTP/2・HTTP/3 のクライアントの要求、WebSocket の upgrade の上流への要求に
  `Via` が 1 つ付き、版がクライアントのプロトコルに合う。無効なら付かない。

## 関連

- [B-232](B-232-x-forwarded-for-passthrough-default.md)（転送ヘッダの方針）
