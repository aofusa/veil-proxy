# F-61: WASM ボディフィルタ経路のアロケーション削減（F-43 残件）

## 出典

`docs/artifacts/completed_tasks_remaining_work_report.md`（F-43 残タスク）。

## 概要

WASM ボディフィルタ経路の copy（アロケーション）を排除する。ヘッダ・文字列・モジュール
リストのアロケーションは F-43 で排除済みだが、ボディは wasmtime ホスト関数へ所有データで
渡す制約上コピーが残っている。

## 現状

- Proxy-Wasm の async ABI（wasmtime へ渡す所有データ）の制約で、ボディを `to_vec()` して
  渡している。HTTP/1.1・HTTP/2・HTTPS・H2C の全フィルタ経路に波及する。

## 改修内容（案）

- エンジン API を借用ベース or `Arc<[u8]>` ベースへ再設計し、ボディの deep copy を排除。
- 全プロトコルのフィルタ適用経路を同時に更新。

## 受け入れ条件

- [ ] WASM ボディフィルタ経路に `to_vec()` / ボディ deep copy が無い。
- [ ] `/wasm/*` の E2E が全プロトコルで回帰なし。

## 依存・リスク

- wasmtime host function ABI の所有権制約。大きめの再設計。P3・独立フェーズ。

## 実装（2026-07-05）

エンジンのボディフィルタ API を **`bytes::Bytes` + Copy-on-Write** ベースへ再設計し、
deep copy を排除した。

1. **`BodyBuffer` 型を新設**（`src/wasm/context.rs`）: `Shared(Bytes)` / `Owned(Vec<u8>)`
   の CoW enum。ホストからは参照カウント共有の `Bytes` をゼロコピーで受け取り、
   モジュールが `proxy_set_buffer_bytes` で書き換えた時のみ `Owned` へ昇格する。
   `HttpContext::{request_body, response_body}` を `Vec<u8>` から `BodyBuffer` へ変更。
2. **エンジン API**（`src/wasm/engine.rs`）: `on_request_body_with_modules(_async)` /
   `on_response_body_with_modules(_async)` と `BodyFilterResult::Continue` を
   `Bytes` ベースへ変更。
   - モジュール未登録時: 受け取った `Bytes` をそのまま返却（コピー 0 回）
   - フィルタチェーン: per-module の受け渡しは `Bytes::clone()`（O(1) 参照カウント）
   - 変更検出時: store から `mem::take` でムーブ取り出し → `into_bytes()`
     （従来は `Vec::clone` の deep copy）
3. **ホスト関数**（`src/wasm/host/buffers.rs`）: `proxy_get_buffer_bytes` の
   ボディ取得を `BodyBuffer::share()`（O(1)）へ変更（**従来はホスト関数呼び出しごとに
   ボディ Vec 全体を clone していた**）。書き込みは `to_mut()` の CoW。

除去されたコピー（1 チャンク・モジュール n 個あたり）: 呼び出し境界 1 + チェーン初期化 1 +
per-module `set_*_body` n + 変更時 clone 1 + ホスト読取ごとの clone（→ すべて 0 or CoW 昇格時のみ）。

注記: 本 API はエンジン内で完結しており、データプレーンへの配線（Pause/resume）は
F-62 のスコープ。配線時にゼロコピー契約（`Bytes` 受け渡し）をそのまま利用できる。

テスト: `wasm::tests::body_buffer_tests`（共有読取のポインタ同一性 / CoW 昇格と元
バッファ不変 / Owned→Bytes のアロケーションムーブ）、既存 wasm 単体 100 件、
WASM E2E 23 件回帰なし。

## 対応状況: 完了（2026-07-05）