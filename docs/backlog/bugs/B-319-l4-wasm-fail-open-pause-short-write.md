# B-319: L4 の Proxy-Wasm が失敗・Pause で素通りし、ショートライトの再送で panic する

**状態: 修正済み（v0.11.0）**

## 事象

v0.11.0 前のセキュリティスキャン（Claude Security の F10・F11・F19）で判明。

- **F10**: L4 のネットワークフィルタ（`proxy_on_new_connection`・`proxy_on_downstream_data`・
  `proxy_on_upstream_data`）がトラップ・fuel 切れ・タイムアウト・インスタンス作成の失敗で終わると、ログに出すだけで
  データをそのまま転送していた。HTTP の段は `failure_policy`（既定 `fail_closed`、F-293）に従うのに、L4 は見ていなかった。
- **F19**: フィルタが `Pause`（判断にもっとデータが要る）を返しても、警告して転送していた。フィルタは read ごとに
  新しいインスタンスで走るので、2 回の read にまたがる語を止められなかった。
- **F11**: L4 の中継のショートライトの再送は、元の長さ `n` を基準に `copy_within(written..n, 0)` していたので、
  2 回目のショートライトで縮めたバッファの外を指して panic した。Linux 以外では c→u の方向を
  `catch_unwind` の無いタスクで走らせていたので、ワーカーのスレッドごと落ちた。

## 修正

- L4 の失敗は `on_failure`（`failure_policy`）に通し、拒否なら接続を閉じる（`phase` は `new_connection`・
  `downstream_data`・`upstream_data`）。
- `Pause` のデータは転送せずに持ち（`PauseHold`、1 MiB まで）、次に読んだ分を後ろに付けて渡し直す。超えたら閉じる。
- ショートライトは `shift_unsent` で今のバッファを基準に詰める。Linux 以外の c→u のタスクは
  `spawn_with_panic_catch` で走らせ、panic しても相手の方向が待ち続けないよう完了を通知する。

文書: docs/guide/wasm.md（英日）の Failure Handling。単体試験 `l4::proxy::tests::pause_hold_prepends_held_bytes`・
`test_copy_within_simulates_short_write_retry`。

## 関連

- docs/artifacts/security/v0110-claude-security/（F10・F11・F19）
- F-293（`failure_policy`）
