# B-214: `veil._veil` の型スタブ（`_veil.pyi`）が v0.10.0 で足した実装とずれ、CI の stubtest が落ちる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の PR の CI（`ci.yml` の `python docs + stubs (F-239)`、`mypy.stubtest veil._veil`。2026-10-27）。

## 事象

- 永続イベントループ（`AioJob`・`AioStream`・`aio_*`）・WebSocket（`NativeWebSocket`・`NativeWsSender`・`register_websocket`）・
  ASGI・CORS・静的配信の登録（`register_asgi`・`register_cors`・`register_static`）・gRPC のクライアント（`GrpcChannel`・`GrpcCall`・
  `grpc_client_setup`）が、スタブでは定義されているのに `__all__` に無かった。
- `AioJob.start_stream` の引数名（スタブ `response`、実物 `sr`）、`AioStream.closed` の種類（スタブはメソッド、実物は getter）が違った。
- `GrpcChannel`・`GrpcCall` は継承できないのに `@final` が無かった。

型検査器が `w.closed` を「関数の真偽」として扱い常に真と判定する等、利用者の型検査を誤らせる。

## 修正

スタブを実物に合わせる（`__all__` に追加、`start_stream(self, sr)`、`closed` を `@property`、`@final` を付ける）。
Windows でホイールをビルドして `mypy.stubtest veil._veil` が通ることを確かめた。
