# F-249: `cargo install`・`cargo binstall` で入れられるようにする

**状態: 実装済み（feat/distribution-tickets、2026-10-23。v0.9.0 で公開）**

## 目的

- `cargo install veil-proxy --locked`（ソースからビルド）で、`veil` のバイナリが入るようにする。
- `cargo binstall veil-proxy`（ビルド済みのバイナリを取る）で、`veil` のバイナリが入るようにする。
- どちらも入るのはバイナリだけ。設定ファイルとページは利用者が用意する前提にする（F-247）。
  - 既定のパス（`/etc/veil/config.toml`、Windows は `%ProgramData%\veil\config.toml`）に置くか、`-c`・`VEIL_CONFIG` で渡す。
  - その前提と、最小の設定・ページの用意のしかたを、文書と `--help` に明記する。
  - 設定ファイルが無いときは、置き場所と対処を示すエラーで止まる（F-247）。

## 今の状態

- crates.io の `veil-proxy` はライブラリと `[[bin]] name = "veil"` を持つ。
  - パッケージには `Cargo.lock` が入る（`cargo package --list` で確かめた）。
  - よって `cargo install --locked` は、リポジトリと同じ依存の版でビルドできるはず。
- **確かめたこと:** ライブラリとしての利用（`.cargo/config.toml` 無しで HTTP/1.1・2・3）は、v0.8.3・v0.8.4 の公開の後に確かめた（F-243・F-245）。
- **確かめていないこと:**
  - `cargo install` でバイナリを入れること。
  - `--features full` などの feature セットが、各 OS でビルドできること。
- **既定の feature:** `default = ["ktls", "http2", "mimalloc"]`。HTTP/3・gRPC・WASM・メトリクスは入らない。
  default は変えない（AGENTS.md）。
- **binstall:** `[package.metadata.binstall]` が無い。
  binstall は GitHub Release の成果物を名前から推測するが、v0.8.1 以降は成果物が無い（F-247）。
- **設定ファイル:** 引数無しの `veil` は `/etc/veil/config.toml` が無いと起動しない。
  エラーは置き場所を示さない（F-247 で文を直す。内蔵の設定には落とさない）。

## 設計

### 既定の設定とページを入れるか（検討の結果: 入れない）

他のパッケージマネージャ（F-250〜F-253）と同じく、各プラットフォーム向けの既定の設定とページを入れられるかを検討した。
cargo・binstall の自然な動きではないので、**バイナリだけを入れる**ことにした。

- **`cargo install` の動き:**
  - 入れるのは `[[bin]]` の実行ファイルを `~/.cargo/bin`（`--root` の `bin/`）へ置くことだけ。
  - インストールの後に処理を走らせる仕組み（post-install のフック）は無い。
  - データのファイル・設定を置く仕組みも無い。
- **`cargo binstall` の動き:**
  - アーカイブから `bin-dir` で指した実行ファイルを、同じ場所へ置くだけ。
  - アーカイブの他のファイル（F-247 の `examples/`）は入れない。
- **退けた案:**
  - **`build.rs` で `/etc/veil/` などへ書く:**
    - ビルドが `target/` の外を書き換えることになる。cargo の前提に反し、サンドボックスのビルドで失敗する。
    - root の権限が要る場所もある。
    - 一般の利用者の権限でビルドすると、端末全体の設定の場所に書けない。
    - 安全上も、ビルドの副作用で設定を置くべきではない。
  - **初回の起動で設定を自動で作る:** 設定を内蔵しない方針（F-247）に反する。
    設定が無いときに黙って何かを作って動くのは、`-c` の打ち間違いなどを隠す。
  - **`veil init` のような、設定とページを書き出すコマンドを足す:** これも雛形をバイナリに内蔵することになり、F-247 の方針に反する。
    要望があれば別のチケットで検討する。
- **結論:**
  - `cargo install`・`cargo binstall` で入るのはバイナリだけ。
  - 設定ファイルとページは利用者が用意する（下の文書に手順を書く）。
  - 既定の設定とページが入った状態ですぐに試したい人には、パッケージマネージャ（F-250〜F-253）かコンテナ（F-246）を勧める。
    このことも文書に書く。

### `cargo install`

- 各 OS で勧めるコマンドを文書にする。
  - Linux: `cargo install veil-proxy --locked --features full`
  - macOS: `--features full-macos`
  - Windows: `--features full-windows`
  - FreeBSD・OpenBSD・NetBSD: `--features full-freebsd`・`full-openbsd`・`full-netbsd`
  - 既定の feature だけなら、`cargo install veil-proxy --locked`。
- 前提のツール（cmake・nasm・C コンパイラ。OS ごとの差）を `docs/guide/building.md`（英日）に書く。
- **crates.io からのビルドでしか起きないこと**を確かめる。
  - vendoring の `third_party/` は crates.io のパッケージに入らない。
  - BSD aarch64・NetBSD の wasmtime はターゲット別の依存で vendoring 版を選んでいる（B-55）ので、crates.io から入れたときに何が選ばれるかを確かめる。
    使えない組み合わせは文書に書き、ビルドが分かりやすいエラーで止まるようにする（`compile_error!` か、feature の説明）。
  - `[patch.crates-io]` は利用者側では効かない。quiche は `veil-proxy-quiche` で解決済み（F-243）。
    他に patch に頼っているものが無いかを洗う。
- **試験:** `release.yml` に `cargo-install` ジョブを足す。
  - crates.io へ公開した後に、各 runner（Linux x86_64・aarch64、macOS、Windows）で回す。
    1. `cargo install veil-proxy@<ver> --locked --features <OS のセット>`
    2. 設定ファイルを置かずに引数無しで起動し、F-247 のエラーで止まることを確かめる。
    3. 文書の「最小の設定」を書いた `config.toml` と `index.html` を用意し、`-c` で起動する。
       `/`・h2c・（`full` なら）HTTP/3 を確かめる。
  - rc では crates.io に公開しない（dry-run）。そこで、パッケージの tarball（`cargo package` の出力）を展開し、
    同じコマンドを `--path` で回す。crates.io から入れるときと同じく、`third_party/` の無い状態で確かめる。
  - BSD は F-247 の VM の workflow で `cargo install --path` を回す。

### `cargo binstall`

- `Cargo.toml` の `veil-proxy` に `[package.metadata.binstall]` を足す。

  ```toml
  [package.metadata.binstall]
  pkg-url = "{ repo }/releases/download/v{ version }/veil-{ version }-{ target }{ archive-suffix }"
  bin-dir = "veil-{ version }-{ target }/{ bin }{ binary-ext }"
  pkg-fmt = "tgz"

  [package.metadata.binstall.overrides.x86_64-pc-windows-msvc]
  pkg-fmt = "zip"
  [package.metadata.binstall.overrides.aarch64-pc-windows-msvc]
  pkg-fmt = "zip"
  # macOS は universal2 の 1 つのアーカイブを両方の target で使う
  [package.metadata.binstall.overrides.aarch64-apple-darwin]
  pkg-url = "{ repo }/releases/download/v{ version }/veil-{ version }-universal2-apple-darwin.tar.gz"
  bin-dir = "veil-{ version }-universal2-apple-darwin/{ bin }"
  [package.metadata.binstall.overrides.x86_64-apple-darwin]
  pkg-url = "{ repo }/releases/download/v{ version }/veil-{ version }-universal2-apple-darwin.tar.gz"
  bin-dir = "veil-{ version }-universal2-apple-darwin/{ bin }"
  ```

  - `{ repo }` は `repository` のキーから決まる。今の値が `https://github.com/aofusa/veil-proxy` であることを確かめる。
- **署名の検証:**
  - binstall は minisign の署名を検証できる（`[package.metadata.binstall.signing] algorithm = "minisign"`、公開鍵を書く）。
  - F-247 で各アーカイブに `.sig`（minisign）を付け、署名の無い・合わないものは binstall が拒否するようにする。
  - 秘密鍵は environment `release` の secret に置く。鍵を入れ替える手順を文書にする。
- **binstall が見るのは crates.io に公開した `Cargo.toml`:** metadata は次の版から効く。
  - 古い版（〜v0.8.4）は推測に頼る。成果物が無いので、ソースからのビルドに落ちる。
- **試験:**
  - 公開の後に各 runner で `cargo binstall veil-proxy@<ver> --no-confirm --disable-strategies compile` を回す。
    起動は `cargo install` と同じ手順（設定が無いときのエラー、用意した設定での起動）で確かめる。
  - binstall はアーカイブの中のバイナリだけを入れる。アーカイブの `examples/` は入らないことを文書に書く。
    `compile` を止めて、ビルド済みのバイナリを取れたことを確かめる。
  - rc では成果物が draft にある（公開されていない）。次の手順で同じことをする。
    1. `gh release download` で取り、runner の中の HTTP サーバで配る。
    2. `--manifest-path` で手元の `Cargo.toml` を使い、`--pkg-url` でその URL に差し替える。

### 文書

- README（英日）の「Install」に、`cargo binstall` と `cargo install` を足す。
  - 「入るのはバイナリだけで、設定ファイルとページは自分で用意する」ことを、コマンドのすぐ下に書く。
  - 既定の設定とページが入った状態で試したい人には、パッケージマネージャかコンテナを勧める。
- `docs/guide/running.md`（英日）の「Configuration file」の節（F-247）に、最小の設定とページを書く。
  - 既定のパスへの置き方を書く。
  - アーカイブの `examples/` を写す方法も書く。
- `docs/guide/building.md`（英日）に、feature のセットと前提のツールを書く。

## 受け入れ条件

- Linux（gnu・musl のどちらが選ばれても）・macOS（arm64・x86_64）・Windows（x64・arm64）で、`cargo binstall veil-proxy` がビルド済みのバイナリを入れる。
  - 署名の検証が通る。
  - 設定が無いときは F-247 のエラーで止まる。
  - 文書の手順で用意した設定で `http://127.0.0.1:8080/` を返す。
- 同じ OS で、`cargo install veil-proxy --locked --features <セット>` がビルドでき、同じ手順で起動する。
- 文書に「設定ファイルとページは自分で用意する」ことと、その手順が書いてある。
- BSD は、`cargo install --path` でビルドできるか、できない組み合わせが文書とエラーで分かる。

## 依存・リスク

- F-247（成果物・設定ファイルの決まり方・署名）が先。
- `cargo install` の full のビルドは、CI で 30 分〜1 時間かかる。公開の後の試験は並列にし、公開そのものは待たせない。

## 関連

- [F-238](F-238-publish-crates-and-pypi.md)
- [F-243](F-243-library-without-cargo-config-env.md)
- [F-247](F-247-distribution-foundation.md)
