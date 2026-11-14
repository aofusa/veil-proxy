# B-392: メッシュの chunked の中継が、大きさの行・トレーラ・宣言したチャンクを上限なく溜めていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F6・F31 から確認。

## 事象

- F6: `mesh::http1::dechunk_into`（HTTP/1.1 の chunked の要求を HTTP/2 の上流へ中継する）は、大きさの行・
  トレーラの行を探して CRLF が見つかるまで `read_more` で読み足し、`acc` の大きさに上限が無かった。ヘッダの
  ループの `MAX_HEAD_BYTES`、`copy_req_body` の `ChunkScanner` のような上限がこの経路には無かった。
- F31: `mesh::h2::to_h1`（HTTP/2 のクライアントを HTTP/1.1 の上流へ中継する）は、上流の応答ヘッダを上限なく
  `rbuf` に溜め、chunked の本文はチャンク全体（`size` は上流が決める。`usize::MAX` まで）が `sc_buf` に揃うまで
  転送しなかった。悪意のある上流は巨大な・終わらないヘッダや大きなチャンクの宣言でプロキシに溜めさせられた。

## 対応

- `take_chunk_data(dec, acc)` を足し、`dechunk_into` は厳格なデコーダ（`ChunkedDecoder::new_request(0)`・
  `next_data_span`。大きさの行 64 バイト・拡張とトレーラは各 16 KiB まで。B-372）で、届いた分をその場で
  デコードして流す。
- `to_h1` は応答ヘッダを `MAX_HEAD_BYTES` までにし（超えたら 502）、chunked の応答の本文も同じデコーダで流す
  （`sc_buf` を消した）。不正なフレーミング・途中で切れた本文はストリームのリセット（INTERNAL_ERROR）にする。

## テスト

- `mesh::http1::tests::take_chunk_data_streams_and_bounds_framing`（`--features full,mesh-istio,mesh-linkerd`）。
- kind での E2E は回していない。
