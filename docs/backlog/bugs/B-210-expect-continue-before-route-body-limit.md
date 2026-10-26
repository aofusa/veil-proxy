# B-210: `Expect: 100-continue` の要求にルートの本文の上限を見る前に 100 Continue を返し、413 が届かないことがある

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Windows の検証（`tests/api/library_e2e.sh production` の Python の「body limit」が `413` でなく `100`。2026-10-26）。

## 事象

- HTTP/1.1 の要求処理（`src/proxy/h1.rs`）は、`Content-Length` がグローバルの上限（`max_body_size`）以内なら、ルートを
  選ぶ前に `Expect: 100-continue` へ `100 Continue` を返していた。その後でルートの検査（`[route.security]
  max_request_body_size`・IP・メソッド・レート制限）が要求を拒むと、クライアントは既に本文を送り始めている。
- veil は 413 を書いてから本文を読み捨てて閉じる（B-102）が、読み捨ての上限より先に閉じると未読の本文で RST になり、
  Windows ではクライアントが 413 を読む前に接続ごと捨てる（curl は `100` を最後の状態として報告する）。
- RFC 9110 §10.1.1: 本文を受け付けないなら 100 Continue を返さずに最終の応答を返すべき。ルートが見つからない 404 も同じ。

## 修正

- 先に返していた 100 Continue を、ルートを選んで `check_security` を通った後へ移す（上限を超える要求は 100 を返さずに
  413、ルートが無ければ 404）。グローバルの上限を超えてルートを見てから決める経路（F-266 の `defer_size`）は従来どおり。

## 試験

- `tests/standalone_e2e.rs` の `b210_expect_continue_waits_for_route_body_limit`（上限を超える `Content-Length` には最初の応答が
  413、上限内には 100）。
