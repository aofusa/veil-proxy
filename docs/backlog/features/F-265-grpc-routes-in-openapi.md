# F-265: gRPC のルートを OpenAPI（`/openapi.json`・`/docs`）に載せる

**状態: 完了（v0.10.0。2026-10-25 起票・設計、2026-10-26 実装。親は [F-258](F-258-v0100-plan.md)）**

## 背景

`App::grpc_*` で登録したメソッドと、サービスごとの `UNIMPLEMENTED` の受け皿は `OperationMeta { hidden: true }` で登録され
（`src/api/grpc/mod.rs` `grpc_route`）、OpenAPI に出ない。同じアプリの REST と gRPC を 1 か所で一覧できない。
メッセージの型の情報は、`veil-build` が生成する `FILE_DESCRIPTOR_SET`（リフレクション用、F-209 #3）にある。

## 方針

OpenAPI は REST の記述なので、gRPC を「呼べる操作」として正確には書けない。**一覧と型の文書** を目的にし、次の形で載せる
（grpc-gateway・Envoy の gRPC-JSON transcoder の OpenAPI 出力と同じく、HTTP の `POST /<package>.<Service>/<Method>`）。
Swagger UI の「Try it out」で gRPC の本体（バイナリ・トレーラ）は呼べないので、gRPC の操作では無効にする（下の 3.）。
gRPC-JSON の変換（transcoding）は範囲外。

## 設計

### 1. 文書の形

```json
"/demo.v1.Demo/SayHello": {
  "post": {
    "operationId": "demo_v1_Demo_SayHello",
    "tags": ["gRPC: demo.v1.Demo"],
    "summary": "SayHello (unary)",
    "description": "<.proto のメソッドのコメント>",
    "requestBody": { "content": {
      "application/grpc": { "schema": { "$ref": "#/components/schemas/demo.v1.HelloRequest" } },
      "application/grpc-web": { "schema": { "$ref": "..." } } } },
    "responses": { "200": { "description": "gRPC response (status in the grpc-status trailer)",
      "content": { "application/grpc": { "schema": { "$ref": "#/components/schemas/demo.v1.HelloReply" } } } } },
    "x-grpc": { "service": "demo.v1.Demo", "method": "SayHello", "kind": "unary",
                "requestType": "demo.v1.HelloRequest", "responseType": "demo.v1.HelloReply" }
  }
}
```

- `kind` は `unary` / `server_streaming` / `client_streaming` / `bidi_streaming`。ストリーミングは `summary` に明記し、
  スキーマは 1 メッセージの型（`x-grpc.streaming: true`）。
- メッセージのスキーマは **proto3 の JSON マッピング** で JSON Schema にする（`int64` は文字列、`bytes` は base64 の文字列、
  `enum` は名前の列挙、`oneof` は `oneOf`、`map` は `additionalProperties`、`repeated` は配列、well-known types
  （`Timestamp` → `date-time` の文字列、`Duration`・`Struct`・`Value`・`Any`・`wrappers`）は対応表で変換）。
  `components.schemas` の名前は完全修飾名（`demo.v1.HelloRequest`）。再帰する型は `$ref` で表す。
- 記述子が無いとき（`BytesCodec` で登録・C/Python で記述子を渡していない）は、`x-grpc` とメソッド名だけ載せ、本文のスキーマは
  `{"type": "string", "format": "binary"}`。
- サービスごとの `UNIMPLEMENTED` の受け皿・`grpc.health.v1`・`grpc.reflection.*` は載せない（`[api] openapi_grpc_builtin = true` で載せられる）。

### 2. 記述子の出どころ

- Rust: `App::grpc_reflection(&[FILE_DESCRIPTOR_SET])` を呼んだアプリはそれを使う。リフレクションを公開したくない人のために
  `App::grpc_descriptors(&[..])`（OpenAPI だけに使う。リフレクションのサービスは登録しない）を足す。`veil-build` の生成する
  `add_<service>` は、メソッドのコメント（`SourceCodeInfo`）を `OperationMeta.description` に入れる。
- 記述子の解析は起動・リロード時に 1 回（`refresh`）。今の `wire.rs` の読み手で `FileDescriptorProto` の必要なフィールド
  （`message_type`・`field`・`enum_type`・`service`・`method`・`source_code_info`・`options.map_entry`・`proto3_optional`）だけを読む
  （prost に依存しない。F-208 の方針）。
- Python: `veil.grpc.add_descriptor(demo_pb2.DESCRIPTOR)`（`FileDescriptor.serialized_pb` を渡す）か、`veil.grpc.Server`
  （grpcio の互換層）に登録した servicer の `_pb2` から自動で取る。C: `veil_grpc_descriptors(app, ptr, len)`。

### 3. 表示

- `[api] openapi_grpc`（新設、既定 `true`。依頼どおり既定で載せる）。`false` で今と同じ。
- Swagger UI（F-264 の同梱版）で gRPC の操作の「Try it out」を消す: 初期化スクリプトの `plugins` に、`x-grpc` を持つ操作の
  `tryItOutEnabled` を false にする小さなプラグインを入れる（同梱する JS は 1 個のまま。HTML の初期化スクリプトの中）。
  同じプラグインで WebSocket の操作（`x-veil-websocket`、F-262・F-263）も対象にする。
  タグ「gRPC: <service>」の説明に、grpcurl の例（`grpcurl -plaintext host:port list`）を入れる（リフレクションが有効なとき）。

## 試験

- 単体: `examples/api/rust-grpc` の `demo.proto` と、型の網羅用の `.proto`（スカラー全種・enum・oneof・map・入れ子・再帰・
  well-known types・`optional`）から作った文書を期待値と比べる。OpenAPI 3.1 の検証器（`openapi-spec-validator`、試験用だけ）で妥当。
- E2E: `library_e2e.sh grpc` で `/openapi.json` に 4 種類のメソッドが載り、`openapi_grpc = false` で消える。Python（`veil.grpc.Server`）も同じ。
- 性能: 生成は起動時だけなので要求の経路は変わらない（計測不要）。

## 文書

- `docs/guide/library.md`（英日）の gRPC 節・OpenAPI 節、`crates/veil-python/docs/grpc.md`（英日）、`examples/config.toml` の `[api]`。

## 受け入れ条件

- Rust・Python・C の gRPC のルートが OpenAPI に型付きで載り、Swagger UI で一覧できる。文書（英日）・`backlog.md` が更新されている。

## 実施内容

- `src/api/grpc/describe.rs`（新設）: 登録したメソッドの種類（`grpc_route` が記録。`App::grpc_kind` で明示）と記述子
  （`App::grpc_reflection`・新設の `App::grpc_descriptors`）を集め、OpenAPI の生成（起動・リロード時）で `paths` の
  `POST /pkg.Svc/Method`・`components.schemas`・トップレベルの `tags` を足す。記述子は `wire.rs` の読み手で必要なフィールドだけ読む
  （prost に依存しない）。スキーマは参照される型だけ載せる。メソッド・サービス・メッセージ・フィールド・enum の説明は
  `SourceCodeInfo` のコメントから取る（`veil-build` は prost-build の記述子に `source_code_info` を残すので、生成コードは変えていない）。
  ルート自体は従来どおり `hidden: true` のまま（POST・OPTIONS・受け皿が重複して載らない）。
- 記述子の無いメソッドは本文を `{"type":"string","format":"binary"}`、記述子の種類（`client_streaming`・`server_streaming`）は
  登録関数から推した種類より優先し、`grpc_kind` の明示は上書きしない。
- `[api] openapi_grpc`（既定 `true`）・`openapi_grpc_builtin`（既定 `false`。health・reflection を載せる）。
- Swagger UI の初期化のプラグインで `x-grpc` の操作の「Try it out」も消す。タグの説明に、リフレクションが有効なら grpcurl の例。
- Python: 素の層は `request_deserializer` の生成された `FromString` から、grpcio の層は `Server._add` から `_pb2` の
  `FileDescriptor`（依存を含めた FileDescriptorSet）を自動で渡す。`veil.grpc.add_descriptor(...)` を公開。
  `register_grpc` にストリーミングの種類を渡す。C: `veil_grpc_descriptors(cfg, data, len)`、`veil_register_grpc` の種類を記録。
- 試験: 単体（proto3 JSON マッピング（スカラー・enum・oneof・map・入れ子・再帰・well-known types・`optional`）、壊れた記述子、
  文書に 4 種類のメソッドと型が載り health が載らないこと）。E2E: `library_e2e.sh grpc` で Rust の 4 種類と
  `openapi_grpc = false` で消えること、`python` で素の層・grpcio の層の両方。
- 文書: `docs/guide/library.md`（英日）の `[api]` 表・「gRPC in OpenAPI」・C API、`crates/veil-python/docs/grpc.md`、
  `crates/veil-capi/README.md`、`examples/config.toml`。
- OpenAPI 3.1 の検証器での検証は、試験用の Python 依存を増やさないため見送った（生成物の形は単体試験で固定）。
