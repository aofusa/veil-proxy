# B-236: Wasm のホスト関数がゲストの `ptr`/`len` を符号拡張して無検査で加算し、不正な値で接続のタスクが panic する

**状態: 未修正（2026-10-27 起票。セキュリティレビュー XT-01）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 の `src/wasm/host/*.rs` の読解）。

## 事象

- `headers.rs::read_string`/`write_to_wasm`（L149〜L190 付近）ほか、`buffers.rs:133`・`foreign.rs:120/131/142/154`・`metrics.rs:15`・`grpc.rs:21`・
  `http_call.rs:15/95/112/129`・`logging.rs:23`・`stream.rs:66/170` の約 25 箇所で、`ptr: i32`/`len: i32` を `ptr as usize` で符号拡張し、
  `start + len as usize` を**検査なしで**加算してから `end > data.len()` を見る。
- 負の `ptr` や巨大な `len` では release で加算が回り込んで `data[start..end]` のスライスで panic する（debug では加算で panic）。
- メモリ安全性は Rust の境界検査で破れない。panic は `CatchUnwindFuture` で接続単位に封じ込められる（ワーカーは落ちない）。
  ただし悪意ある・壊れたプラグインがホスト関数の呼び出しで接続を落とせる。

## 改修案

- `u32` で受け、`checked_add` した上で `memory.data(..).get(start..end)` を使う共通のヘルパ 1 つ（`guest_slice(memory, ptr, len) -> Result<&[u8]>`）に
  全箇所を集約する。失敗は Proxy-Wasm の `Status::BadArgument` を返す。
- `fuzz/fuzz_targets/wasm_host_abi.rs` に `ptr`/`len` が負・`i32::MAX` 付近のケースを足す。

## 受け入れ条件

- fuzz target が panic を見つけない。`tests/e2e_tests/test_f62_*` の読み取り系 API の E2E が変わらない。

## 関連

- [B-134](B-134-wasm-property-path-huge-alloc.md)、[B-19](B-19-proxy-wasm-abi-mismatch.md)
