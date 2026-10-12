# F-197: API ライブラリでの gRPC 対応（prost + veil の gRPC 実装、tokio 非依存）

**状態: 設計済み・未着手（design/f197-f200）**

## 要望

- API ライブラリとして使うとき gRPC サーバを書けるようにする。
- メッセージは **prost** を採用する。tonic は使わない。
- **tokio に依存しない**。veil 既存の gRPC 実装（`src/grpc/`: 5 バイトのフレーミング・`grpc-status`/`grpc-message`・
  `grpc-timeout`・gzip/deflate 圧縮・gRPC-Web）と HTTP/2・HTTP/3 の送信経路をそのまま使う。

## 現状（調査結果）

| 項目 | 現状 |
|------|------|
| gRPC のワイヤ処理 | `src/grpc/`（framing・headers・status・compression・stream 種別・web）。プロキシの gRPC 中継で使用中 |
| HTTP/2 の応答 | `H2RespMsg::{Head, Body, Trailers, Reset}` でストリーミング本文とトレーラを送れる（`Trailers` は `grpc` feature） |
| HTTP/3 の応答 | `http3_stream::RespMsg::{Head, Body, Error, Trailers}`。トレーラ送出と同時に fin |
| HTTP/2 の要求本文 | プロキシ経路だけストリーミング（`h2_route_streaming_plan` → `req_tx` チャネル）。ハンドラ経路は END_STREAM まで溜めた `Bytes` |
| ハンドラ API（F-188） | `Request.body: Bytes`（全量）、`Response { status, headers, body: Bytes }`。**トレーラもストリーミングも無い** |
| prost | 本体の optional 依存（`grpc` feature）として宣言済みだが、`src/` からは未使用 |

→ 不足しているのは「ハンドラ経路でのストリーミング要求・応答とトレーラ」と「gRPC のサービス定義・コード生成」。

## 設計

### 1. ハンドラ経路の拡張（`handler` 内、gRPC 以外にも使える一般機能）

- `Response` に `body: ResponseBody` を導入する:
  - `ResponseBody::Full(Bytes)`（現行と同じ）
  - `ResponseBody::Stream(api::stream::BodySender の受信端)`: 本文断片を順に送る（Server-Sent Events・大きな応答にも使える）
  - 応答トレーラ `trailers: Option<Vec<(Bytes, Bytes)>>`（ストリームの最後に送る）
- `Request` に `body_stream()` を足す（要求本文を断片で受け取る）。全量の `body()` は従来どおり（ストリームを
  溜めて返す）。ストリーミング受信はルート単位で有効にする（`RouteDef.streaming_request = true`。gRPC のルートは常に有効）。
- 送信側の対応:
  - HTTP/2: `H2RespMsg::Body` を断片ごと、`Trailers` で終了。要求側は `h2_route_streaming_plan` を
    `Backend::Handler` でストリーミング指定のルートにも適格にし、`req_tx` チャネルを `RequestBody::Stream` として渡す。
  - HTTP/3: `RespMsg::Body`/`Trailers` を同様に使う（バッファ経路は B-97 の作法どおり `FuturesUnordered` 上）。
  - HTTP/1.1: 応答は chunked（トレーラは chunked のトレーラ）。gRPC は HTTP/1.1 では受けない（gRPC-Web を除く）。
- ホットパス: 変更はハンドラ経路の内側だけ。`Backend::Handler` 以外の経路は 1 行も変わらない。

### 2. gRPC のサーバ API（feature `grpc-handler` = `handler` + `grpc` + `dep:prost`、`api` に含める）

```rust
use veil::grpc::{Request, Response, Status, Streaming};

// 手書き登録（コード生成なし）
App::new()
    .grpc_unary("/helloworld.Greeter/SayHello", |req: Request<HelloRequest>| async move {
        Ok(Response::new(HelloReply { message: format!("Hello {}", req.get_ref().name) }))
    })
    .grpc_server_streaming("/pkg.Svc/Watch", |req| async move { Ok(stream_of_messages) })
    .grpc_client_streaming("/pkg.Svc/Upload", |req: Request<Streaming<Chunk>>| async move { ... })
    .grpc_bidi("/pkg.Svc/Chat", |req: Request<Streaming<Msg>>| async move { ... })
```

- `Request<T>`（メタデータ = 要求ヘッダ、`get_ref`/`into_inner`、デッドライン）、`Response<T>`（メタデータ・トレーラ）、
  `Status`（`src/grpc/status.rs` の `GrpcStatusCode` を使う）、`Streaming<T>`（`futures::Stream<Item = Result<T, Status>>`。
  受信本文を `GrpcFrameDecoder` で切り出して `T::decode`）。応答ストリームは `futures::Stream<Item = Result<T, Status>>`。
- 実体は F-188 のハンドラ（`type = "Handler"` の `[[route]]`、パスは `/pkg.Service/Method`、`method = ["POST"]`）。
  `content-type: application/grpc*` 以外は 415、HTTP/1.1 で来たら `grpc-status = 13`（gRPC-Web は `grpc-web` feature で変換）。
- `grpc-timeout` を veil のタイマー（`runtime::time::timeout`）で強制し、超過は `DEADLINE_EXCEEDED`。クライアントの
  RST_STREAM / 切断でハンドラの Future を drop してキャンセルを伝える。
- 圧縮: `grpc-encoding`/`grpc-accept-encoding` を `src/grpc/compression.rs` で処理（既定 identity）。
- メッセージ上限 `[api] grpc_max_message_size`（既定 4MiB、gRPC の慣習値）。
- 組み込みサービス（任意）: `grpc.health.v1.Health`（`App::grpc_health()`）、Server Reflection は後続。

### 3. コード生成（`crates/veil-build`、build-dependency）

- prost-build の `ServiceGenerator` を実装し、`.proto` から「サービスごとの async トレイト + `App` への登録関数」を生成する:

```rust
// build.rs
veil_build::compile_protos(&["proto/helloworld.proto"], &["proto"])?;
// 生成物
#[veil::async_trait_local]  // Send を要求しない（veil のワーカー上で動く）
pub trait Greeter: 'static { async fn say_hello(&self, req: Request<HelloRequest>) -> Result<Response<HelloReply>, Status>; }
pub fn add_greeter<S: Greeter>(app: App, svc: S) -> App { ... grpc_unary(...) ... }
```

- 生成コードは tokio・tonic に依存しない（`veil::grpc` と prost のみ）。
- OpenAPI には載せない（gRPC は対象外）。代わりに `.proto` から一覧をログに出す。

### 4. 互換性の検証

- tonic のクライアント（dev-dependency、テスト専用で tokio を使ってよい）から veil の gRPC サーバへ unary・server/client
  streaming・bidi・デッドライン・キャンセル・圧縮・大きなメッセージ・エラーステータスを試験する。
- grpcurl（Docker）と gRPC の interop テストのサブセット（`empty_unary`・`large_unary`・`client_streaming`・
  `server_streaming`・`ping_pong`・`timeout_on_sleeping_server`・`status_code_and_message`）。
- HTTP/2・HTTP/3（grpc-full）の両方、io_uring・epoll の両方。

### 5. C / Python

- 初版は Rust のみ。C・Python は後続（バイト列のメッセージと関数ポインタ・デコレータで unary だけ）。

## 受け入れ条件

- `examples/api/rust-grpc`（unary + 4 種のストリーミング、ヘルスチェック）が tonic クライアントと grpcurl で動く。
- `full` のバイナリにライブラリのコードが入らない（`tests/api/library_e2e.sh nonregression`）、tokio が本体依存に入らない。
- ガイド（`docs/guide/library.md` の gRPC 節、英日）。

## リスク

- ストリーミング応答のバックプレッシャ: H2 の `resp_tx` チャネル（容量 4）でハンドラ側の送信 Future が待つ形にする。
- `async fn` を含むトレイトの dyn 化: 生成コードは静的ディスパッチ（ジェネリクス）にして Box を 1 リクエスト 1 個に抑える。
