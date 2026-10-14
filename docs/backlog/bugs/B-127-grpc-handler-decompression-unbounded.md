# B-127: gRPC ハンドラが圧縮メッセージを上限無しで解凍する（解凍後のサイズで DoS）

**状態: 未修正（F-209 の洗い出しで発見、2026-10-14）**

## 症状

`src/api/grpc/mod.rs` の `Streaming::decode_frame` は、`grpc-encoding: gzip`（または `deflate`）の圧縮フラグ付きメッセージを
`GrpcCompression::decompress`（`src/grpc/compression.rs`）で **最後まで解凍してから** `max_message_size()`（既定 4MiB）と比べる。
`decompress_gzip` / `decompress_deflate` は `read_to_end` で上限無しに `Vec` を伸ばす。

圧縮前の上限（フレームの長さ）は `GrpcFrameDecoder::with_max_size` で 4MiB に抑えているが、gzip は 1000 倍以上に縮むので、
4MiB の圧縮メッセージ 1 つで数 GB を確保させられる。gRPC のハンドラを公開している API ライブラリの利用者すべてが対象
（`grpc-handler`。プロキシは `decompress` を呼んでいないので対象外）。

## 再現（想定）

1. `examples/api/rust-grpc` を起動。
2. 0 が 4GB の gzip（約 4MB）を 5 バイトの見出し（圧縮フラグ 1）で包み、`grpc-encoding: gzip` で unary に送る。
3. サーバの RSS が数 GB に増える（またはアロケータの失敗でプロセスが落ちる）。

## 修正方針

- `GrpcCompression::decompress_limited(&self, data, limit) -> Result<Vec<u8>, ..>` を足し、`Read::take(limit + 1)` で読んで
  `limit` を超えたら `RESOURCE_EXHAUSTED` にする（`Vec` の容量も `limit` を超えて伸ばさない）。
- `compression` feature 無しのビルドでは `decompress` が gzip・deflate を解凍せずにそのまま返すので、そのビルドでは
  `grpc-encoding: gzip` を `UNIMPLEMENTED` で断り、`grpc-accept-encoding` に載せない。
- 同じ修正で、`application/grpc-web*` の要求をハンドラが通常の gRPC として扱う問題（F-209 #4）を 415 で断る。

## 試験

- 単体: 高圧縮率の gzip（上限の 1000 倍に展開されるもの）で `RESOURCE_EXHAUSTED` になり、確保が上限 + α に収まること
  （`count-allocs` か `alloc-stats` で最大確保を見る）。上限ちょうどは通ること。
- E2E: `library_e2e.sh grpc` に圧縮爆弾の要求を 1 件足し、応答が `RESOURCE_EXHAUSTED` でサーバが生きていること。
