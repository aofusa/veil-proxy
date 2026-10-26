# B-211: ハンドラのルートで `[api] max_body_size` を超える `Expect: 100-continue` の要求に 100 Continue を返し、413 が届かないことがある

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Windows の検証（B-210 の修正後も `tests/api/library_e2e.sh production` の Rust・Python の「body limit」が `413` でなく
`100`。curl 8.23 は 1 MB を超える本文に `Expect: 100-continue` を付ける。2026-10-27）。

## 事象

- B-210 は 100 Continue をルートの検査（`[route.security]`）の後へ移したが、本文を溜めるハンドラのルートの上限
  （`[api] max_body_size`）はハンドラの経路で本文を読みながら見ていた。上限を超える要求にも 100 Continue を返し、本文の
  途中で 413 を書いて閉じるので、未読の本文で RST になり Windows ではクライアントに 413 が届かない。
- グローバルの上限を超えてルートを見てから決める経路（`defer_size`）の 413 と、ハンドラの経路の本文の読み取りの失敗
  （chunked の上限超過等）の応答も、本文を読み捨てずに閉じていた（B-102 と同じ RST）。

## 修正

- `src/proxy/h1.rs`: 本文を溜めるハンドラのルート（ストリーミング受信でないもの）は、`Content-Length` が `[api]
  max_body_size` を超えれば 100 Continue を返さずに 413 を返し、本文を読み捨ててから閉じる。
- `defer_size` の 413 とハンドラの経路の本文の読み取りの失敗の応答の後も、本文を読み捨ててから閉じる。

## 試験

- `tests/api/library_e2e.sh production` の「Expect over the limit gets 413 first」（Rust・Python。本文を送らずに最初の応答行が
  413）。
