# B-276: `[http2] stream_idle_timeout_secs` が読み込まれるだけでどこにも使われていない

**状態: 修正済み（v0.10.1）**

## 出典

[B-274](B-274-route-client-timeouts-h1-only.md) の修正で HTTP/2 の要求の受信のタイムアウトを足すときに、既存の設定を探して見つけた（コードで確認）。

## 事象

`examples/config.toml` とガイドは `[http2] stream_idle_timeout_secs` を「リクエストが完了しないストリームの切断までの時間」と書くが、
値は `Http2Settings` に読み込まれるだけで、どのストリームの処理からも参照されていなかった。要求本文を送らずに止めたストリームは、
接続が他のストリームで動いている限り残り続けた（`max_concurrent_streams` まで 1 接続で積める）。

## 修正（v0.10.1）

- 要求の受信が止まったストリームを接続の見回り（`collect_stalled_receiving_streams`、接続ごとのタイマー）で集め、`RST_STREAM(CANCEL)` で
  打ち切る。秒数はストリームのルートの `client_body_timeout_secs`（B-274）、ルートの無いストリームは `stream_idle_timeout_secs`
  （0 で無効）。I/O ごとにタイマーを張らない（`src/http2/stream.rs`・`src/http2/connection.rs`）。
- 単体試験 `collect_stalled_receiving_uses_stream_then_default_timeout`、統合試験 `b274_client_body_timeout_applies_to_h2_and_h3`。
- `examples/config.toml`・`docs/guide/protocols.md`（英日）に意味を書いた。

## 関連

- [B-274](B-274-route-client-timeouts-h1-only.md)、[B-119](B-119-http2-no-idle-timeout.md)
