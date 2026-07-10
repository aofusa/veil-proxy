# B-41: HTTP/3 gRPC 成功応答（ボディあり）でクライアントがハングする

## 事象

E2E `test_grpc_over_http3_metadata` / `test_grpc_over_http3_error_handling` /
`test_grpc_over_http3_streaming` がタイムアウトする。

- 無効な protobuf ボディの Unary は即座に 200 + `grpc-status: 13` を返す（ボディ空）。
- 有効な protobuf で上流が **応答ボディ + trailers** を返す経路では、クライアントが
  `recv_data` / 接続タイムアウトまでハングする。

### 再現手順

1. E2E 環境起動: `./tests/e2e_setup.sh start`
2. `cargo test --test e2e_tests --features full -- --exact test_grpc_over_http3_metadata --nocapture`
3. 30s で ntest timeout

## 影響

- gRPC over HTTP/3 の成功 Unary / ストリーミングが実用不能（クライアント永久待ち）。
- エラーのみ（ボディ空）の経路は偶然通るため、基本疎通テスト（`test_grpc_over_http3`）は
  不正 protobuf でパスしてしまい、バグを隠蔽していた。

## 調査メモ

`src/http3_server.rs` の `send_grpc_trailers_internal` が trailers 送出に
`h3_conn.send_response(..., fin=true)` を使用していた。

quiche 0.24 では **初期応答ヘッダ専用**が `send_response`、**後続ヘッダ/trailers は
`send_additional_headers`** が正しい API。2 回目の `send_response` は失敗（debug ログのみ）し、
`send_body(..., fin=false)` のままストリームが閉じられない。

ボディが空のエラー応答では trailers 経路が偶然機能する／ヘッダマージで status が見えるため
早期完了し、ボディあり成功経路のみハングが顕在化した。

## 改修案

1. `send_grpc_trailers_internal` を `send_additional_headers(..., fin=true)` に変更
2. 応答ヘッダから `grpc-status` / `grpc-message` を除外し trailers にのみ載せる（重複回避）
3. `test_grpc_over_http3` を正しい SimpleRequest protobuf に修正し成功パスを固定
4. 単体テストで trailer 組み立ての不変条件を固定

## 関連

- B-39（H2C gRPC 502）
- F-93（レポート残件 E2E）

## 対応状況

進行中（fix/test）
