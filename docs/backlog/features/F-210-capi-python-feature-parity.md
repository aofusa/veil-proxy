# F-210: C API・Python を Rust の API 並みにする（ストリーミング・L4・gRPC・GraphQL）

**状態: 未着手（設計済み 2026-10-14。段階ごとにコミット、必要なら着手時に段階ごとのチケットに切る）**

## 背景

C API（`crates/veil-capi`、F-193）と Python（`crates/veil-python`、F-194）は、本文全体を受け取って本文全体を返す
HTTP のハンドラだけを持つ。

| 機能 | Rust | C | Python |
|---|---|---|---|
| HTTP ハンドラ（本文全体） | ○ | ○（`veil_register_route[_ex]`） | ○（デコレータ・FastAPI 風の引数束縛・OpenAPI） |
| 応答のストリーミング（SSE・大きな本文・トレーラ） | ○ `Response::stream` | × | × |
| 要求本文のストリーミング | ○ `take_body_stream` | × | × |
| L4（TCP・UDP）ハンドラ | ○ `l4_tcp`・`l4_udp` | × | × |
| gRPC（4 種類） | ○ | × | × |
| GraphQL | ○（F-207 で利用者が選ぶ形に変える） | × | × |
| 非同期ハンドラ | ○ | – | △ `async def` は要求ごとに `asyncio.run`（イベントループを毎回作る） |
| ハンドラの実行場所 | ワーカー | ハンドラ用プール（既定）か `VEIL_ROUTE_INLINE` | プール（既定、GIL）か `offload=False` |

## 方針

- **veil は転送だけを受け持つ**: HTTP の枠組み・gRPC の 5 バイトの見出し・圧縮・期限・トレーラ・L4 の接続管理。
  メッセージの直列化と復元（protobuf・JSON・GraphQL の実行）は各言語の利用者が好きなライブラリで行う
  （C: protobuf-c・nanopb・upb、Python: `protobuf`・betterproto・strawberry・graphene・ariadne 等）。veil の C API・Python は
  それらに依存しない。
- C と Python は同じ Rust の土台（下の「パイプ」）の上に作る。Rust 側の gRPC は F-208 の `BytesCodec` で受ける。
- ハンドラ用プールのスレッドは長く占有しない設計にする（L4 の長い接続でプールを使い切らない）。

## 設計

### 1. 土台: ワーカー ⇄ プールのパイプ（`src/api/`、`handler-pool`）

- `api::pipe::{Sender, Receiver}`: 有界の SPSC キュー。
  - プール側（C・Python のコールバックのスレッド）→ ワーカー: 応答の断片。押し込んだら `runtime::offload` の通知 fd で
    ワーカーを起こす（AGENTS.md「スレッド越しに Waker を呼ばない」）。満杯ならプール側はブロック（背圧）。
  - ワーカー → プール側: 要求本文の断片・gRPC の受信メッセージ。プール側は条件変数で待つ（ブロックしてよいスレッド）。
- ワーカー側は `Stream` / `Sink` として見え、既存の `Response::stream`・`RequestBodyStream` にそのまま繋がる。
- 終了・キャンセル: クライアントの切断でワーカー側が drop されたら、プール側の読み書きはエラー（C は `VEIL_ECANCELED`、
  Python は `veil.Cancelled` 例外）になる。

### 2. HTTP のストリーミング

- C:
  - `veil_stream_t *veil_response_stream(veil_response_t *resp, int status)` で応答をストリーミングに切り替え、
    `veil_stream_write(s, ptr, len)`（複製して送る。満杯ならブロック）・`veil_stream_trailer(s, name, value)`・
    `veil_stream_close(s)`。`veil_stream_t` はコールバックから返った後も別スレッドで使える（所有は呼び出し側、`close` で解放）。
  - `veil_request_body_read(req, buf, cap)`（ブロック。0 で終わり）と `veil_route_flag` の `VEIL_ROUTE_STREAM_BODY`
    （本文を溜めずに呼ぶ）。
  - `VEIL_ROUTE_INLINE` のルートではブロックする関数を使えない（`VEIL_EWOULDBLOCK` を返す）。
- Python:
  - 戻り値がジェネレータ / 非同期ジェネレータ、または `veil.StreamingResponse(iterable, media_type=...)` ならストリーミング。
    `veil.EventSourceResponse` で SSE の整形（`data:` 行）も用意する。
  - `request.stream()`（同期イテレータ）・`async for chunk in request.astream()`。
- 非同期（Python）: プールのスレッドごとに 1 個のイベントループを作って使い回す（`asyncio.run` を毎回作らない）。
  `async def` のハンドラはそのループで実行し、非同期ジェネレータもそのまま回す。

### 3. L4（TCP・UDP）

長い接続でプールのスレッドを占有しないよう、**イベントのコールバック**にする（ブロックする `read` は出さない）:

- C: `veil_register_l4_tcp(cfg, name, listen, const veil_l4_callbacks_t *cb, void *ud)`。
  `on_connect(conn)`・`on_data(conn, ptr, len)`・`on_close(conn)`。`veil_l4_write(conn, ptr, len)`（複製して送る、ブロックしない）・
  `veil_l4_close(conn)`・`veil_l4_peer_addr(conn)`。UDP は `veil_register_l4_udp(..., on_datagram)` で応答を返す。
- Python: `@veil.l4_tcp(name, listen)` のクラス（`connection_made` / `data_received` / `connection_lost`、asyncio の
  `Protocol` と同じ名前）と `@veil.l4_udp`。
- 1 接続のコールバックは順番に 1 つずつ（接続ごとのキューをプールに積む。同じ接続のコールバックが並行しない）。
  既定ではプールで、`VEIL_ROUTE_INLINE` 相当でワーカー上でも呼べる。
- Rust 側は `api::l4` の `L4Connection` を使って実装し、本体の L4 経路には触れない。

### 4. gRPC

- Rust 側は F-208 の `BytesCodec` で `App::grpc_*_with` に登録する（4 種類）。期限・圧縮・トレーラは Rust と同じ。
- C: `veil_register_grpc(cfg, "/pkg.Svc/Method", VEIL_GRPC_UNARY | SERVER_STREAMING | CLIENT_STREAMING | BIDI, cb, ud)`。
  - unary: `cb(call, const uint8_t *req, size_t len, veil_grpc_reply_t *reply)` で `veil_grpc_reply_set(reply, ptr, len)` か
    `veil_grpc_reply_status(reply, code, message)`。
  - ストリーミング: `veil_grpc_recv(call, &ptr, &len)`（ブロック、0 で終わり）・`veil_grpc_send(call, ptr, len)`・
    `veil_grpc_finish(call, code, message)`。メタデータ・期限・トレーラの取得・設定関数。
  - サンプル `examples/api/c-grpc` は protobuf-c（または nanopb）で符号化する。
- Python: grpcio の生成コード（`grpc_tools.protoc` の `*_pb2_grpc.py`）を**そのまま使える**ようにする。
  - `veil.grpc.Server` は grpcio の `add_generic_rpc_handlers` / `add_registered_method_handlers` を受け取り、
    `RpcMethodHandler`（`unary_unary` 等の 4 種類と `request_deserializer`・`response_serializer`）を veil に登録する。
    直列化はその生成コードの関数（利用者が選んだ protobuf ライブラリ）が行う。
  - `context` は grpcio の `ServicerContext` の主要部分（`abort`・`set_code`・`set_details`・`invocation_metadata`・
    `set_trailing_metadata`・`time_remaining`・`is_active`）を持つ。`grpc.aio` 形式（`async def`）も 2 のイベントループで動かす。
  - grpcio 本体（C++ のコア）は必須にしない（生成コードが `import grpc` するので、`grpc` が無い環境では
    `veil.grpc` が最小の `grpc` 互換モジュールを提供するか、grpcio を入れてもらうかを着手時に決める。生成コードが使うのは
    `grpc.unary_unary_rpc_method_handler` 等と `StatusCode` だけ）。
  - `@veil.grpc_unary("/pkg.Svc/M", request_deserializer=..., response_serializer=...)` の素の登録も持つ。

### 5. GraphQL

veil に GraphQL の処理は持たない（F-207 と同じ方針）。

- Python: `examples/api/python-graphql`（strawberry の `schema.execute_sync` / `execute`、GraphiQL、2 の SSE でサブスクリプション）。
- C: GraphQL の C ライブラリは一般的でないので、文書で「HTTP のハンドラとして任意の実装を載せる」とだけ書く（サンプルは作らない）。

### 6. OpenAPI

ストリーミングのルートは OpenAPI に載せる（`text/event-stream` 等）。gRPC・L4 は載せない（Rust と同じ）。

## 段階

1. パイプ（単体試験: 背圧・切断・終了）
2. HTTP のストリーミング（C・Python）と Python のイベントループの使い回し
3. gRPC（Rust の `BytesCodec` が F-208 に依存）
4. L4
5. GraphQL のサンプル・文書

## 試験

- `tests/api/library_e2e.sh` の C・Python 節に各機能を足す: SSE を `curl -N` で、要求本文のストリーミングを 1MiB の
  チャンク送信で、gRPC は既存の tonic クライアント（`rust-grpc` と同じ `.proto` を C・Python で実装し同じ試験を流す）、
  L4 は既存の `l4_echo` の確認。
- `crates/veil-python/tests/test_binding.py` に単体、`crates/veil-capi` に C の単体（`cargo test -p veil-capi` から C を呼ぶ）。
- 各 OS（F-203 の手順）で C・Python の節を 1 回。

## 受け入れ条件

- 上の表の C・Python の × が ○ になり、`library_e2e.sh` で確かめられている。
- C・Python の直列化は利用者が選ぶ（veil-capi・veil-python に protobuf・GraphQL のライブラリへの依存が無い）。
- `crates/veil-capi/README.md`・`include/veil.h`・`crates/veil-python/README.md`・`docs/guide/library.md`（英日）を更新する。

## 関連

F-193・F-194（今の C・Python）、F-207（GraphQL）、F-208（`BytesCodec`）、F-209（gRPC の未対応項目は C・Python にも同じく効く）。
