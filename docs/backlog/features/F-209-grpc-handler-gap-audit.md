# F-209: API ライブラリの gRPC の未実装・未対応機能の洗い出しと対応計画

**状態: 完了（feat/api-roadmap-impl、2026-10-14。#10 は F-213 に切り出し、#11・#12 は対象外）**

## 範囲

`src/api/grpc/`（`grpc-handler`、F-197）・`crates/veil-build`・`examples/api/rust-grpc`。プロキシの gRPC 中継
（`src/grpc/`、gRPC-Web 変換を含む）は対象外。C・Python の gRPC は F-210、prost からの切り離しは F-208。

## 実装済み（2026-10-14 時点、コードと `docs/guide/library.md` で確認）

| 機能 | 状況 |
|---|---|
| unary・サーバ・クライアント・双方向ストリーミング | ○（HTTP/2・HTTP/3） |
| `veil-build` によるコード生成（サービス trait + 登録関数） | ○（サーバのみ） |
| `grpc-timeout`（期限） | ○ ハンドラを打ち切って `DEADLINE_EXCEEDED`（ストリームはメッセージの間で確認） |
| キャンセル（RST_STREAM・切断） | ○ ハンドラの Future と応答ストリームを drop |
| 要求の解凍（gzip・deflate） | ○（ただし上限無し → B-127） |
| 受信メッセージの上限（`[api] grpc_max_message_size`） | ○ `RESOURCE_EXHAUSTED` |
| メタデータ・トレーラ・Trailers-Only のエラー応答 | ○ |
| 登録済みサービスの未知のメソッド | ○ `UNIMPLEMENTED` |
| HTTP/1.1 で来た gRPC | ○ `grpc-status: 13` |
| ヘルスチェック（`grpc.health.v1` の Check・Watch） | ○ |
| ミドルウェア（`wrap_on`・`layer_on`） | ○ `route_fn` 経由で gRPC のルートにも掛かる（gRPC 用の型付きインターセプタは無い） |

## 未実装・未対応（優先度順）

| # | 項目 | 現状 | 影響 | 対応案 | 優先度 |
|---|---|---|---|---|---|
| 1 | **解凍後のサイズ上限** | 全部解凍してから上限と比べる | 小さな圧縮メッセージでメモリを使い切れる（DoS） | **B-127** で修正（上限 + 1 バイトで打ち切る） | P1 |
| 2 | 応答の圧縮 | 常に非圧縮（`grpc-accept-encoding` は送る） | 大きな応答・遅い回線で不利。tonic・grpc-go は既定で対応 | 要求の `grpc-accept-encoding` と `[api] grpc_response_compression`（既定 off、`gzip`/`deflate`）・最小サイズで `grpc-encoding` を付けて圧縮。`Response::with_compression` で呼び出しごとに上書き | P2 |
| 3 | サーバリフレクション（`grpc.reflection.v1`・`v1alpha`） | 無い | grpcurl・Postman・grpcui で `.proto` 無しに呼べない | F-208 の後。`veil-build` が `FileDescriptorSet` を `OUT_DIR` に書き、生成コードの `FILE_DESCRIPTOR_SET` を `App::grpc_reflection(&[..])` に渡す。応答の符号化は手書き（prost 無し、`health.rs` と同じ） | P2 |
| 4 | gRPC-Web（ハンドラ） | 無い。`is_grpc_content_type` が `application/grpc-web*` も通すので、HTTP/2 の gRPC-Web 要求は通常の gRPC として処理され、ブラウザが読めないトレーラで返る（`-web-text` は base64 のまま復号して `INTERNAL`） | ブラウザから直接呼べない。誤った応答を返す | まず `-web` を 415 / `UNIMPLEMENTED` で明示的に断る（#1 と同じ修正の中で）。次にプロキシ側の `src/grpc/web.rs`（本文内トレーラ・base64）を使ってハンドラでも gRPC-Web を受ける（HTTP/1.1 も可） | P2 |
| 5 | HTTP/3 の双方向ストリーミング | 要求ストリームを読み切ってからハンドラを呼ぶ（半二重） | HTTP/3 のチャット型の双方向が動かない | HTTP/3 の要求本文をストリーミングでハンドラへ渡す（HTTP/2 の `RequestBodyStream` と同じ形）。B-97 の「バッファ経路はメインループで await しない」制約の中で、要求本文のチャネルを足す | P2 |
| 6 | リッチなエラー詳細（`grpc-status-details-bin`） | 無い | `google.rpc.Status`（`BadRequest` 等）を返せない | `Status::with_details(Bytes)`（符号化済みのバイト列を受ける。prost 不要）。base64 してトレーラへ | P3 |
| 7 | 型付きインターセプタ | `wrap_on` は HTTP のハンドラを包むだけ | 認証などでメタデータを見て `Status` で断る処理を書きにくい | `App::grpc_interceptor(prefix, fn(&Metadata) -> Result<(), Status>)`（`wrap_on` の上に作り、断るときは Trailers-Only） | P3 |
| 8 | 送信メッセージの上限・サービスごとの上限 | 受信の全体の上限のみ | 大きすぎる応答を相手が拒否する | `[api] grpc_max_send_message_size` と、`veil-build` の登録関数に上限を渡す引数 | P3 |
| 9 | 未登録のサービス | ルートが無いので HTTP 404 | gRPC の仕様では 404 → `UNIMPLEMENTED` に読み替えられるので実害は小さい。ログが HTTP の 404 になる | `content-type: application/grpc` の 404 を Trailers-Only の `UNIMPLEMENTED` で返す（`cfg(feature = "grpc-handler")` の分岐） | P3 |
| 10 | HTTP/2 のキープアライブ（PING）の方針 | サーバの HTTP/2 設定に従う。gRPC の `GOAWAY too_many_pings` 相当の制限は無い | クライアントの keepalive が短すぎるとき、grpc-go と違って断らない | `[http2]` の PING 頻度の上限を gRPC の既定（5 分に 2 回まで）に合わせる案を検討。プロキシ側の挙動に関わるので別チケット | P3 |
| 11 | クライアントの生成 | `veil-build` はサーバのみ | veil 上から別の gRPC サービスを呼ぶには tonic（tokio）が要る | `veil::tokio` 上で tonic のクライアントは動く（F-198 の ecosystem）。veil のネイティブクライアントは `http2::upstream_mux` を使う形で将来検討。今回はしない | 対象外（記録のみ） |
| 12 | channelz・ORCA・admin サービス | 無い | 運用の可視化 | メトリクス（Prometheus）で代替。今回はしない | 対象外（記録のみ） |

識別: #1 は B-127。#2〜#5 は本チケットで実装する。#6〜#10 は本チケットの最後にまとめて行うか、着手時に別チケットに切る。

## 試験

各項目で `src/api/grpc/tests.rs` の単体試験と、`tests/api/library_e2e.sh grpc` に項目を足す
（応答の圧縮は tonic の `accept_compressed`、リフレクションは grpcurl の `list`・`describe`（入っていれば）か tonic-reflection の
クライアント、gRPC-Web は `curl` で本文内トレーラを確かめる、HTTP/3 の双方向は quinn + h3 のクライアントで交互に送受信）。

## 受け入れ条件

- #2〜#5 が実装され、`library_e2e.sh grpc` で確かめられている（Linux io_uring・epoll）。
- `docs/guide/library.md`（英日）の gRPC 節の表と Limitations を更新する。

## 実施内容（2026-10-14）

| # | 対応 |
|---|---|
| 1 | B-127 で修正（解凍を上限 + 1 バイトで打ち切る） |
| 2 | `[api] grpc_response_compression`（`none`・`gzip`・`deflate`）・`grpc_response_compression_min_size`（既定 1024）と `Response::with_compression`。要求の `grpc-accept-encoding` が受ける方式だけ使う。応答ヘッダに `grpc-encoding`、メッセージごとに圧縮フラグ |
| 3 | `App::grpc_reflection(&[FILE_DESCRIPTOR_SET])`（`src/api/grpc/reflection.rs`）。`grpc.reflection.v1`・`v1alpha` の `ServerReflectionInfo`（一覧・ファイル名・シンボル（メソッド・入れ子の型を含む）・拡張・拡張番号、見つからなければ `NOT_FOUND` のエラー応答）。`veil-build` が `FileDescriptorSet` を `OUT_DIR` に書き `FILE_DESCRIPTOR_SET` を生成。記述子は `wire` で必要なフィールドだけ読み、ファイルはバイト列のまま返す（prost 不要）。組み込みの health・reflection 自身の型は返さない（一覧には出る） |
| 4 | gRPC-Web をハンドラで直接受ける（`src/api/grpc/web.rs`）: バイナリ・`-text`（断片をまたぐ base64）、HTTP/1.1・HTTP/2・HTTP/3。トレーラは本文の最後の枠（0x80）。エラーは状態をヘッダと本文の枠の両方に載せる。CORS は `[api] grpc_web_cors_origins`（プリフライトは `OPTIONS` をメソッドのルートで受ける）。プロキシの `grpc-web` feature とは独立 |
| 5 | HTTP/3 で要求本文をストリーミングで受けるハンドラ（gRPC のルート）は本文を待たずにタスクで呼び、本文を届いた順に流す（`http3_server.rs` の `Decision::StreamHandler`・`h3_api_stream_task`。HTTP/2 の `h2_api_handler` と同じ形）。双方向が全二重に |
| 6 | `Status::with_details(Bytes)` → `grpc-status-details-bin`（base64、`=` 無し） |
| 7 | `App::grpc_interceptor(prefix, Fn(&Request<()>) -> Result<(), Status>)`（`wrap_on` の上。`Request::method()` を追加） |
| 8 | `[api] grpc_max_send_message_size`（既定 0 = 無制限）。サービスごとの上限は入れていない（必要になったら `veil-build` の登録関数の引数で） |
| 9 | どのルートにも当たらない通常の gRPC（HTTP/2・HTTP/3）は Trailers-Only の `UNIMPLEMENTED`（`cfg(feature = "grpc-handler")` の分岐。プロキシのバイナリは不変） |
| 10 | F-213 に切り出した（HTTP/2 の PING の方針はプロキシと共通の実装の変更になるため） |

試験: 単体（`src/api/grpc/tests.rs`・`web.rs`・`reflection.rs`・`wire.rs`、31 件）、`tests/api/library_e2e.sh grpc`（tonic:
応答の gzip を含む全項目・リフレクション v1/v1alpha、quinn + h3: gzip 応答・非圧縮・全二重の ping_pong・未登録サービス、curl:
gRPC-Web バイナリ（HTTP/1.1）・`-text`（h2c）・CORS・未登録サービス。io_uring と epoll のビルド）。
