# B-99: HTTP/1.1 のプロキシ圧縮だけ zstd のコンテキストを毎回作り直していた

## 事象

2026-10 の Linux フルスイートで、`h2_1_proxy_compression` / `h3_proxy_compression` の HTTP/1.1 が
約 1,900 req/s・CPU 300〜330% だった。54,576B を zstd レベル 3 で圧縮するのに
1 リクエストあたり約 1.6ms の CPU を使っており（1 コア約 33MB/s）、静的配信の圧縮
（キャッシュ無効の `h2_1_feat_compression` で約 0.27ms）の 6 倍だった。

## 原因

F-169 で HTTP/2・HTTP/3 の圧縮はスレッドローカルの `zstd::bulk::Compressor` を使い回す
`zstd_compress_reuse_ctx` に移したが、HTTP/1.1 のプロキシ圧縮経路
（`transfer_compressed_response` / `transfer_https_compressed_response`）は
`zstd::encode_all`（呼び出しごとに圧縮コンテキストを確保・初期化するワンショット API）の
ままだった。AGENTS.md の「ライブラリのワンショット API はコンテキストを作り捨てている
可能性を疑う（F-169）」の取りこぼし。

## 修正

失敗時に `None` を返す `zstd_compress_reused`（`compression` feature のみで有効）を切り出し、
HTTP/1.1 の 2 経路はこれを使って、失敗時は従来どおり無圧縮へフォールバックする。
HTTP/2・HTTP/3 用の `zstd_compress_reuse_ctx` もこれを呼ぶ形にした（挙動不変）。

あわせて、B-93 で HTTP/HTTPS のプールが使わなくなった `PooledConnection::is_valid` が
`--no-default-features --features compression` のビルドで未使用警告になっていたので、
利用者（h2c のプール = `http2`、テスト）に合わせて cfg を付けた。
