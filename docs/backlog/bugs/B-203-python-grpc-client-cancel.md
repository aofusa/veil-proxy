# B-203: Python の gRPC クライアント: 取消しが応答ヘッダの前に効かず、ストリーミングの途中の取消しで END_STREAM を送る。大きな timeout で panic

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の Python・C バインディング・リリースのレビュー（docs/artifacts/v0100/review-bindings-release.md） #2 #9（2026-10-26）。

## 事象

- 取消し（asyncio のタスクの cancel・`GrpcCall` の drop）は応答のメッセージを待つ間しか見ておらず、接続・応答ヘッダの待ちの
  間は効かなかった。応答しないサーバへの呼び出しが永久に残り、HTTP/2 ストリーム・タスク・Python の future がリークした。
- 取消し時に要求ストリームを正常に終えていたため END_STREAM（正常な半クローズ）が送られ、stream_unary・stream_stream の
  途中までの入力をサーバが完全な要求として処理し得た。
- `timeout` に巨大な値・負・NaN を渡すと `Duration::from_secs_f64` が panic した。

## 修正

- 呼び出し全体（接続・ヘッダの待ちを含む）を取消しと `select` し、取消しでは上流のストリームを drop して RST_STREAM(CANCEL)
  を送る。要求ストリームは END_STREAM を送らずに止め（`resp_gone`）、状態は CANCELLED（1）にする。
- `timeout` は `Duration::try_from_secs_f64` で検査し、不正なら `ValueError`。

## 試験

- `crates/veil-python/tests/test_native.py` の `TestGrpcClientCancel`（応答しないサーバへの呼び出しをヘッダの前に取り消す・
  不正な timeout）。

## 実施内容

- `crates/veil-python/src/grpc_client.rs`。
