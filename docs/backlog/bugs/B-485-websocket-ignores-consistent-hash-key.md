# B-485: WebSocket の上流の選択が Consistent Hash の `header:`・`cookie:` キーを見ない

**状態: 未着手（2026-11-21 起票。機能 × プロトコルの網羅性の確認、feat/protocol-parity）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md` の #5）。

## 事象

- `algorithm = "consistent_hash"` + `hash_key = "header:X-User"`・`"cookie:session"` の上流グループで、HTTP の要求は
  そのヘッダ・Cookie で上流を選ぶ（`select_with_header_fn`。HTTP/1.1・HTTP/2・HTTP/3）が、WebSocket は HTTP/1.1 の upgrade
  （`proxy/h1.rs`）も HTTP/2・HTTP/3 の拡張 CONNECT（`proxy::ws_tunnel`）も `select(client_ip)` で選ぶ。
- 同じセッションの HTTP の要求と WebSocket が別の上流へ行く（WebSocket の粘着が効かない。セッションを上流ごとに
  持つアプリで WebSocket が認証されない等）。クライアントの IP が変わらなければ同じでも、`header:`・`cookie:` を
  選んだ意味が無くなる。

## 改修案

- WebSocket の 3 つの経路でも `select_with_header_fn` で選ぶ（HTTP/1.1 は upgrade の要求ヘッダ、拡張 CONNECT は
  ストリームのヘッダ。Wasm がヘッダを書き換えたらその後のヘッダ）。他のアルゴリズムはヘッダを走査しない（今と同じ）。

## 受け入れ条件

- 単体: `cookie:` キーの Consistent Hash で、同じ Cookie の HTTP の要求と WebSocket（3 プロトコル）が同じ上流を選ぶ。

## 関連

- [F-288](../features/F-288-websocket-over-h2-h3.md)
