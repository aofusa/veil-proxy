# F-213: HTTP/2 の PING の受け付け方針（gRPC の keepalive の強制）

**状態: 未着手（F-209 #10 から切り出し、2026-10-14）**

## 背景

gRPC のサーバ（grpc-go・grpc-java 等）は、クライアントの keepalive の PING が短すぎる・データの無い間に送られ続けるとき
`GOAWAY`（`ENHANCE_YOUR_CALM`、debug data `too_many_pings`）で接続を切る（既定: 5 分に 2 回まで、ストリームが無い間は
送らせない）。veil の HTTP/2 サーバはすべての PING に ACK を返し、頻度の上限が無い。API ライブラリの gRPC（F-197）でも
プロキシでも同じ HTTP/2 の実装（`src/http2/`）を通るので、プロキシの挙動の変更になる。

## やること

- `[http2]` に PING の受け付け方針を足す（例: `min_ping_interval_secs`（0 = 制限無し、既定は今と同じ 0）・
  `permit_ping_without_stream`）。超えたら `GOAWAY ENHANCE_YOUR_CALM` + debug data `too_many_pings`。
- ライブラリの既定（`src/api/default_config.toml`）は gRPC の既定（5 分・ストリーム無しは不可）に合わせるかを決める。
- PING flood（DoS）対策の既存の上限（`[http2]` の制御フレームの上限があればそれ）との関係を整理する。
- 試験: h2 の単体（PING の間隔と GOAWAY）・E2E（tonic のクライアントの `http2_keep_alive_interval` を短くして切られる・
  既定では切られない）。プロキシの perf A/B（PING の処理は要求ごとの経路ではない）。

## 関連

F-209（gRPC の未対応機能の洗い出し）、F-116（HTTP/2 アクターモデル）。
