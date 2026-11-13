# B-363: 接続の集約（F-303）の配線が Linux 以外で使われず、clippy の `-D warnings` が通らない

**状態: 修正済み（feat/beat-nginx、2026-11-13）**

[F-303](../features/F-303-concentrate-connections.md) の全プラットフォームの確認（macOS の実機、`full-macos`）で見つけた。

## 事象

`cargo clippy --all-targets --no-default-features --features full-macos -- -D warnings` がライブラリで 6 件の警告を出して失敗する。
試験（単体 1200・統合 63・E2E 591・自己完結型 E2E 51）はすべて通る。

- `active_workers` の Linux 以外のフック（`noop`）の `park_begin`・`park_end` を誰も呼ばない（呼ぶのは Linux の実行器、io_uring と epoll だけ）。
- `metrics::set_active_workers`・`ACTIVE_WORKERS` を呼ぶのは Linux の集約の制御だけ。
- `WorkerEnv::concentrate_connections` を読むのは Linux の TCP リスナーの起動だけ。

kqueue（BSD・macOS）・WSAPoll（Windows）のビルドに共通で、Linux の既定のビルドと CI（Linux）の clippy では見えない。

## 対応

Linux でしか使わないものを Linux に限る（`allow(dead_code)` は使わない）。

- `noop` から `park_begin`・`park_end` を外す。
- `set_active_workers`・`ACTIVE_WORKERS` と `WorkerEnv::concentrate_connections`（とその初期化）に `#[cfg(target_os = "linux")]` を付ける。

挙動は変わらない（Linux 以外では集約もメトリクス `veil_proxy_active_workers` も元から無い。文書は Linux の TCP リスナーのものと書いている）。

## テスト

- macOS の実機（`full-macos`）の `cargo clippy --all-targets -- -D warnings` が通る（修正前は上の 6 件で失敗）。Linux の `full` の clippy も通る。
