# F-31: メモリキャッシュの `bytes::Bytes` ゼロコピー配信

## 出典

`docs/artifacts/architecture_analysis_v3.md`（メモリキャッシュ層の `bytes::Bytes` 対応）。

## 概要

プロキシキャッシュの本体（`CacheEntry` のボディ・ヘッダ）を `Box<[u8]>` / `Vec<u8>` から
`bytes::Bytes` に変更し、キャッシュヒット時のレスポンス返却を `clone()` による
O(1) の参照カウント増加のみで完結させ、ゼロコピー配信をキャッシュ層まで貫通させる。

## 現状の問題

> 補足（実装確認 2026-06）: v3 レポートは `Box<[u8]>` 保持を指摘するが、現行コードでは
> メモリボディは既に **`Arc<[u8]>`**（`CacheEntry::memory_body() -> &Arc<[u8]>`）で
> 参照カウント共有されており、レポートの主要懸念（ボディの deep copy）は解消済み。

- 残る非ゼロコピー箇所は **ソケット書き込み段**: `http_utils.rs::build_cached_response` が
  `response.extend_from_slice(body_data)` でヘッダ + ボディを 1 本の `Vec` に連結する際に
  ボディを 1 回 memcpy している（キャッシュヒットごと）。
- 原因: `WriteFuture<T: IoBuf>` は所有バッファを要求し、`runtime/buf.rs` の `IoBuf` 実装が
  `Vec<u8>` / `Box<[u8]>` のみ。`Arc<[u8]>` を直接書けないため連結コピーしている。
  また `writev`（scatter-gather）は `runtime/io.rs` で stub。

## 改修内容

1. `CacheEntry` のボディ型を `bytes::Bytes` にする。`CacheEntryBuilder` の `body(...)` も
   `impl Into<Bytes>` を受け付けるようにする。
2. キャッシュヒットのレスポンス送出経路（`src/proxy.rs` / `src/cache/`）で、`Bytes::clone()`
   （参照カウント +1、ゼロコピー）で `WriteFuture` に渡す。`IoBuf` を `Bytes` に対して実装
   （または既存実装を利用）し、所有権ベース I/O にゼロコピーで載せる。
3. ディスクキャッシュからのロードも、読み込んだ `BytesMut` を `freeze()` して `Bytes` 化する。

## 受け入れ条件

- [ ] `cargo test --features full` のキャッシュ系テストが回帰なし。
- [ ] キャッシュヒット経路に `to_vec()` / `Vec::from(slice)` / ボディの deep clone が無い。
- [ ] E2E のキャッシュ（`/cached/*`）テストが通過。

## 依存・リスク

- `bytes` は既存依存。`IoBuf for Bytes` の有無を `src/runtime/buf.rs` で確認し、未実装なら追加。
- F-29（キャッシュのロック排除）と同じファイル群を触るため、まとめて実装すると効率的。

## 対応状況: 完了（メモリキャッシュ経路）

- `src/runtime/buf.rs`: `unsafe impl IoBuf for bytes::Bytes` を追加（read_ptr/bytes_init）。
- `src/cache/entry.rs`: `CacheStorage::Memory(Arc<[u8]>)` → `CacheStorage::Memory(Bytes)`。
  `memory_body()` は `Option<&Bytes>` を返す。`size()`/`memory_usage()`/`build_memory()` は
  `.len()`/`.into()` がそのまま使えるため変更不要。
- `src/http_utils.rs`: `build_cached_response_headers`（ヘッダーのみ）を追加し、
  `build_cached_response`（ヘッダー+ボディ）はそれを呼んでボディを連結する形に整理。
- `src/proxy.rs`: メモリキャッシュヒット（通常・stale）の送出を「ヘッダー write_all →
  ボディ(`Bytes`) write_all」の 2 段に変更。ボディは `Bytes::clone()`（O(1) refcount）で
  `WriteFuture` に渡され、レスポンスバッファへの memcpy が消滅（ゼロコピー配信）。

ディスクキャッシュ経路はディスク読込で 1 度コピーが入るため対象外（`build_cached_response`
を継続使用）。`writev`（scatter-gather）が実装されればヘッダー+ボディの 1 syscall 化が可能（残）。
