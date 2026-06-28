# F-29: ホットパスのロック排除・非同期FS・ゼロアロケーション化

## 概要

パフォーマンス評価レポート（`docs/gemini/performance-issue/README.md`）で指摘されたホットパスの問題を修正する。

## 現状の問題

### 1. std::sync::Mutex がホットパスに残存

- **`src/cache/memory.rs`**: `Mutex<LruCache>` と `Mutex<usize>` でリクエスト毎に lock 待ちが発生。
- **`src/cache/file_cache.rs`**: `valid_duration` と `max_entries` の設定値に `Mutex<Duration>` / `Mutex<usize>` を使用。

### 2. ブロッキング std::fs ファイルシステムコールがホットパスに残存

- **`src/cache/file_cache.rs` (`fetch_file_info`)**: キャッシュミス時に `path.canonicalize()` と `std::fs::metadata()` を同期呼び出し → イベントループスレッドをブロック。
- **`src/buffering/handler.rs`**: `std::fs::create_dir_all`・`std::fs::File::create`・`std::fs::metadata`・`std::fs::File::open` がホットパスで同期呼び出し。

### 3. ホットパスでの過剰ヒープアロケーション

- **`src/proxy.rs`**: `peer_addr.ip().to_string()`（接続毎）、`format!("{}:{}", target.host, target.port)`（プロキシパスマッチ毎）、`path_str.to_string()`・`method_str.to_string()`・`client_ip.to_string()` 等（リクエスト毎）。
- **`src/proxy.rs` (Range リクエスト)**: `start.to_string().as_bytes()` 等の数値→文字列変換で `String` を一時確保。
- **`src/proxy.rs` (WASM ヘッダ操作)**: `format!("HTTP/1.1 {} {}\r\n", ...)` 等のレスポンス行構築。

### 4. ホットパスでの過剰クローン

- **`src/proxy.rs`**: `wasm_engine.clone()`・`modules_to_apply.clone()` がリクエスト処理の各段階で複数回発生。
- **`src/proxy.rs`**: ヘッダのキー・値に `.clone()` して `Box` 化・タプル化。

## 改修内容

### (A) cache/memory.rs のロック排除

`Mutex<LruCache>` を廃止し、シャーディングで並行性を高める構造（`dashmap` ベース LRU または `papaya` 等ロックフリーマップ）に移行する。  
キャッシュキーは既にハッシュ値（u64）なので、`u64 % SHARD_COUNT` でシャードに分散可能。

```rust
// 例: DashMap + LRU per shard
const SHARD_COUNT: usize = 16;
struct MemoryCache {
    shards: [Mutex<LruCache<u64, MemoryCacheEntry>>; SHARD_COUNT],
    ...
}
```

競合の激しいグローバル Mutex を 16 シャードに分散することでロック待ちを 1/16 に削減。

### (B) cache/file_cache.rs の Atomic 化と非同期FS

- `valid_duration: Mutex<Duration>` → `AtomicU64`（ナノ秒）に変更。
- `max_entries: Mutex<usize>` → `AtomicUsize` に変更。
- `fetch_file_info` 内の `canonicalize()` / `std::fs::metadata()` → io_uring `STATX` オペコード (`IORING_OP_STATX`) を使った非同期呼び出しに変更。

### (C) buffering/handler.rs の非同期FS

`std::fs::File::create` / `std::fs::metadata` / `std::fs::File::open` を `tokio::fs` 相当の io_uring 非同期版（`src/runtime/io.rs` の `AsyncFile`）に変更。  
すでに `src/runtime/io.rs` に `AsyncOpenFile` 等の基盤がある。

### (D) proxy.rs のゼロアロケーション化

- `peer_addr.ip().to_string()` → `itoa`/`std::net::IpAddr::fmt` を使い StackString (`[u8; 46]`) に書き込む。
- `format!("{}:{}", target.host, target.port)` → スタック上の小バッファまたは `ArrayString` に書き込む。
- `path_str.to_string()` 等 → ライフタイムを伸ばして借用で引き回す（`&str` のまま）。
- 数値→バイト列変換 → `itoa::Buffer::format()` を使いスタックバッファに書き込む。
- WASM ヘッダ構築 → `write!` で事前確保済みの `BytesMut` に直接書き込む。

### (E) proxy.rs の Arc クローン削減

- `wasm_engine.clone()` → 呼び出し元でクローンした `Arc` を引数として受け取る（関数シグネチャ変更）。
- `modules_to_apply.clone()` → `Arc<[WasmModule]>` で参照カウントクローン（shallow copy）に留める。

## 受け入れ条件

- [ ] `cargo test --bins --test integration_tests --features "full"` が通る
- [ ] `./tests/e2e_setup.sh test` が全 389 テスト通過
- [ ] `cargo clippy --features "full"` が警告なし
- [ ] ホットパスに `Mutex::lock()` の同期待ちが残らない（POLL_ADD を使った非同期版のみ）
- [ ] ホットパスに `std::fs::*` の同期呼び出しが残らない

## 依存・リスク

- `itoa` クレート追加（軽量、ゼロアロケーション数値書き込み）
- DashMap はすでに依存済み
- `src/runtime/io.rs` の AsyncFile / STATX 機能が必要（io.rs に OPENAT/STATX が未実装の場合、io_uring の IORING_OP_STATX SQE を追加する必要あり）
- buffering/handler.rs の非同期化は難易度が高い（既存の `AsyncOpenFile` インターフェースの拡充が必要）
