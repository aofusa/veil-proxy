# F-333: ゲストの `memory.copy`・`memory.fill` がランタイム呼び出しになる費用を減らす

**状態: 未着手（[F-327](F-327-proxy-wasm-path-cost.md) の子。やり方は調査と試しの後で決める）**

## 現状

- json-tls のプロファイルで、`Instance::memory_copy` 8.37%・`catch_unwind_and_record_trap<libcalls::memory_copy>` 3.41%・
  `libcalls::raw::memory_copy` 1.50%・ゲストの `wasmtime_builtin_memory_copy` 1.98%、計 2.94 µs/要求（ユーザー時間 19.29 µs の 15%。
  中の memmove は別）。h1・h2 は 0.08 µs。
- フィルタの `write_body` の小さな `extend_from_slice`（数〜数十バイト）が `memory.copy` 1 回ずつになり、wasmtime 36 はそれをゲストの中で
  展開せず、ランタイムの関数を呼ぶ（トラップの準備・境界の検査・memmove）と見ている。Rust 1.87 以降の `wasm32-wasip1` は bulk-memory が
  既定で有効。どのフィルタにも起きる（Rust の SDK で書いた、本文を組み立てるフィルタ）。

## 改修案（ans-02・ans-03 と試しの結果で選ぶ）

1. ホストの側: 小さな `memory.copy`・`memory.fill` をゲストの機械語の中で展開する（境界を検査して数バイトずつ写し、大きいものだけランタイムへ）。
   wasmtime の新しい版にあれば上げる、無ければ wasmtime の手直し（[F-335](F-335-wasmtime-fiber-entry-cost.md) と同じ判断）。どのフィルタにも効く。
2. ゲストの側: フィルタを `-C target-feature=-bulk-memory` でビルドする（std・wasi-libc の `memcpy` の扱い、`-Zbuild-std` の要否を確かめる）。
   エントリだけに効く。フィルタを書く人向けの注意としてガイドに書く（[F-334](F-334-httparena-filter-and-guide.md)）。
3. ホストの側: ランタイム呼び出しの経路そのものを軽くする（wasmtime の手直し）。

### 2 の試し（2026-11-26、調査。エントリのファイルは変えていない）

- Rust 1.99.1 の `wasm32-wasip1` は `bulk-memory` が既定で有効（`rustc --print cfg`）。既定のビルドのフィルタは `memory.copy` 43 箇所
  （`put_i64`・`on_http_request_headers`〈`write_body` を含む〉・`sum`・SDK の `serialize_map`・`serialize_property_path`・`finish_grow` 等）。
- `RUSTFLAGS='-C target-feature=-bulk-memory,-bulk-memory-opt'` でビルドすると、それらは wasi-libc の `memcpy` の呼び出しになり、`memcpy` は
  32 バイト以下をゲストの中のループで写し、超えるときだけ `memory.copy` を使う（`memory.copy` は 7 箇所）。`-Zbuild-std` は要らない。
- aofish（io_uring、同じ veil のバイナリでフィルタだけ替えて交互 3 巡、1 要求の µs）:

  | 負荷 | 既定のビルド user + sys | bulk-memory 無し user + sys |
  |---|---|---|
  | json-tls | 18.74 + 14.06 | 16.51 + 13.40（ユーザー −2.23） |
  | h1 GET | 6.95 + 9.06 | 6.97 + 9.34（差なし。外れ値の 1 回を除く） |

  ランタイム呼び出しの 2.94 µs のうち約 2.2 µs が消える。残りは小さな写しのゲストの中のループと、32 バイトを超える写しのランタイム呼び出し。
- したがってエントリは 2 で足りる。1 は「どのフィルタにも効く」ことの価値（Rust の SDK で本文を組み立てるフィルタは既定で同じ費用を払う）と、
  wasmtime の手直しの費用（[F-335](F-335-wasmtime-fiber-entry-cost.md) と同じ判断）で決める。ans-02 の結果（新しい版の wasmtime が展開するか）を待つ。

## 受け入れ条件

- json-tls の 1 要求のユーザー時間が 2.5 µs 以上下がる（どの案でも）。大きな写し（数 KB 以上）が遅くならない。
- 1 を選ぶ場合は wasmtime の手直しの範囲・保守の方法を利用者が判断してから。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・`docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`・`gemini/ans-02-wasmtime.md`・`gemini/ans-03-guest.md`
