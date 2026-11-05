# B-303: 既定の設定で SVG を圧縮しない、`compression` feature の無いビルドで圧縮を有効にすると壊れた応答になる、HTTP/3 が要求ごとに圧縮の設定を丸ごと複製する

**状態: 修正済み（v0.11.0）**

## 事象（コードの読み取り）

1. **SVG**: `skip_types` を `compressible_types` より先に見る（`src/config/compression.rs:280`）。既定の `skip_types` に `"image/"` があるので
   `image/svg+xml` を圧縮しない。文書は SVG を既定で圧縮すると書いている（`compression-cache.md:119`・ja :117・`examples/config.toml:2254`・
   `library-production.md`）。ライブラリの既定も同じ。
2. **feature 無し**: `compression` feature は default に入っていないが、`[route.compression] enabled = true` を止めない。
   - 静的配信は `Content-Encoding` を付けて圧縮していない本文を返す（`static_response.rs:659` のスタブ）→ クライアントが壊れた本文を受け取る。
   - HTTP/1.1 のプロキシで圧縮すべき応答が全部 502（`proxy/compress.rs:695` のスタブ、:255 で扱う）。
3. **HTTP/3 の複製**: `resolve_http3_compression_config`（:377-437）が要求ごとに約 20 個の文字列を持つ `CompressionConfig` を深く複製する
   （`request_task.rs:1057,1108,1140`・`http3_server/mod.rs:1744`）。ホットパス規則に反する。
4. （Low）`mode = "memory"` の圧縮した本文はプロトコル間で共有され、レベルをキーに含まないので、HTTP/1.1・HTTP/2 が先に圧縮すると HTTP/3 専用の
   レベル・方式が効かない。

## 改修案

1. 最長一致の型を優先する（`image/svg+xml` は `compressible_types` に明示があれば勝つ）か、既定の `skip_types` を具体的な型（`image/png` 等）にする。
2. 設定のロード時に、feature の無いビルドで `enabled = true` なら警告して無効にする。`should_compress` も `cfg!(feature = "compression")` で閉じる。
3. ルートごとの HTTP/3 の圧縮設定をロード時に解決して `Arc` で持つ（F-148/F-159 と同じ）。
4. memory の圧縮本文のキーに方式・レベルを含める。
- 試験: SVG が既定で圧縮される、feature 無しのビルドで圧縮の設定が警告になり応答が壊れない（`--no-default-features` の統合試験）、allocs/req（HTTP/3）。

## 修正

1. `compressible_types` と `skip_types` は長く一致した方が勝つ（`image/svg+xml` は `image/` に勝つ）。
2. `compression` feature の無いビルドで `enabled = true` はロード時に警告し（`CompressionConfig::warn_if_unbuilt`）、`should_compress` は常に `None`（壊れた本文・502 を返さない）。
3. `resolve_http3_compression_config` は `Arc<CompressionConfig>` を受けて返し、HTTP/3 用に重ねた設定を経路の設定の中（`H3Resolved` の `OnceLock`）に 1 回だけ作る。要求ごとの深い複製は無い（どちらも無効なら参照カウントだけ）。
4. `mode = "memory"` の圧縮本文は作ったときのレベルを持ち、違うレベル（HTTP/3 専用の設定）の要求はその要求だけ圧縮する。
- 試験: `svg_is_compressed_despite_image_skip`・`should_compress_only_picks_configured_encodings`。

## 関連

- [B-297](B-297-zero-copy-compression-docs-mismatch.md)（D3）、[F-294](../features/F-294-precompressed-static-siblings.md)
