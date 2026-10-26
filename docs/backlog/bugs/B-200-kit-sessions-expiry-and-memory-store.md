# B-200: veil-kit の Sessions: Cookie 保存は期限をサーバで検査せず、MemoryStore は上限が無く掃除の時機が偏る

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の API・Wasm のレビュー（docs/artifacts/v0100/review-api-wasm.md） #7 #10（2026-10-26）。

## 事象

- Cookie 保存（`CookieStore`）は Cookie の Max-Age だけに頼り、期限をサーバ側で検査しなかった（盗まれた Cookie を永久に使える）。
- `MemoryStore` は期限切れを消すのが件数が 2 のべき乗になったときだけで、上限も無いため、セッションを作り続けるとメモリが
  際限なく増えた。

## 修正

- Cookie の中身を「8 バイトの期限（UNIX 秒、BE）+ JSON」にして封をし、読むときに期限を検査する。
- `MemoryStore` は `max_entries`（既定 100,000。`MemoryStore::max_entries(n)`）を持ち、60 秒ごとか満杯のときに期限切れを
  掃除し、それでも満杯なら期限の近い 10% を捨てる。

## 試験

- `crates/veil-kit/src/session.rs` の `cookie_expiry_is_checked`・`memory_store_is_bounded`。

## 実施内容

- `crates/veil-kit/src/session.rs`。文書: `docs/guide/library-kit.md`（英日）。
