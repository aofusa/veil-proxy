# B-372: 溜める経路の chunked の要求でフレーミング（拡張・トレーラ・大きさの行）が上限なく溜まっていた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx のセキュリティスキャン（`CLAUDE-SECURITY-20261113-030705` の F5。MEDIUM）で判明。

## 事象

`proxy::h1::read_full_request_body`（バッファ経路・ハンドラ等の本文を溜める経路）は、chunked の本文を生のまま溜めてから
デコードしていた。本文の上限（`max_request_body_size`）はデータの量だけに掛かり、チャンクの拡張・トレーラ・大きさの行の
長さには上限が無いため、データ 0 バイトで拡張やトレーラを送り続けるとメモリを使い切らせられた。

## 対応

- `ChunkedDecoder` の厳格なモード（要求）に上限を足す: 大きさの行 64 バイト、拡張は 1 要求で合計 16 KiB、トレーラも合計 16 KiB
  （超えたら `Invalid`）。
- `read_full_request_body` は受けた分を `decode_chunked_into` でその場でデコードし、データだけを溜める（生の本文を持たない）。
- `ChunkedDecoder::feed_counted` はチャンクのデータの中をまとめて読み飛ばす（B-369 で要求の中継に使うときの性能）。

## テスト

- `http_utils::chunked_span_tests::strict_request_decoder_bounds_framing`・`feed_counted_matches_bytewise_feeding`、
  `proxy::h1::b186_overflow_tests::read_full_request_body_bounds_chunked_framing`。
- 文書: `docs/guide/security.md`（英日）。
