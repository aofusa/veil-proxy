# B-492: `stale_while_revalidate`・`stale_if_error` が効かない（期限の切れたエントリを消していた・HTTP/1.1 は応答を 2 つ送る）

**状態: 修正済み（feat/protocol-parity。2026-11-22 起票・修正。F-257 の作業で見つけた）**

## 事象

1. **期限の切れたエントリが残らない。** `CacheIndex::get` は期限の切れたエントリを引いたときに消し、60 秒ごとの掃除
   （`evict_expired`）も期限の切れたものを全部消していた。stale を返す側（`get_stale`）は期限の切れたエントリを探すので、
   `stale_while_revalidate`・`stale_if_error` は一度も期限の切れたエントリを返せなかった（期限内の当たりと区別できない
   試験しか無く、気づけなかった）。
2. **HTTP/1.1 の stale-if-error は応答を 2 つ送る。** 上流の 502・504 を中継した**後に**、保存した応答をクライアントへ続けて
   書いていた（1 つの要求に応答が 2 つ。キープアライブの接続では 2 つ目が次の要求の応答として読まれる）。veil 自身の 502・504
   （上流に接続できない・時間切れ）では stale を見なかった。
3. **HTTP/2・HTTP/3 は stale を見ない**（F-257 でキャッシュを効かせる前から、エラーの経路に stale の分岐が無かった）。
4. HTTP/3 のバッファ経路は、上流の時間切れも 502 にしていた（HTTP/1.1・HTTP/2 は 504）。

## 対応（2026-11-22）

- エントリに保持期間（`stale_retention_secs`）を持たせ、保存するときにルートの設定で決める（`stale_if_error` は 1 時間、
  `stale_while_revalidate` だけなら 60 秒、どちらも無ければ 0）。期限が切れても保持期間の内は `get` が消さず、掃除も残す
  （`get` は期限内のものだけを返すので、普通の引き当ては変わらない）。
- stale-if-error は、クライアントへ何かを書く前に決める。veil の 502・504（接続できない・時間切れ・壊れた応答・健全な
  サーバが無い）と、上流が返した 502・504 の両方で、保存した URL の期限の切れたエントリ（期限から 1 時間まで）を返す
  （`X-Cache: STALE`）。無ければ従来どおりのエラー。HTTP/1.1（平文・HTTPS の上流・h2c・HTTPS の h2 の上流・`buffering`）・
  HTTP/2（全上流）・HTTP/3（流す経路・溜める経路）で同じ（`cache_lookup::stale_fallback` を共有）。veil のエラーの代わりに
  返した HTTP/1.1 の接続は、従来のエラーと同じく閉じる。
- 上流の 502・504 を中継してから stale を書く HTTP/1.1 の後追いの分岐は消した。
- HTTP/3 のバッファ経路は、上流の時間切れを 504 にした。
- 裏の取り直し（stale-while-revalidate）は、HTTP/2・HTTP/3 の要求からも行い、TLS の上流にも接続する（F-257）。

## 試験

- 単体: `expired_entries_are_kept_within_stale_retention`（保持期間の内は `get`・掃除で消えず `get_stale` で引ける。
  保持期間の無いものは消える）、`store_async_uses_route_policy`（保持期間）。
- 自己完結型 E2E: `b492_stale_responses_on_every_protocol`（HTTP/1.1・h2c・HTTP/2（TLS）・HTTP/3 のそれぞれで、
  stale-while-revalidate は期限切れを `X-Cache: STALE` で返して裏で取り直し、次から新しい応答を返す。stale-if-error は上流の
  502 と、上流に接続できないときの両方で期限切れを返し、保存していない URL は 502 のまま）。

## 関連

- [F-257](../features/F-257-proxy-cache-h2-h3.md)・[B-491](B-491-proxy-cache-ignores-route-store-settings.md)
