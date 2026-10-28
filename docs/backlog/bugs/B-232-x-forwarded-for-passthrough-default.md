# B-232: 既定で `X-Forwarded-For`・`X-Real-IP`・`X-Forwarded-Proto` をクライアントから素通しし、veil 自身は付け直さない

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/proxy/mod.rs::write_h1_forward_headers`・`expanded_added_headers`・`examples/config.toml` の読解）。

## 事象

- 上流へ転送する要求ヘッダは、擬似ヘッダ・hop-by-hop・`content-length`・`transfer-encoding`・`host`・`expect` を落とす以外はそのまま送る。
  クライアントが付けた `X-Forwarded-For`・`X-Real-IP`・`X-Forwarded-Proto` もそのまま上流へ届く。
- veil は既定ではこれらを付けない（`[route.security].add_request_headers` に `$client_ip` を書いたときだけ付く）。
- 上流がこれらを信頼する構成（アプリのアクセス制御・レートリミット・監査ログ）では、クライアントが送信元 IP と scheme を偽装できる。
  「信頼プロキシ」の設定の無いリバースプロキシの既定としては危険側（nginx も素通しだが、`proxy_set_header` を書く運用が定着している）。
- メッシュ側（B-137）では leftmost の XFF を使わないよう直した経緯がある。

## 改修案

- 既定で `X-Forwarded-For` にクライアントの IP を**追記**（既存の値があれば `, ` で連結）し、`X-Real-IP`・`X-Forwarded-Proto` は veil の値で
  上書きする。`[route.security] trusted_proxies`（CIDR の一覧）に入る送信元からの値だけ保持する。ホットパスの確保を増やさない
  （追記はヘッダ書き出しのバッファへ直接）。
- 互換性のため `[server] forwarded_headers = "passthrough" | "append"（既定）| "strip"` のような切り替えを用意する。
- `docs/guide/`（英・日）・`examples/config.toml` に推奨設定（上流が送信元 IP を使う場合）を明記する。

## 受け入れ条件

- E2E: クライアントが `X-Forwarded-For: 1.2.3.4` を送っても、上流には `1.2.3.4, <client>` が届く（`strip` では `<client>` だけ）。
- 性能の交互 A/B（HTTP/1.1・HTTP/2 の 3B）が変わらない。

## 修正（v0.10.0）

- `[route.security] forwarded_headers`（`"append"` 既定・`"passthrough"`）と `trusted_proxies`（IP/CIDR、初回に解析して `OnceLock` に持つ）を足した。
  - append: `trusted_proxies` に入らない送信元の `X-Forwarded-For`・`X-Real-IP`・`X-Forwarded-Proto`・`Forwarded` は上流へ送らず、`X-Forwarded-For` にクライアントの IP を入れる。`trusted_proxies` からは受けた値を残し、クライアントの IP を `X-Forwarded-For` の別の行として足す（RFC 9110 のリストとしては末尾に連なる）。nginx・Envoy と同じく `X-Real-IP` は既定で足さない（移行の比較試験もそろう）。`add_request_headers` に `X-Forwarded-For` があればそちらを使う。
  - passthrough: 以前の動き。
- 実装: `expanded_added_headers` が返す型を `AddedHeaders`（利用者の追加 + `ForwardedValues`）にし、受けたヘッダを送るかを `keeps`、足すヘッダを `iter` で引く。クライアントの IP はスタック（`IpStr::from_ip_text`）に写すだけで要求ごとに確保しない。HTTP/1.1 の要求の組み立て（forward.rs）・HTTP/2/HTTP/3 → HTTP/1.1（`write_h1_forward_headers`）・上流 h2c/HTTPS h2（多重化）・HTTP/3 の上流 HTTP/2・WebSocket のすべてに当てた。
- 試験: `proxy::path_tests::forwarded_headers_policy`、standalone E2E `b232_forwarded_headers_append_trusted_and_passthrough`（HTTP/1.1・h2c・TLS の HTTP/2 で、上流に届いたヘッダを見る）。
- `examples/config.toml`・`docs/guide/routing.md`・`security.md`・`migration-nginx.md`（英日）を更新。挙動の変更はリリースノートの移行の注意に書く。

## 関連

- [B-137](B-137-mesh-rbac-leftmost-xff.md)、[F-236](../features/F-236-claude-security-audit.md)
