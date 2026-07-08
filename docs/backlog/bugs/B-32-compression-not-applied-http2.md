# B-32: Accept-Encoding 指定時も HTTP/2 応答が未圧縮（encoding=none）

## 事象

`[route.compression] enabled = true` かつ `Accept-Encoding: gzip` で `GET /` すると、
HTTP/2 応答の `Content-Encoding` が付かない（`compression_cache_probe`: encoding=none）。

## 影響

- 圧縮有効ルートでも HTTP/2 クライアントに帯域削減・圧縮爆弾防御の恩恵が及ばない可能性。
- セキュリティテスト上は「クラッシュなし」で合格するが、機能が期待通りかは未検証。

## 改修案

- HTTP/2 応答経路の圧縮ネゴシエーションを HTTP/1.1 経路と照合し、HPACK で Content-Encoding を送出。

## 関連

- F-90 compression_cache_probe
- `compression` feature