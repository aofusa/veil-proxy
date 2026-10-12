# F-188: API サーバ基盤ライブラリ（Rust）— Handler・マクロ・設定ビルダ・ライブラリ既定設定

**状態: 完了（feat/f188-api-library-vds-k8s）**

## 背景・目的

veil を他の Rust アプリにライブラリとして組み込み、veil のネットワーク層・TLS・HTTP/1.1・2・3・独自ランタイムの上で
API サーバを書けるようにする。ルーティングは Rust のマクロ（`#[veil::route]`）で宣言し、Rust 関数をハンドラとして登録する。
起点の要望・設計書は `docs/artifacts/veil-api-server-design-final.md`（ユーザ提示）。

## 要件

- `#[veil::route(path = "/users/{id}", method = "GET")]` で async 関数をハンドラ登録（内部的には `type = "Handler"` のルート）。
- パスパラメータ・クエリ・JSON 本文を引数で受け取り、`Json<T>` / `Query<T>` は自動バリデーション（失敗は 422、FastAPI 互換の形）。
- `config.toml` を取り込み、Rust コードのビルダで任意の設定を上書きできる（SIGHUP リロード後も上書きは保たれる）。
- **ライブラリ専用の既定設定**: config.toml が無くても動く。平文 HTTP（HTTP/1.1 + h2c）で 0.0.0.0:8080、TLS 不要、スレッド数は CPU 数。
- ルートは記述順に依存せず具体性で並べ替え、同一条件の衝突は起動エラー。
- **veil 本体（feature 指定なしのビルド）にはライブラリ用コードを一切含めない。性能退行なし。**

## 設計（要点）

- feature: `handler`（基盤）、`macro`（proc-macro クレート `crates/veil-macros`）、`api`（便利セット）。
- `BackendConfig::Handler` / `Backend::Handler` は `cfg(feature = "handler")` のときだけ存在し、HTTP/1.1・HTTP/2・HTTP/3 の
  ディスパッチに 1 腕ずつ足す。ハンドラは設定ロード時に名前解決して `Route.resolved_backend` に持つ（リクエスト時の検索なし）。
- 設定は `config_override::apply_to_toml_str`（全ロード経路の単一チョークポイント）に `handler` feature 時だけのフックを置き、
  既定設定との深いマージ・ビルダ上書き・ハンドラルートの注入・具体性ソートと衝突検査を行う。
- 本体側の小改修（ライブラリ有無に関係なく有用）: `[tls]` を省略した H2C 専用（平文）サーバ構成を許す、
  新キー `[server].h2c_http1` で H2C 専用リスナーが平文 HTTP/1.1 も受ける（既定 false で既存挙動は不変）。
- 詳細: `docs/artifacts/f188-implementation-design.md`。

## 受け入れ条件

- `cargo build --features full` のバイナリに `veil::api` 由来のシンボルが無い。`tools/perf/` の交互 A/B で退行なし。
- 単体テスト（パスパターン・抽出子・設定マージ・ソート・衝突）、E2E（examples/api/rust-* を起動して curl / HTTP/2 / h2c で叩く）。
- `docs/guide/library.md`（英日）と `examples/config.toml` の更新。

## 結果（2026-10-12、feat/f188-api-library-vds-k8s）

- `src/api/`（handler・request/response・抽出子・パスパターン・ルート注入と具体性ソート・衝突検査・ライブラリ設定・App/serve）、
  `crates/veil-macros`、本体側は `Backend::Handler` の 3 経路のディスパッチ・`{param}` 照合（`handler` 時のみ）・
  `[tls]` 省略可の平文専用構成・`[server].h2c_http1`・`entry::run_server`・offload の完了待ち一般化とハンドラ専用プール。
- ライブラリとして使うときは `#[global_allocator]` を定義しない（利用者が選ぶ）。
- 試験: 単体（api 33 件ほか）、standalone E2E（平文専用構成 3 件）、`tests/api/library_e2e.sh`（Rust 31 項目・C 10 項目・Python）、
  full ビルドのバイナリに `veil::api`・inventory・async-graphql・tokio・pyo3 のシンボルが無いこと。性能の交互 A/B は docs/perf/ に記録。
