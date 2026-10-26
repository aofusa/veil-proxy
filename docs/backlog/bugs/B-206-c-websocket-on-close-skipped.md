# B-206: C の WebSocket: on_close が呼ばれないまま veil_ws_t が解放され得る

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #5（2026-10-26）。

## 事象

- C の契約は「`veil_ws_t` は `on_close` が戻るまで有効」だが、接続が on_open の後・ループの外（同期のセッションの実行を
  始める前のエラー、ワーカーの停止で Future が drop された等）で終わると `on_close` が呼ばれずにハンドルが解放された。

## 修正

- Rust 側: 同期のセッションを `CloseOnDrop` で包み、正常経路で `on_close` を呼ばずに drop されたら 1001 で `on_close` を
  ハンドラ用プールで呼ぶ。
- C 側: `CWsSession` が `on_close` を呼んだかを覚え、Drop で未呼び出しなら 1001 で呼ぶ（on_open で拒否したものは呼ばない）。

## 試験

- `src/api/ws/mod.rs` の `dropped_blocking_session_gets_on_close`（ループの前に捨てられた同期のセッションに 1001 で `on_close`）。
- C の側は `tests/api/library_e2e.sh c` の WebSocket（`examples/api/c-features`）。`CWsSession` の Drop の守りは `WsSender` を
  外から作れないため単体では試さない。

## 実施内容

- `src/api/ws/mod.rs`、`crates/veil-capi/src/ext.rs`・`include/veil.h`・`README.md`。
