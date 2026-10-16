# B-132: HTTP/2・HTTP/3 の検査の順が HTTP/1.1 と違い、HTTP/3 は content-length の無い要求の本文の上限を見ていなかった

**状態: 完了（feat/v080-release、2026-10-16。F-234 の段階 1 で発見）**

## 症状

- HTTP/1.1 は「IP → メソッド → 本文の大きさ → レート制限」の順で検査した。HTTP/2・HTTP/3 は本文の大きさの前にレート制限を
  数えていた（`proxy::check_security`）。
  - 本文の上限を超えた要求が、413 で拒否されるのにレート制限の枠を使っていた。
  - 両方の条件に当たる要求で、返る状態コード（413 か 429）がプロトコルで違った。
- HTTP/3 のバッファ経路は `content-length` の値だけで本文の上限を見ていた。HTTP/3 では省略できるので、省略した要求は
  `max_request_body_size` を素通りした。

## 修正

- 検査は `src/l7.rs` の `check_security` 1 つにまとめ、3 経路とも同じ順（IP → メソッド → 本文の大きさ → レート制限）にした。
  HTTP/1.1 の手書きの検査をやめた。
- HTTP/3 のバッファ経路は、`content-length` と受け取った本文の長さの大きい方で見る。
- `docs/guide/routing.md`（英日）に検査の順を書いた。

## 試験

- 単体: `l7::tests::oversized_request_does_not_consume_rate_limit`。
- E2E: `standalone_e2e::f234_protocol_matrix_same_decisions`（HTTP/1.1・h2c・HTTP/3 で 413 を 3 回 → 200 → 429）。

## 関連

F-234、B-31、B-133。
