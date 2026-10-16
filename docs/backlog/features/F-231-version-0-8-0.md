# F-231: バージョンを v0.8.0 にする（版の出所を 1 つにする）

**状態: 完了（feat/v080-release、2026-10-16）**

## 目的

Cargo.toml のバージョンを 0.8.0 にする。あわせて、版の書き場所を 1 つにまとめ、タグと版がずれないようにする。
F-237（コンテナイメージ）と F-238（crates.io・PyPI）は、タグの版で公開する。

## 今の状態（2026-10-16 の main）

- 版の書き場所が 6 つある。
  - ルートの `Cargo.toml`
  - `crates/veil-macros`・`veil-capi`・`veil-build`・`veil-python` の各 `Cargo.toml`
  - `crates/veil-python/pyproject.toml`
- ルートから `veil-macros` への依存にも、`version = "0.7.0"` が書いてある。
- タグ `v0.7.1` があるのに、Cargo.toml は 0.7.0 のままである。タグと版がずれる事故が既に起きている。
- 文書・台本の中の `0.7.0` は、用途の違う 3 種類が混ざっている。
  - 例として今の版を書いたもの: `docs/guide/building.md`・`vds.md`・`performance.md`（英日）、`packaging/README.md`。
  - 計測の基準の版として書いたもの: `docs/perf/`・`tools/perf/suite_ab.sh`（main との比較の既定の基準）。
  - その他: `tools/qemu/*`・`.github/workflows/release-sbom.yml` の例。

## 設計

- **版の出所を `[workspace.package] version` の 1 箇所にする。**
  - ルートと `crates/*` の `[package]` は、`version.workspace = true` を使う。
  - `veil-macros` への依存の版も、ワークスペースの依存（`[workspace.dependencies]`）に寄せる。
  - `pyproject.toml` は `dynamic = ["version"]` にする。maturin が `crates/veil-python/Cargo.toml` から版を読むので、Python の
    版を別に書かなくて済む。
- **版を上げる台本 `tools/release/bump-version.sh <X.Y.Z>`:**
  - ワークスペースの版を書き換え、`cargo update -w` で Cargo.lock を更新する。
  - 次の Cargo.lock の中の veil の版も更新する: `examples/api/*`・`tools/api-bench/*`・`tools/perf/h3load`・
    `tools/tokio-compat/ecosystem`・`tests/*/`。どれも path 依存なので、`cargo update -p veil` で済む。
  - 「今の版の例」として書いた文書の箇所を書き換える。計測の基準の版として書いた箇所は、意味が変わるので書き換えない。
    書き換える対象は、台本の中の表で明示する（`git grep` の一括置換にしない）。
- **タグの検査:** F-237・F-238 の公開の workflow の最初のジョブで、タグ（`v` を除く）と `cargo metadata` の版を比べる。
  違えば何も公開せずに失敗させる。
  - CI でタグから版を書き換える案は採らない。タグのコミットのソースと公開物の中身がずれ、再現できなくなるため。
- **0.8.0 の中身:**
  - ワークスペースの版を 0.8.0 にする。
  - 「今の版の例」として書いた文書の箇所を 0.8.0 にする。
  - リリースノートは `packaging/README.md` の手順に従う。
- 版を 0.8.0 にするのは、F-238 の名前の変更（crates.io の `veil` は他のプロジェクトが使っている）と同じリリースに入れる。
  先に版だけ上げてもよいが、タグ `v0.8.0` は F-237・F-238 の workflow と一緒に打つ。

## 受け入れ条件

- `cargo metadata` で、全メンバーの版が 0.8.0 になっている。
- `maturin build` の wheel の版が 0.8.0 になっている。
- `--no-default-features`・default・`full`・`full,epoll` の clippy とビルドが通る。
- `bump-version.sh` を同じ版で 2 回実行しても、差分が出ない（冪等）。
- `docs/guide/building.md`（英日）に版の上げ方を書き、`packaging/README.md` のリリース手順を更新する。


## 実装の結果

- `[workspace.package] version = "0.8.0"`。ルートと `crates/*` は `version.workspace = true`、pyproject は `dynamic = ["version"]`。
- `tools/release/bump-version.sh`（2 回実行して差分が出ないことを確認）、`tools/release/verify-tag.sh`（タグと版・main）。
- `docs/guide/building.md`（英日）の「版とリリース」、`packaging/README.md` の「公開」。

## 関連

F-237、F-238、F-239、`packaging/README.md`。
