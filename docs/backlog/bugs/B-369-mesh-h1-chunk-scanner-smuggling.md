# B-369: メッシュの HTTP/1.1 の chunked の走査が裸の LF を通し、要求の密輸で RBAC・JWT を素通りできた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx のセキュリティスキャン（`CLAUDE-SECURITY-20261113-030705` の F2。HIGH）で判明。

## 事象

メッシュの HTTP/1.1（`src/mesh/http1.rs`）は、chunked の本文の終わりを独自の緩い `ChunkScanner` で探し、生のバイトを
`Transfer-Encoding: chunked` のまま上流へ送っていた。`ChunkScanner` はチャンクの拡張・トレーラの行の裸の LF を中身として
扱い、空のチャンクの大きさの行を最後のチャンクとして受けていた。裸の LF を行の終わりとして読む上流（h11/uvicorn 等）とは
本文の終わりがずれ、後ろのバイトを veil が解析・認可していない 2 つ目の要求として実行させられる
（`0\r\n\nGET /admin HTTP/1.1\r\n...`）。AuthorizationPolicy・RequestAuthentication を素通りし、veil が消すはずの
身元のヘッダ（`x-forwarded-client-cert`・`l5d-client-id` 等）も付けられる。

## 対応

- `ChunkScanner` を外し、データプレーンと同じ厳格な `http_utils::ChunkedDecoder::new_request` で本文の終わりを求める
  （`chunked_end`。要求の本文の読み捨て・中継・溜める経路。応答の中継も同じ）。
- フレーミングが不正な要求は 400（`DPE`）で接続を閉じる（途中まで送った上流の接続は捨てる）。不正な応答は途中で切って
  両方の接続を閉じる。読み捨ての上限は 1 MiB。

## テスト

- `mesh::http1::tests::chunked_end_finds_end_and_rejects_smuggling`（裸の LF のトレーラ・拡張・空の大きさの行を拒否し、
  正しい本文の終わりを返す）。
