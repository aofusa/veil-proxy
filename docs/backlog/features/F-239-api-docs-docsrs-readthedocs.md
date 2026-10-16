# F-239: Rust の API ライブラリと Python のモジュールの文書（docs.rs・Read the Docs）

**状態: 未着手（2026-10-16 起票・設計。F-238 の公開と同時に使えるようにする）**

## 目的

F-238 で公開したときに、それぞれの文書のサイトで API の文書を読めるようにする。

- Rust: docs.rs の `https://docs.rs/veil-proxy`。
- Python: PyPI のプロジェクトのページと、Read the Docs の `https://veil-api.readthedocs.io`。

今は利用者向けの説明が `docs/guide/library.md` に 1 本あるだけで、型・関数ごとの API の文書は無い。

- Rust: `src/lib.rs` の crate の doc は「fuzz・統合テスト向け」の 2 行だけである。
- Python: 型の情報（`.pyi`）が無い。`_veil.abi3.so` のほかには、`__init__.py`・`grpc.py`・`py.typed` しかない。

## 設計

### Rust（docs.rs）

- **docs.rs のビルドの設定（`Cargo.toml` の `[package.metadata.docs.rs]`）:**
  - `features = ["full", "api"]`、`rustdoc-args = ["--cfg", "docsrs"]`。
  - `targets = ["x86_64-unknown-linux-gnu"]`（1 つにして、ビルドの時間の上限に収める）。
  - `#![cfg_attr(docsrs, feature(doc_cfg))]` を付けて、どの feature が要る項目かを文書に出す。
- **docs.rs で本当にビルドできるかを、段階 0 で確かめる。**
  - docs.rs はネットワーク無し・時間と記憶の上限ありでビルドする。`.cargo/config.toml` の `[env]` も読まない。
  - rustdoc はリンクしないので、F-238 の項目 3（aws-lc-sys のプレフィックス）は文書のビルドには関係しないはずである。
  - aws-lc-sys・quiche・wasmtime のビルドスクリプトが、時間内に通るかを確かめる。
  - 確かめ方: docs.rs のビルドの環境のイメージ（`ghcr.io/rust-lang/crates-build-env/linux`）と `cargo +nightly doc` を
    ネットワーク無しで回し、時間を測る。
  - 時間に収まらなければ、`features` から重いもの（`wasm`・`http3`）を外す。外した機能は、文書に「docs.rs では
    この feature を除いている」と書く。
- **公開の範囲を絞る。**
  - `veil` クレートは、プロキシの内部のモジュールも `pub` で出している。lib.rs の `pub mod`・`pub use` は 59 個ある。
    これらは fuzz・統合テスト・mesh のために公開しているので、可視性は変えない。
  - 代わりに、利用者向けでないモジュールに `#[doc(hidden)]` を付ける。docs.rs の目次に出すのは次の 3 つだけにする:
    `veil::api`（とその下）・`veil::tokio`・`veil::veil_alloc::VeilAlloc`。
  - crate の doc（`src/lib.rs` の `//!`）は、`docs/guide/library.md` の最初の節を元に書き直す。依存の書き方
    （`veil = { package = "veil-proxy", .. }`）・feature の選び方・最小の例・アロケータ・F-238 の項目 3 の設定を入れる。
- **品質の検査（CI）:**
  - `veil::api` の下に `#![warn(missing_docs)]` を付け、CI の clippy の `-D warnings` で戻りを防ぐ。
  - 文書の中の例は doctest にして、CI で `cargo test --doc --features full,api` を回す。サーバを起動する例は `no_run` にする。
  - `RUSTDOCFLAGS="--cfg docsrs -D warnings" cargo +nightly doc --features full,api --no-deps` で、壊れたリンクを検出する。
- **ほかのクレート:** `veil-proxy-macros`（`#[veil::route]`・`#[veil::main]`・`#[derive(Schema)]` の属性の書き方）と
  `veil-proxy-build`（`compile_protos` と生成されるもの）にも、crate の doc と例を書く。各クレートの `README.md` を、crates.io の
  ページに出す（`readme = "README.md"`）。
- `Cargo.toml` に `documentation`・`homepage`・`repository`・`keywords`・`categories` を付ける。

### Python（PyPI と Read the Docs）

- **型の情報（`.pyi`）を作る。**
  - 拡張モジュールの公開する型・関数ごとに、`python/veil/_veil.pyi` に型と docstring を書く。
  - PyO3 の `#[pyo3(text_signature)]` と doc コメントも、Rust 側に書く。`help()` で読めるようにするため。
  - `.pyi` と実物のずれは、CI の `python -m mypy.stubtest veil` で検出する（wheel をビルドした後に回す）。
- **Read the Docs:**
  - Sphinx と `sphinx-autoapi` で作る。autoapi は `.pyi` と `.py` を静的に読むので、Read the Docs のビルドで Rust の拡張を
    コンパイルしなくてよい。
  - `.readthedocs.yaml` は `crates/veil-python/docs/` を指す。
  - ページの構成: 入門（FastAPI からの移行の対応表）・経路とデコレータ・要求と応答・依存注入・WebSocket・gRPC（`veil.grpc`）・
    設定・API の一覧（autoapi）。
  - 入門の内容は `docs/guide/library.md` の Python の節から移す。同じ内容を 2 箇所に書かないよう、library.md からは
    Read the Docs へリンクする。英語を正とし、日本語は Sphinx の国際化（gettext）で `ja` の版を作る（docs/guide の英日の方針に
    合わせる）。
  - 版: Read the Docs の自動の版（タグを push すると、その版の文書ができる）を有効にし、`stable` を最新のタグに向ける。
    Read the Docs のプロジェクトの作成と GitHub との連携は、人が 1 回だけ行う（`packaging/README.md` に手順を書く）。
- **PyPI のページ:**
  - pyproject の `readme = "README.md"` で、`crates/veil-python/README.md` を出す。
  - `[project.urls]` に `Documentation`（Read the Docs）・`Source`・`Changelog` を付ける。
  - `classifiers` に、wheel を出す OS（F-238）を書く。

## 受け入れ条件

- docs.rs の環境のイメージで、`veil-proxy`・`veil-proxy-macros`・`veil-proxy-build` の文書が上限の時間内にビルドできる
  （段階 0 の時間を記録）。
- 0.8.0 の公開の後、次の 4 つが開け、目次・例・feature の表示が出る:
  - `https://docs.rs/veil-proxy/0.8.0`
  - `https://veil-api.readthedocs.io/en/stable/`
  - crates.io のページ
  - PyPI のページ
- CI の検査が通る: `missing_docs`・doctest・`RUSTDOCFLAGS=-D warnings` の rustdoc・`mypy.stubtest`・Sphinx の `-W`（警告を
  失敗にする）。
- `docs/guide/library.md`（英日）から、docs.rs と Read the Docs へリンクする。

## 関連

F-238（公開）、F-231（版）、F-188〜F-196（API ライブラリ）、`docs/guide/library.md`、`crates/veil-python/`。
