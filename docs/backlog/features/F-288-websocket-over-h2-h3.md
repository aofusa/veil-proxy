# F-288: HTTP/2・HTTP/3 の上の WebSocket（RFC 8441・RFC 9220 の拡張 CONNECT）をプロキシとハンドラで受ける

**状態: 未着手（2026-11-04 起票。v0.10.1 で文書を「WebSocket は HTTP/1.1 だけ」に直した）**

## 今の状態

- WebSocket はプロキシ（`proxy::websocket`、`Upgrade: websocket` の検出）もハンドラ（F-261・F-262）も HTTP/1.1 だけ。HTTP/2・HTTP/3 は
  `SETTINGS_ENABLE_CONNECT_PROTOCOL` を広告せず、ハンドラのルートへの拡張 CONNECT は 400（`library.md` に明記）。
- プロキシの文書（`protocols.md` の WebSocket Support）はプロトコルの制限を書いていない。
- ブラウザは HTTP/2 のサーバが拡張 CONNECT を広告しなければ HTTP/1.1 の別の接続で WebSocket を張るので、今でも使えはする。ただし HTTP/2 の接続を
  使い回せず、HTTP/3 だけを受ける構成（UDP のみ・HTTP/3 専用のリスナー）では WebSocket が使えない。
- [F-258](F-258-v0100-plan.md)・F-261・F-262 では範囲外として外した。チケットが無かったので、[B-266](../bugs/B-266-head-local-responses-send-body.md) と
  同じ調査（プロトコル間の機能の差）で起票する。

## 改修内容

- HTTP/2: `SETTINGS_ENABLE_CONNECT_PROTOCOL = 1` を送り、`:method = CONNECT`・`:protocol = websocket` のストリームを受ける。プロキシは上流へ
  HTTP/1.1 の Upgrade（または上流が HTTP/2 なら拡張 CONNECT）で張り、ストリームの DATA と上流の TCP を双方向に中継する（フロー制御はストリームの窓）。
  ハンドラは `Request::upgrade_protocol()` で `websocket` を返し、既存の WebSocket の処理（フレーム・ping・圧縮）をストリームの上で動かす。
- HTTP/3: `SETTINGS_ENABLE_CONNECT_PROTOCOL` を送り、拡張 CONNECT のリクエストストリームで同じことをする（quiche の h3 の設定・メインループの
  ダーティ集合に乗せる。B-97 と同じくメインループで待たない）。
- `websocket_idle_timeout_secs`・接続数の上限・メトリクス（`veil_proxy_websocket_connections`）を HTTP/1.1 と同じに数える。
- 文書: `protocols.md`・`library.md`（英日）。実装までは `protocols.md` に「HTTP/1.1 のみ」と書く。

## 受け入れ条件

- E2E: h2c・HTTP/2・HTTP/3 の拡張 CONNECT で、プロキシのルート（上流 HTTP/1.1 の WebSocket サーバ）とハンドラのルートの echo・ping・close が通る。
  HTTP/1.1 の WebSocket は今のまま。h2spec・h3spec の既存の項目が通る。
- WebSocket の無いルートの性能が変わらない。

## v0.10.1 での扱い

- 実装は見送り、`docs/guide/protocols.md`・`migration-nginx.md`・`performance.md`（英日）に WebSocket は HTTP/1.1 だけと書いた
  （文書と挙動の食い違いを無くす。拡張 CONNECT の実装はこのチケットで続ける）。

## 関連

- [F-261](F-261-handler-http11-upgrade.md)、[F-262](F-262-handler-websocket-rust.md)、[F-258](F-258-v0100-plan.md)、[B-266](../bugs/B-266-head-local-responses-send-body.md)
