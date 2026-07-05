# B-20: WASM 読み取り系ホスト関数の同期 `call` が async store で panic する

## 出典

F-62（HTTP コール Pause/resume 配線）の E2E 実装中に検出（2026-07-05）。
**同日修正済み。**

## 事象

WASM エンジンは async store（`async_support(true)` + fuel yield、F-27）で動作するが、
読み取り系ホスト関数がモジュール側アロケータ `proxy_on_memory_allocate` へ再入する際に
**同期 `TypedFunc::call`** を使用していた。wasmtime は async store での同期 call を
`panic!("must use call_async with async stores")` で拒否するため、以下のホスト関数を
呼ぶモジュールはワーカータスクごと panic していた（タスク単位で捕捉されるが
当該リクエストは異常終了）:

- `proxy_get_header_map_pairs` / `proxy_get_header_map_value`（headers.rs）
- `proxy_get_buffer_bytes`（buffers.rs）
- `proxy_get_property`（properties.rs）
- `proxy_get_shared_data`（shared_data.rs）

B-19 と同様、既存 E2E の header_filter.wasm は書き込み系 API のみ使用のため露見せず。

## 修正内容（2026-07-05）

- 上記 5 ホスト関数を `Linker::func_wrap_async` へ変更し、
  `proxy_on_memory_allocate` の呼び出しを `TypedFunc::call_async(...).await` 化。
- `proxy_get_shared_data` は `RwLock` ガードを await 前に確実にドロップするよう
  ブロックスコープ化（Future の `Send` 境界要件）。

回帰確認: wasm 単体 100+ 件・WASM E2E 23 件・F-62 E2E（読み取り系 API を実際に使う
http_call_filter.wasm）で検証。

## 関連

- 検出: [F-62](../features/F-62-proxy-wasm-http-call-benchmark.md)
- 併発: [B-19](B-19-proxy-wasm-abi-mismatch.md)
- 背景: F-27（wasmtime async 化・fuel yield）
