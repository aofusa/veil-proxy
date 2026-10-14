# veil-api（Python バインディング、F-194）

veil の API サーバライブラリを Python から使うための PyO3 バインディングです。
HTTP の受理・TLS・HTTP/1.1・HTTP/2・HTTP/3・OpenAPI 配信は Rust 側（io_uring ベースの veil データプレーン）が担当し、
Python は FastAPI に似たデコレータでハンドラ関数だけを書きます。import 名は `veil` です。

## インストール・ビルド

```bash
python3 -m venv .venv && . .venv/bin/activate
pip install maturin
cd crates/veil-python
maturin develop --release        # 開発用: venv にインストール
maturin build --release          # wheel を作る（target/wheels/）
pip install pydantic             # 任意（モデルによる本文検証・スキーマ生成）
```

abi3（Python 3.9 以上、1 つの wheel で全バージョン）です。

## 使い方

```python
import veil

@veil.get("/ping", offload=False)
def ping():
    return "pong"

@veil.get("/hello/{name}")
def hello(name: str, greeting: str = "Hello"):       # name=パス、greeting=クエリ（注釈で型変換）
    return {"message": f"{greeting}, {name}"}

@veil.post("/users", status_code=201,
           body_constraints={"name": {"type": "string", "minLength": 1, "maxLength": 50, "required": True},
                             "age": {"type": "integer", "minimum": 0, "maximum": 120, "required": True}})
def create_user(body: dict):
    return body

veil.run(listen_cleartext="127.0.0.1:8000")      # Ctrl-C / SIGTERM で graceful に停止
```

FastAPI のように `app = veil.App(title=..., version=...)` を作って `@app.get(...)`、`app.run()` も使えます。
サンプルは `examples/api/python-hello/`・`examples/api/python-fastapi-like/` にあります。

### 引数の束縛（`inspect.signature`）

| 引数 | 束縛 |
|------|------|
| 名前が `req` / `request`、または `veil.Request` 注釈 | リクエスト（`method`・`path`・`query`・`headers`・`body`・`json()`・`text()`・`params`・`query_params`・`client_ip`） |
| パスの `{name}` と同名 | パスパラメータ（`int`/`float`/`bool`/`str` 注釈で変換。失敗は 422） |
| pydantic `BaseModel` 注釈、名前 `body`、`dict`/`list` 注釈 | JSON 本文（pydantic があれば `model_validate_json`。失敗は FastAPI 形の 422） |
| それ以外 | クエリパラメータ（既定値あり=任意、無し=必須。欠落・変換失敗は 422） |

### 戻り値

`dict`/`list`/数値 → JSON、`str` → `text/plain`、`bytes` → `application/octet-stream`、`veil.Response` / `JSONResponse`、
`(body, status)`、pydantic モデル → JSON、`None` → 204。`status_code=` は既定の 200 を置き換えます。
`raise veil.HTTPException(404, "not found")` → `{"detail": "not found"}`。ハンドラの未処理例外は
トレースバックを標準エラーに出して 500 `{"detail":"Internal Server Error"}` を返します。

### `body_constraints`（pydantic 無しの検証）

`minimum` / `maximum` / `exclusiveMinimum` / `exclusiveMaximum` / `minLength` / `maxLength` / `pattern`（`re.fullmatch`）/
`enum` / `type` を辞書本文のフィールドごとに検証します（違反は 422）。必須は `{"required": ["age"]}`（トップレベル）または
フィールド内の `"required": True`。同じ内容が OpenAPI の `requestBody` に反映されます。

### OpenAPI

各ルートの Operation Object（operationId=関数名、docstring の要約・説明、tags、パス/クエリのパラメータ、requestBody、
422 応答）を Python 側で作って veil に渡します。`/openapi.json` と `/docs`（Swagger UI）に載ります。
pydantic モデルのスキーマは `components.schemas` に入ります。`run(openapi=False)` で無効化できます。

## offload（GIL とスレッド）

Python のハンドラは GIL を握って実行されます。

- **既定（`offload=True`）**: veil のハンドラ用プール（`[api] handler_threads`）へ退避して実行します。
  イベントループ（ワーカースレッド）は GIL 待ちで止まらず、I/O を捌き続けます。
- **`offload=False`**: ワーカースレッド上で直接呼びます。自明に軽い処理（定数応答など）専用です。
  重い処理・ブロッキング I/O・長い GIL 保持を置くと、そのワーカーが担当する全接続が止まります。

`veil.run()` は GIL を解放して起動します。async 関数はハンドラ用プールのスレッドごとに 1 つのイベントループで実行されます（F-212）。
Python は純粋な Python ハンドラの並列度を GIL に制限されるため、CPU バウンドな処理は別プロセス構成を検討してください。

## ストリーミング（F-210）

```python
@veil.get("/stream")
def stream():                      # ジェネレータ（非同期ジェネレータも可）は断片ごとに text/plain で送る
    for i in range(3):
        yield f"line {i}\n"

@veil.get("/sse")
def sse():                         # Server-Sent Events。要素は str・JSON にできる値・{"event", "data", "id", "retry"}
    return veil.EventSourceResponse({"event": "tick", "data": {"n": i}} for i in range(3))

@veil.get("/file")
def file():
    return veil.StreamingResponse(open("big.bin", "rb"), media_type="application/octet-stream",
                                  trailers=[("x-checksum", "...")])

@veil.post("/upload", stream_body=True)   # 本文が届く前に呼ぶ
def upload(req: veil.Request):
    return str(sum(len(c) for c in req.stream()))   # 届いた順に読む
```

ストリーミングの応答はハンドラ用プールのスレッドで反復します（`offload=False` のルートでは全量を作ってから送ります）。
クライアントが切断すると反復を止めます。

## gRPC（F-210）

veil はメッセージの直列化に依存しません（protobuf・betterproto・JSON 等を利用者が選びます）。2 つの層があります。

**基本の層（依存なし）** — grpcio と同じ引数名:

```python
import veil, veil.grpc
from demo_pb2 import HelloRequest, HelloReply

@veil.grpc.unary_unary("/demo.v1.Demo/SayHello",
                       request_deserializer=HelloRequest.FromString,
                       response_serializer=HelloReply.SerializeToString)
def say_hello(request, context):
    return HelloReply(message="Hello " + request.name)

veil.grpc.add_health()                            # grpc.health.v1.Health
veil.run(listen_cleartext="127.0.0.1:50051")
```

`unary_stream`・`stream_unary`・`stream_stream` も同じ形で、ストリーミングの入力は反復子、出力は反復子・ジェネレータです。
直列化関数を省くと `bytes` のまま受け渡します。`async def` と非同期ジェネレータも使えます。

**grpcio の層（`pip install veil-api[grpc]`）** — `grpc_tools.protoc` の生成コードをそのまま使う:

```python
server = veil.grpc.Server()
demo_pb2_grpc.add_DemoServicer_to_server(MyServicer(), server)
server.add_insecure_port("127.0.0.1:50051")
server.start(); server.wait_for_termination()     # = veil.run(listen_cleartext=...)
```

`context` は grpcio の `ServicerContext` の主要部分（`abort`・`abort_with_status`・`set_code`・`set_details`・
`invocation_metadata`・`send_initial_metadata`・`set_trailing_metadata`・`time_remaining`・`is_active`・`peer`）を持ち、
状態コードは `grpc.StatusCode` も `veil.grpc.StatusCode` も受けます。TLS（HTTP/2・HTTP/3）は
`app.run(listen=..., tls_cert=..., tls_key=...)` で使います（`add_secure_port` はありません）。
圧縮・期限・gRPC-Web・`[api] grpc_*` の設定は Rust のハンドラと同じです。
サンプル: `examples/api/python-grpc/`（`VEIL_GRPC_MODE=grpcio|plain`）。

## L4（TCP・UDP、F-210）

```python
@veil.l4_tcp("echo", "127.0.0.1:7000")
class Echo(veil.Protocol):                 # asyncio.Protocol と同じ名前
    def data_received(self, data):
        self.transport.write(data)         # コールバックの中でだけ

@veil.l4_udp("udp_echo", "127.0.0.1:7001")
def udp_echo(data, peer):                  # peer = (host, port)。bytes を返すと応答、None は無応答
    return data
```

コールバックはハンドラ用プールで、接続ごとに 1 つずつ順に呼ばれます。

## GraphQL

veil は GraphQL の実装を持ちません。好きなライブラリを普通のルートとして載せます。
`examples/api/python-graphql/` は strawberry（クエリ・ミューテーション・SSE のサブスクリプション）の例です。

## 設定

`veil.run(config_path=None, *, listen_cleartext=None, listen=None, tls_cert=None, tls_key=None, threads=None, overrides=None, openapi=True)`。
`overrides` は `{"server.threads": 4, "api.max_body_size": 1048576}` のようにキー → Python 値（TOML に変換）です。
環境変数 `VEIL_OVERRIDE_<n>` も使えます。キーは `examples/config.toml` を参照してください。
`veil.Config`（`from_file`・`set`・`listen_cleartext`・`listen`・`tls`・`threads`・`to_toml`）を直接組み立てることもできます。

## テスト

```bash
python -I tests/test_binding.py                  # 純 Python の単体テスト（ネイティブ拡張不要）
PYTHON=$(which python) tests/e2e.sh              # maturin develop 後。サンプルを起動して curl（HTTP/1.1 と h2c）で検証
```
