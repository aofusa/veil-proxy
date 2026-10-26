# B-197: WebSocket・アップグレード接続に既定のタイムアウトが無く、途中のフレームで大きな受信バッファを握れる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の API・Wasm のレビュー（docs/artifacts/v0100/review-api-wasm.md） #3（2026-10-26）。

## 事象

- `upgrade_idle_timeout_secs`・`websocket_idle_timeout_secs`・`websocket_ping_interval_secs` の既定がすべて 0（無効）で、
  何も送らない WebSocket・アップグレード接続を永久に保持できた。
- フレームヘッダが宣言した長さ（最大 `websocket_max_frame_size`）を先に確保してから読んでいたため、ヘッダだけ送って
  止まる接続 1 本ごとに最大 `websocket_max_frame_size` のバッファを握れた。

## 修正

- 既定を `upgrade_idle_timeout_secs = 300`・`websocket_ping_interval_secs = 30`・`websocket_idle_timeout_secs = 120` にした
  （0 で従来どおり無効）。
- 受信の確保を宣言した長さではなく、受けた量に合わせて倍々に増やす（`read_want`。最小 `READ_MIN`）。

## 試験

- `src/api/ws/mod.rs` の `read_reserve_follows_received_bytes`、`src/api/app.rs` の `api_settings_defaults`（既定値）。

## 実施内容

- `src/api/app.rs`、`src/api/ws/mod.rs`、`src/api/upgrade.rs`。
- 文書: `docs/guide/library.md`・`docs/guide/library-production.md`（英日）、`examples/config.toml`。
