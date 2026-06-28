# F-34: コネクション状態の Slab / Arena アロケーション

## 出典

`docs/artifacts/architecture_analysis_v4.md` / `v5.md`（Slab アロケーションの実装）。

## 概要

接続ごとの状態構造体（`HttpHandler` 等）の確保を、起動時に確保した固定長 Slab/Arena から
インデックス（トークン）で払い出す方式に変更し、実行時の `malloc`/`free` を排除する。

## 現状の問題

ホットパスのバッファ（`src/pool.rs` の `SafeReadBuffer`）は最適化済みだが、数万同時接続時、
接続ごとの状態構造体の確保がメモリアロケータ（mimalloc）のボトルネックになり得る。

## 改修内容

1. `src/pool.rs` を拡張し、`max_concurrent_connections` 上限に基づく固定長配列
   （`Box<[MaybeUninit<HandlerState>]>` + free-list）をスレッドローカルに確保。
2. 接続受理時にスロットを払い出し（O(1)）、切断時に free-list へ返却（O(1)）。
   トークン（インデックス）で状態を参照する。
3. Slab 枯渇時はヒープへフォールバック（接続上限を超えるバースト対策）。

## 受け入れ条件

- [ ] 全 E2E 回帰なし。
- [ ] 接続確立/切断のホットパスで状態構造体の `Box::new`/`malloc` が発生しない（Slab ヒット時）。

## 依存・リスク

- thread-per-core 設計のためスレッドローカル Slab が自然。`unsafe`（`MaybeUninit`）の不変条件を
  コメントで明示する必要がある。難易度高。
