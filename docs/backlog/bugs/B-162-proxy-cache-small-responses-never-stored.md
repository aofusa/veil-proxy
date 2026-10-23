# B-162: プロキシのキャッシュが、最初の読み込みに収まる小さい応答を一度も保存しない

**状態: 完了（feat/distribution-tickets、2026-10-23。F-256 の Grafana のダッシュボードの試験で発見）**

## 現象

`[route.cache] enabled = true` のルートで、上流の応答が小さい（ヘッダと本文が最初の読み込みに収まる）と、何度要求しても
キャッシュに保存されず、毎回上流へ行く。

- **再現:** `traefik/whoami` を上流にしたルートで同じ GET を 3 回送る。3 回とも `X-Cache` が付かない。
  - メトリクスは `veil_proxy_cache_misses_total` だけが増え、`stores_total`・`hits_total` は出ない。
- 大きい応答（本文の残りを別に読むもの）は保存される。

## 原因

- HTTP/1.1 の中継（`proxy::compress::proxy_http_request_with_compression` の圧縮しない分岐）は、キャッシュの保存
  （`CacheSaveContext::save_to_cache`）を、本文の残りを転送する `transfer_response_body_with_cache` の中でしか呼んでいなかった。
- 本文がすべて最初の読み込みに入っていると、残りが 0 なのでこの関数を呼ばず、保存もしなかった。
- **試験が気づけなかった理由:** E2E の `test_e2e_cached_route_hit` は大きいファイルで試し、しかも「`X-Cache` がある」
  か「本文が一致する」で通る。`test_cache_hit` はキャッシュを確かめていない。

## 修正

- 本文が最初の読み込みに収まり、`Content-Length` がある（本文が完全）ときも保存する。
  接続の終わりで区切る応答は途中かもしれないので保存しない。
- E2E の `test_e2e_cached_small_response_hits` を足した。小さい応答の 2 回目が `X-Cache: HIT` であることを求める。
- 確かめたこと: whoami の応答で 2 回目以降が `X-Cache: HIT`、メトリクスは hits 2・misses 1・stores 1。

## 合わせて分かったこと

- プロキシのキャッシュは HTTP/1.1 のクライアントから平文の上流への要求にしか効かない（HTTP/2・HTTP/3 の中継の経路は
  キャッシュを見ず、HTTPS の上流は保存しない）。E2E の `/cached/*` は HTTPS の上流なので、試験は平文の上流のルート
  `/cached-plain/*` を足して確かめる。
  文書に無かったので `compression-cache.md`（英日）と移行ガイドに書き、対応は F-257 で起票した。

## 関連

- [F-256](../features/F-256-grafana-dashboard.md)
- [F-257](../features/F-257-proxy-cache-h2-h3.md)
