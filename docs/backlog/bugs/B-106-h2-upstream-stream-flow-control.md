# B-106: 上流 HTTP/2 クライアントがストリーム単位のフロー制御を無視する

**状態: 対応中（feat/v080-limitations、F-174 で解消）**

## 事象

`H2cClient`（`src/http2/client.rs`）は送信を接続レベルのウィンドウだけで制御し、ストリームレベルの
WINDOW_UPDATE と SETTINGS_INITIAL_WINDOW_SIZE を無視する。1 接続で 1 ストリームずつ直列に使う。

## 影響

- ストリームの初期ウィンドウが 64KB の上流（grpc-go の既定など）へ 64KB を超えるリクエスト本文を送ると、
  FLOW_CONTROL_ERROR（RST_STREAM / GOAWAY）になる潜在的なプロトコル違反。
- 多重化しないため、同時要求数ぶん上流接続を張る。

## 改修

多重化対応の上流 HTTP/2 クライアント（F-174）で、接続・ストリーム両方のウィンドウを追跡する。
