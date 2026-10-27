# B-234: 上流の応答のヘッダが 65 個以上だと「データ不足」と扱い、64KB か 10 秒まで待ってから 502/504 にする

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/http_utils.rs::parse_http_response` の読解）。

## 事象

- `parse_http_response` は `[httparse::EMPTY_HEADER; 64]` で解析し、httparse の `Err`（`TooManyHeaders` を含む）を `None`（データ不足）として返す。
- 呼び出し側は `None` のあいだ読み続け、`MAX_RESPONSE_HEADER_SIZE`（64KB）か `BACKEND_HEADER_TIMEOUT`（10 秒）で 502/504 を返す。
- 多数の `Set-Cookie` を返す正当な上流（65 個以上）で、その経路の全要求が 10 秒待って失敗する（可用性）。

## 改修案

- `Err` と `Partial` を区別し、`TooManyHeaders` は即 502 にする（上限は 128 に上げてもよい。`Vec` を増やさず固定長の配列のまま）。
- HTTP/1.1 の要求側（`proxy/h1.rs`）の 64 個の扱いも同じかを確かめる（要求側は 431 を返すのが正しい）。

## 受け入れ条件

- 単体: ヘッダ 65 個の上流の応答で即座に 502（10 秒待たない）。128 個まで上げる場合はその応答が通る。

## 修正（v0.10.0）

- `http_utils::response_head_invalid` で、`parse_http_response` が `None` を返したときに「データ不足」と「この先も応答のヘッダに
  ならない（不正・ヘッダが 65 個以上）」を見分ける。上流の応答を読む全経路（HTTP/1.1 の中継・圧縮・splice・バッファリング・
  HTTP/2 の中継・WebSocket・バックグラウンドの再検証）で、ヘッダの上限を超えたときと同じ失敗（502）にする。
- 再度の解析は `None` のときだけ（ヘッダが 1 回の read で揃わない稀な場合）で、通常の経路のコストは変わらない。
- 上限（64 個）は上げない（下流で応答のヘッダを組み直す経路も 64 個の配列で解析している）。
- 単体: `response_head_invalid_distinguishes_partial_from_too_many_headers`。

## 関連

- [B-17](B-17-malformed-backend-client-hang.md)（上流の不正な応答でのハング）
