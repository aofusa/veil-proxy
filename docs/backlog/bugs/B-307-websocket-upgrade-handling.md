# B-307: HTTP/1.1 の WebSocket のアップグレードの扱い（101 以外の応答の切り詰め・応答ヘッダの方針を通らない・Proxy 以外のルートで常に 400・`Connection` の解析・検査の緩さ）

**状態: 未修正（2026-11-05 起票。コードレビューで発見。行番号は f8dc80de）**

## 事象（コードの読み取り）

1. **101 以外の上流の応答を切り詰める**（`src/proxy/websocket.rs:262-263,416-418`）: ヘッダを読み終えるまでに受け取ったバイトだけを返して閉じる。本文が欠け、
   `100 Continue` 等の 1xx を最終の応答として扱う。
2. **応答ヘッダの方針を通らない**（`websocket.rs:237-242,392-397`）: 101（と拒否の応答）の head を上流のまま書く。`remove_response_headers`・`add_response_headers`・
   `Server`・CORS・Wasm の `on_response_headers` が効かない。`examples/config.toml:2115` は `remove_response_headers` が全プロトコルに効くと書いている。
3. **Proxy 以外のルートで常に 400**（`src/proxy/h1.rs:1262-1267`）: JWT の失敗が 401 でなく 400、fault の abort が設定のコードでなく 400、`Respond`・`Redirect` の
   ルートも 400。
4. **`Connection` の解析**（`h1.rs:430-446`）: 要求ごとの `to_ascii_lowercase()` の確保（ホットパス規則）、部分一致（`x-upgrade-hint` をアップグレードとみなす）、
   最初の `Connection` ヘッダしか見ない、`close, foo` を close と認識しない、`Upgrade` の値が `websocket` と完全一致でないと通らない。ハンドラの
   `upgrade_allowed`（:1274）も同じ判定を使い、`Request::upgrade_protocol` と食い違う。
5. **検査の緩さ**: GET 以外・HTTP/1.0 もアップグレードし、HTTP/1.0 を 1.1 に書き換える（`websocket.rs:66`）。`Connection` に挙げられたホップバイホップのヘッダを上流へ転送する。

## 改修案

1. 101 以外は通常のプロキシの経路で応答全体を返す（少なくとも Content-Length まで読む）。1xx は読み捨てる（B-11 と同じ）。
2. head を `forward.rs:1119` の書き換え（`render_response_head`）に通す。Wasm の応答ヘッダの段も呼ぶ。
3. WebSocket の分岐は `Backend::Proxy` のときだけにし、他は `handle_backend` へ（認可・fault の応答はそのまま返る）。
4. 全 `Connection` ヘッダをトークンに分けて大文字小文字を区別せず比べる（確保なし。`has_token` 相当）。ハンドラも同じ関数。
5. GET・HTTP/1.1 以外は 400（RFC 6455 §4.1）。`Connection` のトークンに挙がったヘッダを上流へ送らない。
- 試験: 上流が 403 + 本文・`100 Continue` → 101、`remove_response_headers` が 101 に効く、JWT のルートで 401、`Connection: keep-alive, Upgrade` と
  `Connection: x-upgrade-hint` の区別、HTTP/1.0 の拒否。

## 関連

- [B-306](B-306-websocket-relay-drops-data-iouring.md)、[F-288](../features/F-288-websocket-over-h2-h3.md)、[F-261](../features/F-261-handler-http11-upgrade.md)
