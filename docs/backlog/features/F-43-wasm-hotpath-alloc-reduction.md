# F-43: WASM パスのアロケーション（clone / deep copy）削減

- **優先度**: P3
- **対応状況**: 未着手
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
