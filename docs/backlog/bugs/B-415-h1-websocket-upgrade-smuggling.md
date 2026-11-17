# B-415: HTTP/1.1 の WebSocket の中継が、クライアントの `Upgrade` をそのまま上流へ送り、どの 101 でも生の中継を始めていた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-17）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「HTTP/1.1 WebSocket path forwards the client's Upgrade value verbatim and tunnels on any upstream 101」から確認。

## 事象

- HTTP/1.1 の要求は `Upgrade` のどこかに `websocket` のトークンがあれば WebSocket の経路に入る（`h1.rs`）。
  `proxy::websocket::handle_websocket_proxy` は `Upgrade` の行を全部（`h2c` 等のほかのトークンも）と
  `HTTP2-Settings` をそのまま上流へ写していた。
- 上流の応答は `101` でありさえすれば、切り替えた先のプロトコルも `Sec-WebSocket-Accept` も見ずに生の双方向の
  中継を始めていた（HTTP/2・HTTP/3 の拡張 CONNECT の中継 `ws_tunnel` は `Upgrade` と Accept を確かめている）。
- `h2c` を `HTTP2-Settings` の `Connection` 無しでも受ける上流、またはクライアントが並べたほかのトークンに 101 を
  返す上流では、ルートの検査（パスの照合・`[route.security]`・レート制限・Wasm・JWT）の外で上流へ生の HTTP を
  送れる通り道になった。

## 対応

- 上流へは `Connection: Upgrade` と `Upgrade: websocket` だけを送り、クライアントの `Upgrade`・`HTTP2-Settings`・
  `Connection` に挙がったヘッダは送らない。
- 上流の `101` は、`Upgrade` に `websocket` があり、`Sec-WebSocket-Accept` が 1 つでクライアントの
  `Sec-WebSocket-Key` から求めた値と一致するときだけ中継する。ほかの `101` は 502。
- `tls_provider::sha1_legacy` を feature を問わずにコンパイルする（HTTP/1.1 の経路で使うため）。
- 利用者ガイド（`routing.md` の英日）に書いた。

## テスト

- `proxy::websocket::tests::accept_key_matches_rfc6455_example`・
  `proxy::websocket::tests::only_a_websocket_101_with_the_right_accept_is_relayed`。
- WebSocket の E2E（実際の上流との 101）は従来どおり通る。
