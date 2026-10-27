# B-257: Wasm の gRPC 実行器が受信 DATA に WINDOW_UPDATE を返さず、プール接続が累計 64KB で止まる。16KB 超のメッセージを 1 つの DATA フレームで送って接続ごと落とす

**状態: 未修正（2026-10-28 起票）**

## 出典

2026-10-27〜28 のセキュリティレビュー 3 回目の GE-01・GE-06（`docs/artifacts/security/analysis/09-remaining-areas-grpc-mesh.md`、git 管理外）。

## 事象

`src/wasm/host/grpc_executor.rs`（F-139 の専用スレッドの HTTP/2 状態機械）で:

1. **受信側のフロー制御の補充が無い。** `ActiveCall::handle_frame` の `FrameType::Data` の腕は DATA を gRPC デコーダへ渡すだけで、
   `encode_window_update` を一度も呼ばない。相手（gRPC サーバ）から見た送信ウィンドウは接続・ストリームとも初期値 65,535 のままなので、
   (a) 64KB を超える応答は完了できず `DEADLINE_EXCEEDED`、(b) プールされた接続（`grpc_pool.rs`）は **累計 64KB の DATA を受けると以後の全呼び出しが止まる**。
   デッドラインで `goaway=true` になって接続が捨てられ、次の新規接続で復帰するまでの呼び出しは全部タイムアウトする。
2. **フレームの分割が無い。** `poll_once` はゲストの 1 メッセージを `encode_data_into` で **1 つの DATA フレーム**にする。5 + len が相手の
   `SETTINGS_MAX_FRAME_SIZE`（既定 16,384。実行器は相手の値を読まない）を超えると相手は `FRAME_SIZE_ERROR` の接続エラーを返し、
   同じプール接続上の他の呼び出しも巻き添えになる。ゲストが 16KB 超のメッセージを `proxy_grpc_send` するだけで起きる。

## 影響

- P2（可用性）: Proxy-Wasm の `proxy_grpc_call`/`proxy_grpc_stream` を使うプラグインで、応答やメッセージが 16KB〜64KB を超えると機能しない。
  データプレーンには影響しない（専用スレッド）。

## 改修案

- 受信した DATA の長さを接続・ストリームごとに積み、閾値（初期ウィンドウの半分）を超えたら `WINDOW_UPDATE` を両方に送る（`http2::upstream_mux` の
  消費連動の補充と同じ方針。F-116）。ストリームが閉じても接続側の補充は続ける。
- 送信は相手の `SETTINGS_MAX_FRAME_SIZE`（受けた SETTINGS から保持。無ければ 16,384）で分割する。`encode_data_into` に分割版を足すか、呼び出し側で chunk する。
- 長期: `http2::upstream_mux` と状態機械を共有する（`docs/artifacts/security/analysis/10-security-architecture-review.md` §7）。

## 受け入れ条件

- 単体（`spawn_mock_grpc_server` を拡張）: 70KB の応答を返すケースが同じプール接続で 2 回連続で成功する。17KB のメッセージを送るケースで相手が
  `FRAME_SIZE_ERROR` を返さない。
- 既存の gRPC の E2E（Wasm のプラグイン）が通る。

## 関連

- [F-139](../features/F-139-wasm-grpc-call-execution.md)、[F-106](../features/F-106-h2c-backend-connection-pooling.md)（再利用で顕在化した送信ウィンドウの枯渇。受信側はその鏡像）
