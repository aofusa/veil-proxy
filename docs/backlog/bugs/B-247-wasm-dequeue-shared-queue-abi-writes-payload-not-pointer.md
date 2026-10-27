# B-247: `proxy_dequeue_shared_queue` が Proxy-Wasm ABI と違い、`return_data_ptr` にポインタではなくペイロードを直接書く

**状態: 未修正（2026-10-27 起票）**

## 出典

2026-10-27 のセキュリティレビュー 2 回目の WS-01（`docs/artifacts/security/analysis/07-f281-followup.md`）。

## 事象

Proxy-Wasm ABI の `proxy_dequeue_shared_queue(queue_id, return_data_ptr: *mut *const u8, return_data_size: *mut usize)` は、ホストが
`proxy_on_memory_allocate` でゲストのメモリを確保してペイロードを書き、**その番地** を `return_data_ptr` に書く。
`src/wasm/host/shared_queue.rs` L202〜L245 は `write_bytes(&mut caller, return_data_ptr, &bytes)` で **ペイロードのバイト列そのもの** を
`return_data_ptr` の番地に書く。`buffers.rs`・`properties.rs`・`shared_data.rs` は正しく `proxy_on_memory_allocate`（無ければ `malloc`）を使っている。

SDK（Rust の `proxy-wasm` / C++）は書かれた 4 バイトをポインタとして `Vec::from_raw_parts` するので、プラグインのスタック上の 4 バイト変数が
ペイロードで上書きされ（ペイロードが 4 バイトを超える分は隣接するメモリを壊す）、その後に不正なポインタから `Vec` を作る。**サンドボックス内**なので
ホストのメモリ安全性は破れないが、キューを使うプラグインは動かない。

## 改修案

- `shared_data.rs` と同じく `proxy_on_memory_allocate(len)` を呼んで確保し、ペイロードを書き、ポインタと長さを `return_data_ptr`/`return_data_size` に書く。
- E2E: Rust SDK のプラグインで `proxy_enqueue_shared_queue` → `proxy_dequeue_shared_queue` の往復（`test_f62_*` の派生）。

## 関連

- [B-236](B-236-wasm-host-abi-ptr-len-unchecked-add.md)、[F-282](../features/F-282-security-review-followup-hardening.md)
