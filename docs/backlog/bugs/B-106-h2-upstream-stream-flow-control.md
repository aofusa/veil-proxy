# B-106: 上流 HTTP/2 クライアントがストリーム単位のフロー制御を無視する

**状態: 完了（feat/v080-limitations、F-174）**

## 事象

`H2cClient`（`src/http2/client.rs`）は送信を接続レベルのウィンドウだけで制御し、ストリームレベルの
WINDOW_UPDATE と SETTINGS_INITIAL_WINDOW_SIZE を無視する。1 接続で 1 ストリームずつ直列に使う。

## 影響

- ストリームの初期ウィンドウが 64KB の上流（grpc-go の既定など）へ 64KB を超えるリクエスト本文を送ると、
  FLOW_CONTROL_ERROR（RST_STREAM / GOAWAY）になる潜在的なプロトコル違反。
- 多重化しないため、同時要求数ぶん上流接続を張る。

## 改修

多重化対応の上流 HTTP/2 クライアント（F-174）で、接続・ストリーム両方のウィンドウを追跡する。

## 結果（2026-10-09）

F-174 の多重化クライアントで、要求本文の送信を接続・ストリーム両方のウィンドウで制御し、足りなければ WINDOW_UPDATE を
待つ（SETTINGS_INITIAL_WINDOW_SIZE の変化も既存ストリームへ反映する）。

- 単体: `f174_multiplexes_and_respects_stream_window`（ストリームウィンドウ 16KB の模擬サーバへ 40KB × 2 ストリーム）。
- E2E: `test_b106_grpc_large_request_respects_upstream_flow_control`（1.5MB の gRPC 要求を HTTP/2 で受けて tonic の h2c
  上流へ。上流の接続ウィンドウ 1MB を超える）。旧クライアントでは 502 になることを確認済み。
