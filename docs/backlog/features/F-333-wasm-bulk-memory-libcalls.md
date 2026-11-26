# F-333: ゲストの小さな `memory.copy`・`memory.fill` を wasmtime の側で速くする（見送り）

**状態: 見送り（2026-11-26、利用者の判断。[F-327](F-327-proxy-wasm-path-cost.md) の子。wasm のランタイムはセキュリティ上デリケートなので、
wasmtime に手直し・独自の拡張を入れない。HttpArena のエントリはフィルタを bulk-memory 無しでビルドする〈[F-334](F-334-httparena-filter-and-guide.md)。
測った値は json-tls −2.2 µs/要求〉。下は判断の記録）**

## 現状

- json-tls のプロファイルで、`Instance::memory_copy` 8.37%・`catch_unwind_and_record_trap<libcalls::memory_copy>` 3.41%・
  `libcalls::raw::memory_copy` 1.50%・ゲストの `wasmtime_builtin_memory_copy` 1.98%、計 2.94 µs/要求（ユーザー時間 19.29 µs の 15%。
  中の memmove は別）。h1・h2 は 0.08 µs。
- フィルタの `write_body` の小さな `extend_from_slice`（数〜数十バイト）が `memory.copy` 1 回ずつになり、wasmtime 36 はそれをゲストの中で
  展開せず、ランタイムの関数を呼ぶ（トラップの準備・境界の検査・memmove）と見ている。Rust 1.87 以降の `wasm32-wasip1` は bulk-memory が
  既定で有効。どのフィルタにも起きる（Rust の SDK で書いた、本文を組み立てるフィルタ）。

## 改修案

- 下の「wasmtime の側」の (A)（ランタイムの側だけ）か (B)（Cranelift での展開）。どちらも wasmtime の手直しで、Linux でも vendoring 版を使う
  判断（[F-335](F-335-wasmtime-fiber-entry-cost.md) の案 A と同じ）が要る。
- ゲストの側の対処（bulk-memory 無しのビルド）はエントリとガイドの話なので [F-334](F-334-httparena-filter-and-guide.md) で行う。下の「ゲストの側の試し」は
  その根拠と、(A)・(B) の価値を見積もる比較の材料。

### wasmtime の側（ans-02 の 1。下ろし方はソースで確かめた）

- wasmtime 36 は `memory.copy`・`memory.fill` を大きさに関わらず組み込み関数の呼び出しに下ろす（`wasmtime-internal-cranelift-36.0.21/src/func_environ.rs`
  の `translate_memory_copy`）。`Config` に切り替えは無い。Gemini が調べた範囲では 37〜42 でも変わっていない（main は未確認）。
- 1 回の経路: ゲストの包み（出口の記録・間接呼び出し）→ `libcalls::raw::memory_copy` → `catch_unwind_and_record_trap`（veil は `panic = "abort"`
  を禁じているので毎回 `catch_unwind` の枠を作る）→ `Instance::memory_copy`（同じメモリでも `get_memory` を 2 回、境界の検査を 2 回）→ `memmove`。
- wasmtime を手直しする場合の 2 段: (A) ランタイムの側だけ（`catch_unwind` を通さない結果の型、`get_memory` 1 回、小さい写しをその場で）
  約 50〜100 行、見込み −1.0〜1.8 µs（json-tls）。(B) Cranelift で小さい写しをゲストの機械語に展開（書く前に両方の範囲を検査し、範囲外なら
  1 バイトも書かずにトラップ、重なりは `memmove` の意味）約 150〜300 行、見込み −2.0〜2.6 µs。

### ゲストの側の試し（2026-11-26、調査。エントリのファイルは変えていない）

- Rust 1.99.1 の `wasm32-wasip1` は `bulk-memory` が既定で有効（`rustc --print cfg`）。既定のビルドのフィルタは `memory.copy` 43 箇所
  （`put_i64`・`on_http_request_headers`〈`write_body` を含む〉・`sum`・SDK の `serialize_map`・`serialize_property_path`・`finish_grow` 等）。
- `RUSTFLAGS='-C target-feature=-bulk-memory,-bulk-memory-opt'` でビルドすると、それらは wasi-libc の `memcpy` の呼び出しになり、`memcpy` は
  32 バイト以下をゲストの中のループで写し、超えるときだけ `memory.copy` を使う（`memory.copy` は 7 箇所）。`-Zbuild-std` は要らない。
- ans-03 の確かめ: Rust の self-contained の wasi-libc（wasi-sdk の clang 22 で `+bulk-memory` でビルド）の `memcpy`・`memset`・`memmove` は
  32 バイトを超えるときだけ `memory.copy`・`memory.fill` を使う。`bulk-memory-opt` は `memory.copy`・`memory.fill` のコード生成だけを
  決める部分集合。失うのは受動データセグメント等で、Proxy-Wasm のホストでは問題にならない。TinyGo・Clang（C++ の SDK）も既定で
  bulk-memory が有効で同じことが起きる（Clang は `-mno-bulk-memory -mno-bulk-memory-opt`）。
- aofish（io_uring、同じ veil のバイナリでフィルタだけ替えて交互 3 巡、1 要求の µs）:

  | 負荷 | 既定のビルド user + sys | bulk-memory 無し user + sys |
  |---|---|---|
  | json-tls | 18.74 + 14.06 | 16.51 + 13.40（ユーザー −2.23） |
  | h1 GET | 6.95 + 9.06 | 6.97 + 9.34（差なし。外れ値の 1 回を除く） |

  ランタイム呼び出しの 2.94 µs のうち約 2.2 µs が消える。残りは小さな写しのゲストの中のループと、32 バイトを超える写しのランタイム呼び出し。
- したがってエントリはゲストの側で足りる。(A)・(B) は「どのフィルタにも効く」ことの価値（Rust・TinyGo・Clang の既定のビルドで本文を組み立てる
  フィルタは同じ費用を払う）と、Linux でも wasmtime を vendoring して手直しを持ち越す費用で決める。

## 危険

- (B) は bulk-memory の仕様（範囲外なら 1 バイトも書かずにトラップ・重なりは `memmove` の意味）を守る。Pulley（BSD の一部）でも同じ結果。
- wasmtime のセキュリティの修正を手直しの上へ持ち越す費用。

## 受け入れ条件

- 既定のビルド（bulk-memory あり）のフィルタで json-tls の 1 要求のユーザー時間が (A) で 1.0 µs 以上、(B) で 2.0 µs 以上下がる。
  大きな写し（数 KB 以上）が遅くならない。範囲外・重なりの試験（wasmtime の spec の試験を含む）が通る。
- 着手は wasmtime の手直しの範囲・保守の方法を利用者が判断してから。

## 関連

- [F-327](F-327-proxy-wasm-path-cost.md)（親）・`docs/artifacts/proxy-wasm-perf/profile-2026-11-26.md`・`gemini/ans-02-wasmtime.md`・`gemini/ans-03-guest.md`
