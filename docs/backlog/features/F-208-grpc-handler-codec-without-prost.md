# F-208: gRPC ハンドラを prost から切り離す（メッセージの符号化は `veil-build` が生成するコードが担う）

**状態: 完了（feat/api-roadmap-impl、2026-10-14）**

## 背景

F-197 の gRPC ハンドラ（`src/api/grpc/`、`grpc-handler` feature）は、メッセージ型の境界に `prost::Message` を直接使っている
（`App::grpc_unary` 等の `Req: prost::Message + Default`、`Streaming<T: prost::Message>`、`encode_message`、`pub use prost`）。
`grpc` feature（プロキシの gRPC のワイヤ処理）が `dep:prost` を持つので、`grpc-handler` を選ぶと prost の版が veil に固定される。
利用者は veil が再エクスポートする prost に合わせるしかなく、別の protobuf 実装（rust-protobuf・quick-protobuf・
micropb 等）や JSON（`application/grpc+json`）を選べない。

prost を使っているのは 3 か所だけ:

| 場所 | 用途 | 方針 |
|---|---|---|
| `src/api/grpc/mod.rs` | 要求・応答メッセージの符号化と復号 | codec の trait に置き換える |
| `src/api/grpc/health.rs` | `grpc.health.v1` の 2 メッセージ（文字列 1 個・列挙 1 個） | 手書きの符号化にする |
| `src/vds_grpc.rs` | VDS の制御プレーンとの通信 | **このまま prost を使う**（今回の依頼） |

`crates/veil-build`（build-dependency、prost-build を使う）は既にある。

## 設計

### 1. `veil::api::grpc::Codec`

```rust
/// 1 種類のメッセージ型の符号化と復号（`.proto` から生成したコードが実装する）。
pub trait Codec<T>: Clone + Send + Sync + 'static {
    /// `out` の末尾にメッセージを書く（veil が先に 5 バイトの見出しの領域を空けて、書いた後に長さを入れる）。
    fn encode(&self, message: &T, out: &mut BytesMut) -> Result<(), Status>;
    /// 1 メッセージ分（解凍済み）を復号する。
    fn decode(&self, buf: Bytes) -> Result<T, Status>;
}
```

- `Codec<T>` を「型ごとの trait」ではなく「codec の型に対する trait」にするのは orphan rule のため: 生成コードが利用者の
  クレートに置く codec の型（ローカル型）には `impl<T: prost::Message + Default> veil::api::grpc::Codec<T> for ProstCodec`
  と書ける。メッセージ型に直接 veil の trait を実装する形だと、`prost_types::Empty` 等の外部の型を rpc に使えない。
- 見出しの長さは書いた後に埋める（`encoded_len` を先に求める必要が無い。中間バッファも無い）。
- `App::grpc_unary_with(path, codec, f)` 等、4 種類それぞれに codec を取る版を足す。`Request<T>`・`Response<T>`・
  `Streaming<T>`・`Status`・`BoxStream<T>` はメッセージの trait 境界を持たない形にする（`Streaming<T>` は codec を持つ）。
- 組み込みの codec は `BytesCodec`（`Bytes` をそのまま。生成コード無しで使う人・C/Python の橋渡し（F-210）が使う）だけ。
- `content-type` の副型（`application/grpc+proto` / `+json`）は codec の関連定数 `CONTENT_SUBTYPE`（既定 `None` = 何でも受ける）
  で照合できるようにする。

### 2. `veil-build` が生成するもの

- 今と同じく prost-build でメッセージを生成し、出力ファイルの先頭に 1 回だけ codec の型を書く:
  ```rust
  #[derive(Clone, Copy, Default)]
  pub struct ProstCodec;
  impl<T: ::prost::Message + Default> ::veil::api::grpc::Codec<T> for ProstCodec { .. }
  ```
  （パッケージごとのファイルに 1 個ずつ。`include_proto!` するモジュールごとに型が分かれるだけで衝突しない。）
- `add_<service>(app, svc)` は `App::grpc_*_with(path, ProstCodec, ..)` で登録する。サービスの trait は変えない
  （利用者のサービス実装はそのまま動く）。
- 生成コードは `::prost` を直接参照する（veil の再エクスポートを使わない）。利用者の `Cargo.toml` に `prost` が要る
  （今のサンプルも既に依存している）。`veil-build` は prost-build の版と合わせて使う prost の版を README に書く。

### 3. veil 本体の features

```toml
grpc = []                                            # プロキシの gRPC のワイヤ処理（prost を使っていない）
grpc-handler = ["handler", "grpc"]                   # prost 無し
vds = ["dep:prost", "dep:serde_json", "grpc", "http2"]
```

- `pub use prost` を消す。`full`・`full-*` は `vds` 経由で prost を引き続き含む（VDS は変えない）。
- `grpc.health.v1` は 2 メッセージを手書き（varint とタグ 1 個）。単体テストで prost（dev-dependency）と同じバイト列に
  なることを確かめる。

### 4. 生成コード無しで使う

文書の「Without code generation」節を、`#[derive(prost::Message)]` の型に `ProstCodec` を自分で書く例（10 行）と、
`BytesCodec` で生のバイト列を受け取る例に書き換える。

### 5. サンプルと試験

- `examples/api/rust-grpc`: 新しい生成コードで動くようにする（サービスの実装は変えない想定）。prost は利用者の依存として残る。
- `tests/api/library_e2e.sh grpc`（tonic クライアント・quinn + h3 クライアント、epoll ビルドでも）が通ること。
- `nonregression`: `grpc-handler` だけのビルド（`--no-default-features --features "http2,grpc-handler"` 相当）で `cargo tree` に
  prost が出ないことを検査に足す。`full` に `veil_build::`・`prost_build::` が無い検査はそのまま。
- `src/api/grpc/tests.rs` を codec 経由に書き換え（テストでは prost を dev-dependency で使う）。

## 互換性

`App::grpc_unary` 等の codec 無しの版は消す（prost の型を受ける版を残すと prost 依存が戻る）。`veil-build` の生成コードを
使っている利用者は再ビルドするだけで移行できる。リリースノートに書く。

## 受け入れ条件

- `cargo tree -e normal --features "handler,grpc-handler"`（vds 無し）に prost が出ない。
- `examples/api/rust-grpc` が `library_e2e.sh grpc` で通る（Linux io_uring・epoll、他 OS は F-203 の手順で 1 回）。
- `docs/guide/library.md`（英日）の gRPC 節・`crates/veil-build/README`（無ければ作る）・`backlog.md` を更新する。

## 関連

F-197、F-209（gRPC の未対応機能。サーバリフレクションは記述子を `veil-build` が埋め込む形で本チケットの上に作る）、
F-210（C・Python の gRPC は `BytesCodec` の上に作る）。

## 実施内容（2026-10-14）

- `veil::api::grpc::Codec<T>`（`encode`・`decode`・任意の `encoded_len`）と `BytesCodec`。`Bytes`・`BytesMut` を再エクスポート。
- 登録のメソッドは名前を変えずに **第 2 引数に codec** を取る形にした（`grpc_unary(path, codec, f)` 等）。設計の
  `grpc_*_with` は、codec 無しの版を残さないので接尾辞だけが残る形になるため採らなかった。
- `Streaming<T>` は登録時に 1 回作った復号関数（`Arc<dyn Fn(Bytes) -> Result<T, Status>>`）を持つ（呼び出しごとの確保は増えない）。
  応答は 5 バイトの見出しの領域を空けて codec に書かせ、後から長さを入れる（`encoded_len` が分かれば確保 1 回）。
- `grpc.health.v1` は手書きの符号化（`HealthCodec`、`src/api/grpc/wire.rs` の最小の varint・長さ付きフィールド・未知フィールドの読み飛ばし）。
  単体試験で prost（dev-dependency）と同じバイト列であることを確かめる。
- features: `grpc = []`、`vds` が `dep:prost` を持つ。`pub use prost` を削除。
- `veil-build`: サービスのモジュールに `ProstCodec`（`impl<T: prost::Message + Default> Codec<T>`）を生成し、`add_<service>` は
  それで登録する。`crates/veil-build/README.md` を新設。
- 設計の `CONTENT_SUBTYPE`（`application/grpc+json` 等の照合）は入れていない。受け付けは今どおり `application/grpc*` で、
  副型の扱いは codec を選ぶ利用者に任せる（必要になったら F-209 の続きとして足す）。
- 試験: 単体（`health_codec_matches_prost`・`bytes_codec_roundtrip`・wire の varint と読み飛ばし）、`library_e2e.sh grpc`
  （tonic・quinn + h3、io_uring・epoll）、`nonregression` に「`http2,grpc-handler` の依存に prost が無い」「`api` に async-graphql が無い」を追加。
