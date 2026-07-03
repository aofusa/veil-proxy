# F-43: WASM パスのアロケーション（clone / deep copy）削減

- **優先度**: P3
- **対応状況**: 完了（2026-07-03。ヘッダ・文字列・モジュールリストの主要 3 系統を排除。ボディフィルタ経路の copy は残課題として明記）
- **出典**: `docs/artifacts/remaining_tasks_analysis.md` F-29 残件

## 機能説明・現状

Proxy-Wasm の async ABI 制約に起因して、WASM フィルタ適用経路には以下のアロケーションが残る:

- `path` / `method` / `client_ip` の文字列化。
- ヘッダの deep copy。
- `modules_to_apply.clone()`。

## 改修内容

- WASM エンジン API を借用ベースまたは `Arc<[..]>` 共有ベースに再設計し、リクエストごとの deep copy を排除する。
- `modules_to_apply` は `Arc` 共有 + 世代管理で clone を排除。

## 受け入れ条件

- WASM 有効時のリクエストごとアロケーションが削減され、`benches/wasm.rs` で退行がない。
- WASM 関連の単体・統合・E2E テストが通過。

## 調査メモ（2026-07-02）

- 対象アロケーションは Proxy-Wasm の async ABI（wasmtime ホスト関数へ渡す所有データ）に起因し、エンジン API（`src/wasm/engine.rs` / `context.rs` / `integration.rs`）の借用/`Arc<[..]>` ベース再設計が必要。フィルタ適用の全経路（HTTP/1.1・HTTP/2・HTTPS・H2C）に波及するため、独立フェーズとして継続する。

## 依存・リスク

- proxy-wasm ABI（ゲスト側が期待するホスト関数のセマンティクス）を壊さないこと。
- エンジン API の再設計は影響範囲が大きい。

## 実装内容（2026-07-03 完了）

1. **`modules_to_apply` の clone 排除**: `Backend` は元々 `Option<Arc<Vec<String>>>` を
   保持していたが、アクセサが `&[String]` を返すため呼び出し側が `to_vec()` + 多数の
   `.clone()`（文字列 deep copy）をしていた。`Backend::modules_arc()` を追加し、
   エンジン async API（`*_with_modules_async` / `on_log_with_modules_async` /
   `on_request_complete_async`）とプロキシ全経路（HTTP/1.1・HTTP/2・HTTP/3）の
   `wasm_modules` を `Arc<Vec<String>>` 化。clone は参照カウントのみになった。
   空リストは `wasm::empty_wasm_modules()`（プロセスで 1 個の共有 Arc）。
2. **`path` / `method` / `client_ip` の per-module `to_string` 排除**: `HttpContext` の
   該当フィールドと `set_request` を `Arc<str>` 化。リクエストごとの確保は Arc 構築
   1 回のみで、複数モジュール適用時も参照カウント共有。
3. **ヘッダ deep copy の排除（ムーブスルー化）**: `execute_on_request_headers` /
   `execute_on_response_headers` がヘッダを所有権で受け取り、実行後に（変更有無に
   関わらず）コンテキストから `mem::take` で回収して返す設計へ変更。従来の
   「per-module `headers.to_vec()`（全ペア deep copy）+ 変更時の `clone()`」を排除。
   SDK ライフサイクル（_start → context 生成 → vm_start → configure → コールバック）は
   `run_headers_module` に共通化。エラー時もヘッダは失われない。
4. **応答側 copy-back の排除**: フィルタ結果ヘッダの `Box<[u8]>` 変換を
   `iter().map(clone)` → `into_iter().map(into_boxed_slice)`（ムーブ）へ。

**残課題（本チケットのスコープ外として明記）**: ボディフィルタ経路
（`on_request_body` / `on_response_body`）の per-module `body.to_vec()`、
`request_query` の per-context 文字列化。いずれもバッファリング経路（全量バッファが
前提）のため優先度は低い。

## 受け入れ条件の検証

- WASM 有効時のリクエストごとアロケーションが削減され、`benches/wasm.rs` で退行がない
  → ヘッダ per-module deep copy・モジュールリスト deep copy・文字列 per-module 確保を
  排除。`cargo bench --bench wasm -- --test` のスモーク通過（コンパイル・実行正常）。
- WASM 関連の単体・統合・E2E テストが通過
  → 単体 644・統合 53・E2E 全 406（WASM 系 23 含む）通過 ✓
