# F-207: async-graphql への依存をやめる（GraphQL のライブラリは利用者が選ぶ）

**状態: 完了（feat/api-roadmap-impl、2026-10-14）**

## 背景

F-191 で `graphql` feature（`dep:async-graphql`）と `App::graphql(path, schema)`（`src/api/graphql.rs`、88 行）を入れた。
`api-core`・`api` に含まれるので、`api` を選んだ利用者は GraphQL を使わなくても async-graphql（と依存ツリー）をビルドする。
GraphQL のライブラリは async-graphql の他に juniper・cynic（クライアント）等があり、版の上げ方も利用者ごとに違う。

`App::graphql` が使っているのは、どのハンドラでも使える機能だけ（`Request::method`・`query()`・`header()`・`body()`、
`Response::html`・`with_header`・`with_body`、`route_fn` と `OperationMeta`）。veil 側に GraphQL 固有の処理は要らない。

## 設計

### veil 本体

- `Cargo.toml`: `graphql` feature と `async-graphql` 依存を消す。`api-core` から `graphql` を外す。
- `src/api/graphql.rs` と `src/api/mod.rs` の `mod graphql` を消す。
- `tests/api/library_e2e.sh nonregression` の「full に async-graphql が無い」検査は残す（再混入の防止）。
- 互換性: `App::graphql` は消える（破壊的変更）。移行はサンプルの 60 行ほどの関数をコピーする。リリースノートに書く。

### サンプル `examples/api/rust-graphql`（新規、独立クレート `[workspace]`）

- 依存: `veil`（`api`）・`async-graphql`・`serde_json`。
- `src/graphql.rs` に今の `graphql.rs` の `execute`（GET のクエリ文字列・POST の JSON・バッチ・GraphiQL・
  `application/graphql-response+json` の選択）を利用者のコードとして置き、`App::route_fn` で `GET`・`POST` を登録する
  関数 `fn graphql(app: App, path: &str, schema: Schema<..>) -> App` にする。
- `examples/api/rust-showcase` の GraphQL 部分をこのサンプルへ移す（showcase は `graphql` feature を使わなくなる）。
- サブスクリプションは `Response::stream` の SSE（GraphQL over SSE の distinct connections モード）で例を付ける
  （WebSocket は API ライブラリに無い。F-209 の表を参照）。

### 文書

- `docs/guide/library.md`（英日）: Cargo features の表から `graphql` を消す。GraphQL 節を「任意のライブラリをハンドラで
  載せる」に書き換え、async-graphql の例（サンプルへのリンク）と、juniper 等でも同じ形で書けることを記す。Limitations の
  「GraphQL subscriptions are not supported」を SSE の例への案内に変える。
- F-191 のチケットに「F-207 で feature を廃止」と追記。

### 試験

- `tests/api/library_e2e.sh` の Rust 節の GraphQL の確認（showcase に対して行っているもの）を `rust-graphql` に向ける
  （query・mutation・GET・バッチ・GraphiQL・SSE のサブスクリプション）。
- `cargo test --features api` と `cargo clippy --features api` から GraphQL のテストが消えることを確認。

## 受け入れ条件

- `cargo tree --features api -p veil` に async-graphql が出ない。
- `examples/api/rust-graphql` が `library_e2e.sh` で通る（Linux。他 OS は F-203 の手順で 1 回）。
- 文書（英日）・`backlog.md` が更新されている。

## 実施内容（2026-10-14）

- `graphql` feature・`async-graphql` 依存・`src/api/graphql.rs` を削除し、`api-core` から外した（`cargo tree --features api` に
  async-graphql が出ない）。
- `examples/api/rust-graphql`（新規）: `src/graphql.rs`（コピーして使う橋渡し、GET・POST・バッチ・GraphiQL・
  `application/graphql-response+json`・GraphQL over SSE）と、問い合わせ・変更・サブスクリプション（`countdown`、
  `veil::tokio::time::sleep`）の `main.rs`。rust-showcase から GraphQL を外した。
- `tests/api/library_e2e.sh` に `run_graphql`（`rust` と `all` で実行）: POST・GET・変数・変更・バッチ・エラー・
  応答のメディア型・GraphiQL・SSE のサブスクリプション（HTTP/1.1・h2c）。`nonregression` の async-graphql 検査は残した。
- 文書: `docs/guide/library.md`（英日）の GraphQL 節・features の表・Limitations、`docs/guide/features.md`（英日）、AGENTS.md。
