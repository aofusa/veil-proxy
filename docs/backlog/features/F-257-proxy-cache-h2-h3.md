# F-257: HTTP/2・HTTP/3 のクライアントと、HTTPS の上流にもプロキシのキャッシュを効かせる

**状態: 未着手（2026-10-23 起票。B-162 の調査で分かった）**

## 今の状態

- `[route.cache]` は HTTP/1.1 のクライアントの要求（`proxy::forward` の経路）だけで読み書きされる。
- HTTP/2（`proxy::h2`）・HTTP/3（`http3_server`）の中継はキャッシュを見ずに上流へ送る。
- HTTPS の上流（`proxy::https::proxy_https_pooled`）は、キャッシュを引くが保存しない（`proxy::forward` のコメント
  「HTTPS は別途実装が必要」）。E2E の `/cached/*` の上流は HTTPS なので、そこではキャッシュから返らない。
- 文書には v0.9.0 で書いた（`compression-cache.md` の Notes）。

## 案

- HTTPS の上流の経路でも、HTTP の経路と同じく `CacheSaveContext` で応答を集めて保存する。
- HTTP/2・HTTP/3 の中継の経路で、要求を上流へ送る前に `cache::get_global_cache()` を引き、当たれば `Bytes` の本文をそのまま返す。
  ゼロコピーで、要求ごとの確保を増やさない。
- 外れたら、応答を `CacheSaveContext` で集めて保存する。HTTP/1.1 と同じ判定（`Cache-Control`・`Vary`・`Set-Cookie`・`Authorization`）にする。
- 試験: E2E で h2c・HTTP/2（TLS）・HTTP/3 の 2 回目の `X-Cache: HIT`。性能の交互 A/B（キャッシュの無いルートが変わらないこと）。

## 関連

- [B-162](../bugs/B-162-proxy-cache-small-responses-never-stored.md)
