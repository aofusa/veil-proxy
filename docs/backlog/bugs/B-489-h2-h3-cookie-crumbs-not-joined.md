# B-489: HTTP/2・HTTP/3 で分けて送られた cookie をつながずに渡す

**状態: 修正済み（feat/protocol-parity。2026-11-22 起票・修正。機能 × プロトコルの網羅性の確認）**

## 出典

機能とプロトコルの対応の確認（`docs/artifacts/protocol-parity/audit.md`）。B-485（WebSocket の Consistent Hash）を直す中で、
`cookie:` キーが 1 つ目の `cookie` フィールドしか見ないことから見つけた。

## 事象

- HTTP/2・HTTP/3 のクライアントは Cookie を複数の `cookie` フィールド（crumb）に分けて送ってよい（RFC 9113 §8.2.3・
  RFC 9114 §4.2.1。ブラウザは圧縮のために分けることが多い）。HTTP/1.1 や汎用のアプリケーションへ渡す前に `"; "` で
  1 つにつながなければならない（MUST）。
- veil はつながずに扱っていた:
  - HTTP/1.1 の上流へは複数の `cookie:` 行のまま送る（RFC 6265 §5.4 は 1 行に限る。1 行目しか読まない実装では
    2 つ目以降の Cookie が消える）。
  - 一貫性ハッシュの `hash_key = "cookie:<名前>"`（`select_with_header_fn`）・ルートの条件・Proxy-Wasm・ハンドラは 1 つ目の
    かけらしか見ない。同じ Cookie を 1 行で送る HTTP/1.1 のクライアントと別の上流へ行く。

## 対応（2026-11-22）

- HTTP/2 はヘッダの復号の直後（`Http2Connection::decode_and_set_headers`。トレーラは除く）、HTTP/3 は要求のヘッダを
  受けた直後（`:path` の正規化の後）に、2 つ以上ある `cookie` を最初の位置の 1 つにつなぐ（`merge_cookie_crumbs`・
  `h3_merge_cookie_crumbs`）。1 つ以下なら触らない（確保もしない）。以後のルートの照合・ハッシュ・Wasm・上流への要求は
  すべてつないだ値を見る。HTTP/2・HTTP/3 の上流へもつないだまま送る（RFC はどちらでもよい）。

## 受け入れ条件

- 単体: かけらを 1 つにつなぎ、順序と他のヘッダを保つ（HTTP/2・HTTP/3）。
- 自己完結型 E2E（`b489_cookie_crumbs_are_joined`）: HTTP/2（h2c・TLS）・HTTP/3 で `cookie: a=1` と `cookie: b=2` を
  送ると HTTP/1.1 の上流は `cookie: a=1; b=2` の 1 行だけを受ける。`cookie:sid` の一貫性ハッシュで、`sid` を 2 つ目の
  かけらに置いた HTTP/2・HTTP/3 の要求が、1 行で送った HTTP/1.1 の要求と同じ上流へ行く。

## 関連

- [B-485](B-485-websocket-ignores-consistent-hash-key.md)
