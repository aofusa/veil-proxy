# B-493: HTTP/2 のクライアントへ中継する応答に `server` が 2 つ付く（`server_header_enabled` のとき）

**状態: 修正済み（feat/protocol-parity。2026-11-22 起票・修正。F-257 の作業で見つけた）**

## 事象

- `[server] server_header_enabled = true` のとき、HTTP/1.1（`append_own_server_line`）・HTTP/3（`send_h3_head_fin`）は
  上流の `server` を落として veil の `server` を付けるが、HTTP/2 の中継（`h2_relay_backend_response`・
  `h2_emit_upstream_response`・`h2_relay_h2_streaming`）は veil の `server` を足したうえで上流の `server` も写していた。
  HTTP/2 のクライアントにだけ `server` が 2 つ届き、上流のソフトウェアを隠す設定が HTTP/2 で効かなかった。

## 対応（2026-11-22）

- veil の `server` を付ける応答では、上流の `server` を写さない（HTTP/1.1 の上流の 3 つの経路の写しのループ、h2c・HTTPS の
  h2 の上流の見出しは `h2_append_base_headers` で）。`server_header_enabled = false` のときは従来どおり上流の値を 1 つ通す。

## 試験

- 自己完結型 E2E: `b493_single_server_header_on_every_protocol`（HTTP/1.1 の上流の流す経路・`buffering = "full"`・h2c の
  上流で、4 プロトコルとも `server` が veil の値 1 つ）。`f257_proxy_cache_on_every_protocol`・
  `b492_stale_responses_on_every_protocol` もキャッシュの当たり・stale で同じことを確かめる。

## 関連

- [F-257](../features/F-257-proxy-cache-h2-h3.md)
