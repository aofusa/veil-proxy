# F-42: buffering/handler.rs の非同期 FS 化（runtime::offload 適用）

- **優先度**: P1
- **対応状況**: 完了（2026-07-02）

## 完了メモ

- `disk_buffer::write_to_disk`（create_dir_all + write_all + fsync）、`read_from_disk`（metadata + read_exact）、`remove_disk_buffer`（unlink）の全ブロッキング FS 操作を `runtime::offload::offload()` 経由に移行。イベントループは POLL_ADD（eventfd）で完了待機し、決してブロックしない。
- `remove_disk_buffer` は async 化（呼び出し元なしのため互換影響なし）。
- write → read → remove のラウンドトリップ単体テストを追加。全単体テスト通過。
- **出典**: `docs/artifacts/remaining_tasks_analysis.md` F-29 残件

## 機能説明・現状

`src/buffering/handler.rs` のディスクバッファ操作は `std::fs`（`create_dir_all` / `File::create` / `metadata` / `File::open` / `remove_file`）の同期呼び出しで実装されており、リクエストボディのディスクバッファリング時にイベントループをブロックし得る。ホットパス絶対規則「同期処理の使用を一切禁止」に違反。

## 改修内容

- ディスクバッファの書き込み・読み出し・削除・メタデータ取得を `src/runtime/offload.rs` の `offload()`（専用スレッドプール + eventfd POLL_ADD）経由の非同期実行へ移行する。
- 新規 io_uring オペコードは追加しない（セキュリティサーフェス維持）。

## 受け入れ条件

- `buffering/handler.rs` のホットパス経路に同期 FS 呼び出しが残らない。
- バッファリング関連の単体・統合・E2E テストが通過。

## 依存・リスク

- offload はスレッドプールへの引き渡しで所有権が必要になるため、バッファの受け渡し設計（`Bytes` 化）に注意。
