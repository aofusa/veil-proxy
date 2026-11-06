# F-288: HTTP/2・HTTP/3 の上の WebSocket（RFC 8441・RFC 9220 の拡張 CONNECT）をプロキシとハンドラで受ける

**状態: 実装済み（2026-11-05。2026-11-04 起票。上流の HTTP/2 の拡張 CONNECT・トンネルの本数の上限は範囲外。下の「修正」）**

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

## 設計（2026-11-05、v0.11.0 の計画で詳細化）

調査の全文は `docs/artifacts/v0110/review-07-websocket.md`（git 管理外）の §2。行番号は f8dc80de 時点。

- **先に直すもの**: HTTP/1.1 の中継の [B-306](../bugs/B-306-websocket-relay-drops-data-iouring.md)（io_uring でデータを失う・交互のポーリング・書き込みの時間切れ）。
  その作り直しで、両方向を同時に待つ中継の部品ができ、HTTP/2・HTTP/3 でも使う。
- **HTTP/2**: `src/http2/settings.rs` に `SettingsId::EnableConnectProtocol = 0x8`。送り、受け取った値が 0・1 以外か 1 → 0 の変更は接続エラー。
  `validate_request_headers`（`connection.rs:1170`）は `:protocol` を CONNECT のときだけ、0x8 を広告した後だけ許し、拡張 CONNECT には `:scheme`・`:path`・
  `:authority` を求める。要求の部品に `protocol` を持たせる。トンネルのストリームは `stream_idle_timeout_secs`・`set_stream_recv_timeout` の対象から外し、
  `websocket_idle_timeout_secs` で切る。
- **上流の HTTP/2 の多重化接続**（`upstream_mux`）: 相手の 0x8 を記録する。今は SETTINGS の前にストリームを開く（`upstream_mux.rs:216`）ので、拡張 CONNECT の
  ストリームはサーバの SETTINGS が届くまで待たせる。双方向の CONNECT はメッシュの HBONE で既に扱っている。上流が 0x8 を広告しなければ 502。
- **HTTP/3**: vendoring の quiche は `h3::Config::enable_extended_connect`（`third_party/quiche/src/h3/mod.rs:641`）と `extended_connect_enabled_by_peer`（:1803）を
  持つが `:protocol` を検査しない。`init_h3`（`http3_server/mod.rs:1081`）で有効にし、`h3_check_field_section` に PROTOCOL のビットと「広告済み」を足す
  （既存の否定の試験 `mod.rs:2293` は残す）。`classify` でトンネルと判定し、`Decision::Stream`/`ProxyStream` で `RecvWatch` 無しに流す（B-97 を守る。
  メインループで待たない）。素の CONNECT（`:protocol` 無し）は [B-308](../bugs/B-308-websocket-shutdown-metrics-docs-h3-connect.md) の W-12。
- **中継のタスク**（新しい `src/proxy/ws_tunnel.rs`、HTTP/2・HTTP/3 で共有。既存の `H2RespMsg`/`RespMsg` のチャネルと `req_rx` を使う）:
  - ルーティング・セキュリティの検査は GET として行う（RFC 8441 §5）。
  - 上流 HTTP/1.1: `GET` + `Upgrade: websocket` を組み、RFC 8441 のクライアントは `Sec-WebSocket-Key` を送らないので**プロキシが作り、返った Accept を検査する**。
    101 をクライアントへは 200 にし、`Sec-WebSocket-Protocol`・`Sec-WebSocket-Extensions` を残す。101 以外は本文ごと返す。上流は `readable()` で、チャネルと同時に待つ
    （`futures::join!` を使わない。F-173）。本文は `Bytes` のまま渡す。
  - フロー制御は既存の消費連動のウィンドウ補充（h2）と quiche のクレジット（h3）。END_STREAM・fin は上流の `shutdown(Write)`、エラー・アイドルの満了は
    RST_STREAM(CANCEL)・H3_REQUEST_CANCELLED（B-86 と同じく切り詰めを成功に見せない）。
  - permessage-deflate は触らずに通す。
- **ハンドラ**: `Request::upgrade_protocol` は HTTP/2・HTTP/3 で `:protocol` を返す。`Upgraded` を enum（H1、チャネルのストリーム。キャンセル安全）にする。
  `check_response` は拡張 CONNECT を許し、101 ではなく 200 で答える。`WebSocketUpgrade::from_request` は CONNECT + `:protocol` を鍵無しで受ける。C（`veil_ws_accept`）・
  Python は同じ抽出器を使うのでそのまま効く。
- **横断**: リスナーごとの `extended_connect` の設定（広告するとブラウザが全部そちらへ移るので、最初は既定オフも検討）。トンネルを
  `max_concurrent_connections` に数えるか、トンネルの上限と接続あたりの上限を足す（1 本の h2 接続から 100 本のトンネル＝上流の TCP 100 本を張れる）。
  graceful shutdown（F-187）でトンネルを閉じる。メトリクスは [B-308](../bugs/B-308-websocket-shutdown-metrics-docs-h3-connect.md) の W-10
  （`veil_proxy_websocket_connections` はハンドラだけを数えている。上の「HTTP/1.1 と同じに数える」はこの前提が誤り）。
- **試験**: 受け入れ条件に加えて、h2spec・h3spec（CONNECT 以外の `:protocol` は malformed のまま）、WebSocket の無い要求の性能（分岐 1 つ・確保なし）の交互 A/B、
  epoll の E2E。
- **PR の順**: B-306 → HTTP/2 → HTTP/3 → 上流の h2 の多重化接続 → 文書。

## 修正（2026-11-05）

- **設定**: `[http2] enable_connect_protocol`・`[http3] enable_connect_protocol`（bool、既定 `false`）。`true` のときだけ
  `SETTINGS_ENABLE_CONNECT_PROTOCOL` を広告する（広告するとブラウザが WebSocket をその接続へ移すので既定は無効）。
- **HTTP/2**（`src/http2/settings.rs`・`connection.rs`・`stream.rs`）: `SettingsId::EnableConnectProtocol = 0x8` の送出（有効時だけ）と受信の検査
  （0・1 以外、1 → 0 は PROTOCOL_ERROR）。`validate_request_headers` は `:protocol` を広告済みの CONNECT だけに許し（トークンであること）、
  拡張 CONNECT には `:scheme`・`:path`・`:authority` を求める。それ以外は従来どおり malformed（h2spec の項目は不変）。
  拡張 CONNECT のストリームは `Stream::tunnel` で印を付け、`stream_idle_timeout_secs`・受信の時間切れの対象から外す。要求の部品に `protocol`。
  本文はストリーミングの計画（`plan = Some(0)`）で消費連動のウィンドウ補充のまま流す。
- **HTTP/3**（`src/http3_server/mod.rs`・`h3_fields.rs`）: `init_h3` で quiche の `enable_extended_connect`。`h3_check_field_section` に `:protocol`
  （広告済み・CONNECT・`:scheme`/`:path`/`:authority` 必須・トークン）を足した（既存の否定の試験は残し、拡張 CONNECT の試験を足した）。
  `classify` は `:protocol` 付きの CONNECT を `Decision::Tunnel` にし、中継は `runtime::spawn` のタスク（メインループで待たない。B-97）。
  `:protocol` の無い CONNECT は従来どおり 405（B-308）。
- **中継**（新しい `src/proxy/ws_tunnel.rs`、HTTP/2・HTTP/3 で共有）: ルーティング・`[route.security]`・Proxy-Wasm の要求ヘッダは `GET` として行う。
  `Proxy` のルートは上流へ常に HTTP/1.1 の `GET` + `Upgrade: websocket`（`Sec-WebSocket-Key` を veil が作り、`Sec-WebSocket-Accept` を検査）。
  101 は 200 にし、`Sec-WebSocket-Protocol`・`Sec-WebSocket-Extensions` を残してルートの応答ヘッダの方針を通す（B-307 と同じ）。101 以外の応答は
  状態・ヘッダ・本文を返し、本文の切り詰めはリセット。両方向は 1 つの `poll_fn` でチャネルと上流の readiness を同時に待つ（`futures::join!` を使わない。
  起床した側だけ poll する F-173 の形）。上流への書き込みは送り先のチャネルの空き（`poll_ready`）を見てから読む。クライアントの END_STREAM・fin は
  上流の `shutdown(Write)`、上流の EOF はストリームの終わり、エラー・`websocket_idle_timeout_secs`・graceful shutdown（`timeout_idle(.., true, ..)`）は
  RST_STREAM(CANCEL)・H3_REQUEST_CANCELLED。`websocket_tunnel_opened("h2")`・`("h3")` で数える。`websocket` 以外の `:protocol` は Proxy のルートでは 501、
  Proxy 以外のルートは 400。`proxy::websocket` から上流のパスの組み立て（`ws_upstream_path`）を切り出して共有した。
- **ハンドラ**（`handler` feature、`src/api/`）: `Request::upgrade_protocol()` は HTTP/2・HTTP/3 で `:protocol` を返す。`WebSocketUpgrade::from_request` は
  CONNECT + `:protocol: websocket` を鍵無しで受け、`on_upgrade` は 200（`Sec-WebSocket-Accept` 無し）。`Upgraded` を H1・ストリーム（チャネル。キャンセル安全な
  `peeked`）の enum にし、WebSocket・`switching_protocols` がストリームの上で動く（`:protocol` の不一致は 500）。`Upgraded::into_tokio_io()` は HTTP/1.1 だけ
  （ストリームの上では未対応）。
- **文書**: `protocols.md`（拡張 CONNECT の節）・`routing.md`・`library.md`・`configuration.md`・`observability.md`（`protocol` に `h2`・`h3`）・
  `migration-nginx.md`（英日）、`examples/config.toml`。
- **試験**: 単体（SETTINGS の広告・値の検査、`validate_request_headers` の拡張 CONNECT、`h3_check_field_section` の拡張 CONNECT、`fuzz_api` の
  smoke、`ws_tunnel` の Accept の計算・鍵の生成・上流への要求・クライアントへのヘッダ）。E2E（`tests/e2e_tests/tickets.rs`）: HTTP/2（TLS）・h2c の
  拡張 CONNECT で `/ws/*` の WebSocket のエコーを往復（200・Accept 無し・ランダムな長さのフレーム 200 個・END_STREAM で終わる）、未知の `:protocol` の 501、
  `/ws-idle/*` のアイドルで RST_STREAM(CANCEL)、HTTP/3 の拡張 CONNECT がトンネルまで届くこと（`connect-udp` で 501）。

### 範囲外・残り

- **HTTP/3 の WebSocket の往復の E2E**: テストの h3 クライアント（`h3` 0.0.8）の `Protocol` が `webtransport`・`connect-udp` だけで `websocket` を送れない。
  HTTP/3 は検査・振り分け・応答の経路を E2E（501）で、中継の部品は HTTP/2 と共有のため HTTP/2 の E2E と単体試験で見ている。
- **上流の HTTP/2 の拡張 CONNECT**（`upstream_mux`）: 上流へは常に HTTP/1.1 のアップグレード（`use_h2c` のルートでも）。
- **トンネルの本数の上限**: 接続あたりは `max_concurrent_streams` で抑えるが、トンネル専用の上限・`max_concurrent_connections` への計上は足していない。
- **ハンドラのライブラリの E2E**（拡張 CONNECT でのハンドラの WebSocket）: 単体の範囲の確認だけ。

## 同じ調査で見つけた HTTP/1.1 の WebSocket の不具合

- [B-306](../bugs/B-306-websocket-relay-drops-data-iouring.md)（High）、[B-307](../bugs/B-307-websocket-upgrade-handling.md)（Medium）、
  [B-308](../bugs/B-308-websocket-shutdown-metrics-docs-h3-connect.md)（Low）。
- 「v0.10.1 での扱い」の `performance.md` は英日とも WebSocket の記述が無い（B-308 で足す）。

## 関連

- [F-261](F-261-handler-http11-upgrade.md)、[F-262](F-262-handler-websocket-rust.md)、[F-258](F-258-v0100-plan.md)、[B-266](../bugs/B-266-head-local-responses-send-body.md)
