# F-241: docs.rs の文書を英語だけにし、チケット番号と経緯を出さない

**状態: 設計（feat/v083-release、2026-10-21。v0.8.3 で公開）**

## 目的

docs.rs（`veil-proxy`・`veil-proxy-macros`・`veil-proxy-build`）に出る文書を次のようにする。

- 英語だけで書く。
- チケット番号（`F-123`・`B-45` など）を出さない。
- 今の動作と注意だけを書く。「以前は〜だった」「v0.x で変えた」「計測では〜」のような経緯は書かない。

## 今の状態（2026-10-21 の main = v0.8.2）

- docs.rs のビルドは 3 つとも通っている（`https://docs.rs/crate/<name>/0.8.2/status.json` が `doc_status: true`）。
- crate の doc（`src/lib.rs` の `//!`）は英語である。しかし、その下に並ぶ項目の doc はほとんど日本語である。
  F-239 で目次に出す範囲を絞ったが、その範囲の中身は訳していない。
- 文書に出る範囲にある doc コメント（`///`・`//!`・`#[doc = ..]`）の数は次のとおり。非公開の項目も数に含めた概数で、
  公開の項目だけを訳せばよい。

  | 範囲 | doc の行 | 日本語を含む | チケット番号を含む |
  |---|---|---|---|
  | `src/api/`（`veil::api`） | 841 | 739 | 91 |
  | `src/tokio/`（`veil::tokio`） | 521 | 442 | 19 |
  | `src/veil_alloc/`（`veil::veil_alloc`） | 243 | 217 | 9 |
  | `src/lib.rs`・`src/entry.rs`（ルートの `run`・`run_server`・`ServerOptions`） | 181 | 59 | 20 |
  | `crates/veil-macros` | 60 | 37 | 1 |
  | `crates/veil-build` | 31 | 21 | 3 |

- doc の文字列をマクロで組み立てている箇所もあるので、`///` を数えるだけでは見落とす。
  - `src/tokio/signal.rs`（Windows のシグナルの種類）・`src/tokio/io/util.rs`・`src/tokio/runtime.rs`・
    `src/api/grpc/mod.rs`。
- **veil-build が生成するコードの doc も日本語である。** `crates/veil-build/src/lib.rs` の 159・171・183・247 行目。
  利用者のクレートの中に生成されるので、利用者の文書（`cargo doc`）に日本語が出る。
- docs.rs のソースの表示（`[src]`）には、日本語のコメントがそのまま出る。
- `crates/README.crates-io.md`（crates.io のページ）は英語で、チケット番号も無い。ただし `.cargo/config.toml` の `[env]` の
  説明があるので、F-243 で書き直す。
- `crates/veil-build/README.md` の 37 行目に `(F-208)` がある。

## 設計

### 訳す範囲

- 対象は rustdoc が出す文書だけにする。
  - `#[doc(hidden)]` を付けたモジュールと、非公開の項目の doc は対象外（docs.rs に出ないため）。
  - ただし、同じファイルの中で公開の項目だけ英語にすると、どれが英語であるべきか分かりにくい。そこで、
    次のディレクトリの中の doc コメントは、非公開の項目も含めてすべて英語にする。
    - `src/api/`、`src/tokio/`、`src/veil_alloc/`、`crates/veil-macros/`、`crates/veil-build/`
    - `src/lib.rs` と `src/entry.rs` は、公開の項目の doc だけを英語にする。
- 普通のコメント（`//`）は日本語のままにする（AGENTS.md「既存に合わせる」）。
- doc コメントに書いていたチケット番号と経緯は、すぐ上の `//` コメントへ移す。設計の追跡には要るので、消さない。

  ```rust
  // F-197: ストリーミング受信。本文の上限は掛けない（gRPC はメッセージ単位で検査する）。
  /// Receives the request body as a stream. No body size limit is applied; gRPC checks each message.
  pub fn streaming(...)
  ```

- 書く内容は「今の動作・引数と戻り値・エラー・注意（プラットフォームと feature の制約）・例」に限る。性能の数字は書かない。

### 生成されるコード（veil-build）

- 生成する doc の文字列を英語にする。

### ソースの表示

- `#![cfg_attr(docsrs, doc(html_no_source))]` を `veil-proxy`・`veil-proxy-macros`・`veil-proxy-build` の crate root に付ける。
  docs.rs のソースの表示を止める。
  - こうすれば、`//` の日本語のコメントは docs.rs に出なくなる。
  - ソースは crates.io のパッケージと GitHub で読める（`repository` は Cargo.toml に書いてある）。
- 代わりに `//` のコメントもすべて英語にする案は、採らない。AGENTS.md の規約（日本語のコメント）と食い違ううえ、
  訳す量が 1 桁増えるため。

### 戻りを防ぐ検査

- `tools/release/check-docs.sh` を足す。
  1. docs.rs と同じ設定で文書を作る。

     ```bash
     RUSTDOCFLAGS="--cfg docsrs" cargo +nightly doc --no-deps --no-default-features --features full,api \
       -p veil-proxy -p veil-proxy-macros -p veil-proxy-build
     ```

  2. 生成された HTML の本文（タグの外の文字）を見る。対象は `target/doc/{veil,veil_macros,veil_build}/` の下で、
     `target/doc/src/` は除く。次のどちらかがあれば、ファイルと前後の文を出して失敗させる。
     - CJK の文字（ひらがな・カタカナ・漢字）
     - チケット番号の形（`\b[FB]-[0-9]+\b`）
  3. veil-build の生成物は、`tests/api/` の gRPC の例で生成したコードの `cargo doc` を同じ条件で検査する。
- CI（`ci.yml` の既存の `cargo doc` のジョブ）でこの台本を回す。nightly が要るので、そのジョブだけ nightly を入れる。
  - stable の `cargo doc`（`-D warnings`）は、そのまま残す。
  - nightly が要るのは `doc_cfg` のためである。
- 例外の指定は設けない。英語の文書に日本語が要る場面（文字列の例など）は、Unicode のエスケープで書く。

## 受け入れ条件

- `tools/release/check-docs.sh` が通る。
- CI で同じ検査が回る。
- 0.8.3 の公開の後、docs.rs の次のページの目次・本文に日本語とチケット番号が無い。
  - `https://docs.rs/veil-proxy/0.8.3/veil/`
  - `https://docs.rs/veil-proxy-macros/0.8.3`
  - `https://docs.rs/veil-proxy-build/0.8.3`
- docs.rs にソースの表示が出ない。
- `cargo test --doc --no-default-features --features full,api` が通る（訳したときに例を壊していない）。
- `missing_docs` の警告が増えていない。

## 範囲外

- **PyPI のページ（`crates/veil-python/README.md`）と Python の docstring。**
  今も日本語で、チケット番号もある。PyPI と Read the Docs は docs.rs と同じ問題を持つが、依頼の範囲が docs.rs だけなので、
  別のチケットにする。
- `docs/guide/`（英日の両方がある）と、GitHub だけで読む README。

## 関連

- [F-239](F-239-api-docs-docsrs-readthedocs.md)（docs.rs の設定と目次の範囲）
- [F-243](F-243-library-without-cargo-config-env.md)（crate の doc の「HTTP/3 on Linux」の節を書き直す）
- [F-245](F-245-version-0-8-3.md)
