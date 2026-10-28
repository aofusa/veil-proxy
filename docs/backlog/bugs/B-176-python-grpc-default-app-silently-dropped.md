# B-176: Python の `veil.grpc.Server()` を既定の App に登録したまま別の App を run すると、gRPC が黙って配信されない

**状態: 修正済み（v0.10.0）**

## 出典

F-269（HttpArena の veil-python の提出物）の validate で、gRPC の 4 件が `grpc-status=12`（`service ... is not implemented`）
になった（2026-10-26）。

## 事象

- `veil.grpc.Server(app=None)`・`veil.grpc.add_health(app=None)`・`@veil.grpc.unary_unary(..., app=None)` は、`app` を渡さないと
  モジュールの既定の App（`veil._default_app`）に登録する。
- 利用者が `app = veil.App(...)` を作って `app.run()` すると、既定の App の gRPC の登録は使われず、何の警告も無く
  UNIMPLEMENTED を返す。grpcio の生成コードの書き方（`server = grpc.server(...)` に相当する `veil.grpc.Server()`）を
  そのまま写すと起きやすい。

## 修正

- `App.run()` の登録の前（`_warn_grpc_on_default_app`）で、実行する App が既定の App でなく、既定の App に gRPC の
  メソッドかヘルスの登録があれば `RuntimeWarning` を出す（英語の文、`veil.grpc.Server(app)` を案内する）。
  起動時に 1 回だけで、要求の経路には何も足さない。
- 文書（`docs/guide/library.md`（英日）、`crates/veil-python/docs/grpc.md`）に、自分の App では `veil.grpc.Server(app)`・
  `veil.grpc.add_health(app)` と渡すこと・渡さないと警告することを書いた。

## 試験

- `crates/veil-python/tests/test_binding.py` の `TestGrpcDefaultAppWarning`（別の App で警告・ヘルスだけでも警告・
  App に登録したら警告しない・既定の App を run したら警告しない。文が ASCII であること）。`python3 -I tests/test_binding.py` 45 件 OK。
- HttpArena の veil-python の提出物は `veil.grpc.Server(app)` に直して validate 112 passed / 0 failed。

## 実施内容

- `crates/veil-python/python/veil/__init__.py` に `App._warn_grpc_on_default_app` を足し、`run()` から呼ぶ。
- 単体テスト 4 件と文書（英日）。
