# F-210: C API・Python を Rust の API 並みにする（ストリーミング・L4・gRPC・GraphQL）

**状態: 完了（feat/api-roadmap-impl、2026-10-14）**

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
- 非同期（Python）: プールのスレッドごとに 1 個のイベントループを作って使い回す（`asyncio.run` を毎回作らない。F-212 の手順 5 で入れる）。
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
- Python: **2 層**にする（2026-10-14 決定）。どちらも同じ Rust の土台（`BytesCodec`）の上に載り、上の層は下の層だけで書く。
  - **基本の層（依存無し）**: `veil.grpc.unary_unary(path, handler, request_deserializer=None, response_serializer=None)`・
    `unary_stream`・`stream_unary`・`stream_stream`（デコレータ形 `@veil.grpc.unary_unary("/pkg.Svc/M", ...)` も）。
    直列化は「`bytes` → オブジェクト」「オブジェクト → `bytes`」の任意の関数で、省略すると `bytes` のまま渡す。
    grpcio と同じ引数名なので、どの protobuf ライブラリでも同じ書き方になる:
    `protobuf` は `Msg.FromString` / `Msg.SerializeToString`、betterproto は `Msg().parse` / `bytes`、JSON なら `json.loads` / `json.dumps`。
    `context` は下の `ServicerContext` 互換と同じもの（grpcio が無くても動く。状態コードは `veil.grpc.StatusCode`）。
  - **grpcio の層（任意依存）**: `pip install veil[grpc]`（`pyproject.toml` の optional-dependencies に `grpc = ["grpcio"]`。
    pydantic と同じ扱い）。`veil.grpc.Server` が grpcio の `add_generic_rpc_handlers` と `add_registered_method_handlers`
    （新しい grpcio-tools の生成コードが使う）を受け、各 `RpcMethodHandler` の 4 種類と直列化関数を基本の層に登録する。
    `grpc_tools.protoc` が生成した `add_<Service>Servicer_to_server(servicer, server)` と servicer のクラスが**そのまま動く**。
    `context` は grpcio の `ServicerContext` の主要部分（`abort`・`abort_with_status`・`set_code`・`set_details`・
    `invocation_metadata`・`send_initial_metadata`・`set_trailing_metadata`・`time_remaining`・`is_active`・`peer`）を持ち、
    状態コードは `grpc.StatusCode` をそのまま受ける。`grpc.aio` 形式（`async def` のメソッド・非同期ジェネレータ）は
    F-212 のイベントループで動かす。
  - `grpc` という名前の偽モジュールは提供しない（本物の grpcio と同じ環境に入ると衝突し、どちらが読まれるかで挙動が変わるため）。
    grpcio の生成コード（`*_pb2_grpc.py`）を使う人は `veil[grpc]` を入れる。生成コードを使わない人は grpcio 不要。
  - grpcio-tools（`.proto` からの生成）は利用者の開発時の道具で、veil の依存にしない。`*_pb2.py` が使う `protobuf` も利用者の依存。
  - 対応しないもの（記録）: betterproto・grpclib のサービス基底クラス（`ServiceBase`）をそのまま載せる層。betterproto の
    メッセージは基本の層で使える。要望があれば 3 つ目の層として足せる（基本の層の上に 100 行程度）。
  - 試験: `examples/api/python-grpc` を grpcio-tools の生成コード + `veil.grpc.Server` で書き、`rust-grpc` と同じ tonic
    クライアントの試験を流す。基本の層は grpcio を入れない venv で、`protobuf` の `FromString` 版と生の `bytes` 版を試す。

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
- C・Python の直列化は利用者が選ぶ（veil-capi・veil-python の必須依存に protobuf・GraphQL のライブラリが無い。grpcio は
  Python の任意依存 `veil[grpc]` のみで、grpcio 無しでも基本の層で gRPC を提供できる）。
- `crates/veil-capi/README.md`・`include/veil.h`・`crates/veil-python/README.md`・`docs/guide/library.md`（英日）を更新する。

## 関連

F-193・F-194（今の C・Python）、F-207（GraphQL）、F-208（`BytesCodec`）、F-209（gRPC の未対応項目は C・Python にも同じく効く）。

## 実装（2026-10-14）

設計からの変更点と実装の要点。

- **土台**は `api::bridge`（設計の `api::pipe`）: `to_worker`（プール → ワーカー。押し込むと `WakeFlag` と通知 fd で
  ワーカーを起こす。スレッド越しに Waker を呼ばない）と `to_pool`（ワーカー → プール。Condvar で待つ）の有界キュー、
  `call_blocking`（ハンドラ用プールでコールバックを呼び、応答の頭・断片・トレーラをキューで返す）、
  `runtime::offload::spawn_handler`。
- **gRPC** は `api::grpc::blocking`（`App::grpc_blocking`）: 4 種類ともワイヤ上は同じなので双方向として `BytesCodec` で
  登録し、単項等の 1 通の制限は C・Python の層で行う。何も送らずにエラーで終えたら Trailers-Only。
- **L4** は `api::l4` の blocking 版（`L4Out`・`L4Session`、`App::l4_tcp_blocking`・`l4_udp_blocking`）。
- **C**（`crates/veil-capi/src/ext.rs`、`veil.h`）: 関数名は `veil_stream_set_trailer`・`veil_grpc_set_status`・
  `veil_l4_reply` 等（README の表）。エラーコード `VEIL_ERR_CLOSED`・`VEIL_ERR_STATE` を追加。サンプルは
  `examples/api/c-features`（protobuf-c ではなく手書きの最小の protobuf の符号化で、依存を増やさない）。
- **Python**（`crates/veil-python/src/ext.rs`・`native.rs`、`python/veil/grpc.py`）: `StreamingResponse`・
  `EventSourceResponse`・ジェネレータの戻り値・`stream_body=True` と `request.stream()`・`veil.grpc`（基本の層と
  `Server`）・`veil.Protocol` と `l4_tcp`・`l4_udp`。`request.astream()` は作らなかった（`async def` の中でも
  `request.stream()` が使える。プールのスレッドごとのイベントループなので、断片を待つ間ブロックしても他の要求を止めない）。
  `pyproject.toml` の任意依存 `grpc = ["grpcio"]`。
- **GraphQL**: `examples/api/python-graphql`（strawberry）。

## 結果

- `tests/api/library_e2e.sh c` / `python`: C・Python とも Rust の gRPC のサンプルと同じ tonic の相互運用試験
  （h2 と h3、26 項目）・SSE・本文のストリーミング・L4 の TCP/UDP。Python は grpcio の生成コードと基本の層の両方。
- 試験中に HTTP/2 の接続ウィンドウの補充漏れ（B-130）を発見・修正した。
