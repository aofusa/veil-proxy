# B-491: プロキシのキャッシュの保存がルートの `[route.cache]` を見ず、ディスクへの保存をワーカーで同期に書く

**状態: 修正済み（feat/protocol-parity。2026-11-22 起票・修正。F-257 の作業で見つけた）**

## 事象

- データプレーンの保存（`CacheSaveContext::save_to_cache`）は、全体のキャッシュのマネージャー（`entry.rs` で作る既定の
  設定）の `store_with_vary` を呼んでいた。保存してよいか・TTL・`Vary` の扱いは、そのマネージャーの既定
  （`cacheable_statuses = [200, 301, 302, 304]`・`default_ttl_secs = 300`・空の `key_headers`）で決まり、ルートの
  `[route.cache]` の `cacheable_statuses`・`default_ttl_secs`・`key_headers`・`respect_vary` を見ていなかった。
  - 例: `cacheable_statuses = [200]` のルートでも 301・302 を保存した。`default_ttl_secs = 7` のルートでも 300 秒持った。
  - 例: `key_headers = ["Accept-Encoding"]` のルートでも、`Vary: Accept-Encoding` の応答は「キーに無いヘッダで Vary する」
    として保存しなかった（文書の書き方どおりに設定しても値ごとのキャッシュにならない）。
- ディスクへ置く応答（`memory_threshold` を超えるもの）は、ワーカーのスレッドで `write_sync` していた（ホットパスの同期の
  ファイル書き込み）。

## 対応（2026-11-22）

- `CacheManager::store_async(route, ..)` を足し、データプレーンの保存はこれだけを使う。保存の判定（状態・`Cache-Control`・
  `Set-Cookie`・`Vary` と `key_headers`）はルートの設定で行い、ディスクへ置く本文は `offload` で書く。
  `CacheSaveContext` は引き当てたルートの `Arc<CacheConfig>` を持つ（要求ごとの確保は増やさない）。
- 保存する形の整え（ホップバイホップの削除・chunked の本文の復号・`Content-Length` の付け直し）は `normalize_for_store`
  に 1 つにした（HTTP/1.1・HTTP/2・HTTP/3 の当たりが同じ本文・ヘッダを返す。F-257）。
- 保存先の設定（`max_memory_size`・`disk_path`・`max_disk_size`・`memory_threshold`）がルートごとに効かない件は別の問題
  として [B-494](B-494-proxy-cache-storage-settings-not-applied.md) に分けた。

## 試験

- 単体: `store_async_uses_route_policy`（ルートの状態・TTL・`key_headers` で決まる。stale の保持期間も）、
  `store_async_writes_disk_entries`（ディスクへ置く応答も保存できる）。

## 関連

- [F-257](../features/F-257-proxy-cache-h2-h3.md)・[B-492](B-492-proxy-cache-stale-never-served.md)・[B-494](B-494-proxy-cache-storage-settings-not-applied.md)
