# B-295: HTTP/1.1 の splice の経路のいくつかの書き込みに時間の上限が無く、平文の splice のループが譲らない。`SPLICE_F_MORE` を本文が続くかで付けている

**状態: 修正済み（v0.11.0）**

## 事象（コードの読み取り、f8dc80de）

1. 時間の上限が無い書き込み（`async_raw_write_all`、`src/pool.rs:806-841` は時間切れを持たない）: `src/proxy/splice.rs:447,459`（応答 head。kTLS は常に、
   平文は本文が無いとき）、`:516`（chunked の本文）、`:545`（接続の終わりまでの本文）、`:299,306`（上流への要求 head・最初の本文）。読まないクライアント・
   上流で接続とワーカーのタスクが永久に残る（[B-17](B-17-malformed-backend-client-hang.md) と同種）。
2. 平文の `splice_now` のループ（`splice.rs:57-120`）が譲らない。速い相手と速い上流の間で大きな本文を流す間、同じワーカーの他の接続が止まる
   （B-286 は `sendfile` にだけ `yield_now` を入れた）。
3. `SPLICE_F_MORE`（`splice.rs:90-99`）を「残りがある」で付けている。残りがあってもすぐ届くとは限らず、上流を待つ間 kTLS は 16KB 未満のレコードを
   保留したままにする（[B-25](B-25-reverse-proxy-http1-wrk-zero-completed.md) と同種の遅延。ハングにはならないが遅れる）。

## 改修案

1. すべて `timeout(WRITE_TIMEOUT, ..)` で包む（`send_head_more` と同じ）。
2. 1MB 動かすごとに `yield_now`。
3. MORE は「今回の src → pipe でチャンクいっぱい（`n == chunk_size`）読めた」ときだけ付ける。
- 確認: 読まないクライアントで `WRITE_TIMEOUT` 後に閉じる（応答 head・chunked・EOF 終端）。大きな本文を流しながら同じワーカーの別の接続の応答時間が伸びない。

## 修正

- `src/proxy/splice.rs`: 上流への要求 head・本文、クライアントへの応答 head・chunked・EOF 終端の本文の書き込みを `timed_raw_write`（`WRITE_TIMEOUT`）にした。その場の splice は 1MB ごとに `yield_now`。`SPLICE_F_MORE` は残りがありチャンクいっぱい読めたときだけ。
- 要求本文の splice の条件を `is_ktls_recv_enabled() || is_plain()` にした（送信だけの kTLS で暗号文を上流へ流さない。F-292 の項目 6）。

## 関連

- [F-292](../features/F-292-ktls-h1-zero-copy-same-as-plain.md)、[B-286](B-286-plaintext-h1-copies-file-and-proxy-bodies.md)
