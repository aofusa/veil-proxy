# F-238: Rust のライブラリと Python のモジュールの公開（タグの push で crates.io・PyPI へ自動公開）

**状態: 完了（feat/v080-release、2026-10-16。公開そのもの〈タグの push と最初の 1 回の人の準備〉は main へのマージの後）**

## 目的

git のタグ `vX.Y.Z` を push したら、CI が次の 2 つを自動で公開・更新する。公開する版はタグの版にする。

- Rust の API ライブラリ（`veil::api`、マクロ、gRPC のコード生成）を crates.io へ。
- Python のモジュール（`import veil`）を PyPI へ。

今の利用者は `git = "https://github.com/aofusa/veil-proxy"` で依存している。

## 名前（2026-10-16 に各レジストリで確かめた）

| 今のパッケージ | 状態 | 公開する名前 |
|---|---|---|
| `veil`（ルート） | crates.io の `veil` は別のプロジェクト（primait/veil、Debug の秘匿の derive マクロ、0.3.0）が使っている | **`veil-proxy`**（空き）。`[lib] name = "veil"` はそのままなので、`use veil::...` と `#[veil::route]` は変わらない |
| `veil-macros` | crates.io の `veil-macros` も primait/veil が使っている | **`veil-proxy-macros`**。`[lib]` の名前は `veil_macros` のまま |
| `veil-build` | 空き | **`veil-proxy-build`**（揃える）。`[lib]` の名前は `veil_build` のまま |
| `veil-capi` | `publish = false` | 公開しない（C のライブラリは packaging の配布物で配る） |
| `veil-python`（PyPI は `veil-api`） | PyPI の `veil-api` は空き。`veil` は別のプロジェクトが使っている | **`veil-api`**（今の pyproject の名前のまま）。`import veil` は変わらない |

- ルートのパッケージ名を変えると、次の参照も変わる。
  - `cargo install veil-proxy` で入るバイナリは `veil` のままである（`[[bin]] name = "veil"`）。
  - path 依存で `veil = { path = .. }` と書いている所（`examples/api/*`・`tools/*`・`tests/*`・crates の中）は、
    `veil = { package = "veil-proxy", path = .. }` に書き換える。こうすると、コードの `veil::` も依存のキーも変わらない。
- マクロが生成するコードは `::veil::` のパスを使う。利用者が依存のキーを変えると壊れるので、利用者には
  `veil = { package = "veil-proxy", version = "0.8" }` と書いてもらう。F-239 の文書の最初の例をこの形にする。
- 名前は 0.8.0 で一度に変える（F-231 と同じリリース）。

## 公開を妨げるもの（段階 0 で `cargo publish --dry-run`・`cargo package --list` で全部洗い出す）

1. **vendoring した wasmtime の path 依存。**
   - 状況: NetBSD・FreeBSD/OpenBSD aarch64 の `veil-wasmtime`（`third_party/wasmtime`）には版が無い。crates.io は、
     target 別の依存でも、全部が crates.io にあることを求める。
   - 対策: `third_party/wasmtime` を `veil-proxy-wasmtime` として公開する。
     - 中身は wasmtime 36.0.18 の `wasmtime` クレートだけ。下位の `wasmtime-*` は crates.io の版を使う。
     - ライセンスは Apache-2.0 WITH LLVM-exception。NOTICE と、veil の差分の説明（README.veil.md）を含める。
2. **quiche の `[patch.crates-io]`。**
   - 状況: patch は依存する側のビルドには効かない。crates.io から入れた利用者は、STREAM フレームを詰め合わせる差分（F-172）の
     無い quiche を使うことになり、HTTP/3 の性能が変わる。
   - 対策: `third_party/quiche` を `veil-proxy-quiche`（版は `0.24.9-veil.1`）として公開する。依存は
     `quiche = { package = "veil-proxy-quiche", version = "=0.24.9-veil.1" }` にする（コードの `quiche::` は変わらない）。
     - ライセンスは BSD-2-Clause。
     - `[patch.crates-io]` から quiche を外す。テスト用の quinn-udp は dev 依存なので、そのまま残す。
3. **`.cargo/config.toml` の `[env]`（`AWS_LC_SYS_NO_PREFIX_<triple>`。B-47）。**
   - 状況: 依存する側のビルドでは読まれない。今の git の利用者も同じで、リポジトリの外のプロジェクトで Linux の
     `http3` を有効にすると、aws-lc-sys がシンボルにプレフィックスを付けたままになり、quiche のリンクが失敗するはずである
     （段階 0 でリポジトリの外のプロジェクトを作って確かめる）。
   - 0.8.0 での対策:
     - veil の `build.rs` が、Linux かつ `http3` のときにこの環境変数を見る。値が違えば、利用者がすべき設定
       （`.cargo/config.toml` の `[env]` の 1 行）を示して、ビルドを止める（`panic!`。リンクの失敗より先に、分かる形で止める）。
     - F-239 の文書の最初に、この設定を書く。
   - 先の案（別チケット）: `veil-proxy-quiche` の FFI の宣言を、aws-lc-sys のプレフィックス付きのシンボルに向ける。そうすると
     利用者の設定が要らなくなる。差分が大きいので 0.8.0 には入れない。
4. **パッケージの大きさ（crates.io の上限は 10MB）。**
   - 対策: `include` で `src/`・`build.rs`・`Cargo.toml`・`README`・`LICENSE*` だけを入れる。
     `tests/`・`docs/`・`tools/`・`fuzz/`・`benches/`・`third_party/` は入れない。
   - `build.rs` がリポジトリの中のほかのファイルを読んでいないかも確かめる。
5. **tokio の差し替え（`crates/veil-tokio/tokio-shim`、パッケージ名 `tokio`・版 1.99.0）。**
   - 状況: この名前では crates.io に公開できない。
   - 対策の案: `veil-proxy-tokio-shim` として公開する。利用者は `[patch.crates-io] tokio = { package = "veil-proxy-tokio-shim",
     version = "1.99.0" }` と書く。
   - ただし、crates.io の依存を crates.io の別名のパッケージで patch できるかは確かでない。段階 0 で、リポジトリの外の
     プロジェクトで確かめる。できなければ、tokio-shim は今の git の patch のままにして、文書に書く。

## Python の wheel

- ビルドは `PyO3/maturin-action` で行い、abi3-py39（今の設定）なので、プラットフォームごとに 1 つの wheel になる。
  - Linux: `manylinux_2_28` と `musllinux_1_2` の x86_64・aarch64。aarch64 はネイティブの arm のランナーで作る。
  - macOS: arm64・x86_64（`full-macos`）。
  - Windows: x86_64（`full-windows`）。
  - BSD: PyPI に BSD の wheel の型は無いので、sdist から作る。
- macOS と Windows は B-125 で Python 版がビルドできるようになっている。CI で wheel を作った後、そのランナーで
  `crates/veil-python/tests/e2e.sh` を回してから公開する。
- sdist も公開する。maturin の sdist は、ワークスペースの path 依存を含める。大きさ（PyPI の上限）と、展開した sdist から
  ビルドできることを、段階 0 で確かめる。

## 公開の手順（`.github/workflows/release.yml`、F-237 と共有）

1. `verify`（F-237 と同じ）: タグと版の一致。タグのコミットが main に含まれていること。
2. `crates`: 依存の順に公開する。
   - 順: `veil-proxy-quiche` → `veil-proxy-wasmtime` → `veil-proxy-macros` → `veil-proxy` → `veil-proxy-build`。
   - 前のクレートが索引に載るのを待ってから、次を公開する。
   - quiche と wasmtime は、版が変わったときだけ公開する（既に同じ版があれば飛ばす）。
   - 失敗したときに再実行できるよう、各クレートで「その版が既にあれば飛ばす」ようにする（冪等）。
3. `wheels` の行列: ビルドし、そのランナーで e2e を回す。
4. `pypi`: wheel と sdist をまとめて公開する。
- 鍵: どちらも trusted publishing（GitHub の OIDC）を使い、長く使える API の鍵を secret に置かない。
  - crates.io は `rust-lang/crates-io-auth-action`。
  - PyPI は `pypa/gh-action-pypi-publish`（environment `pypi`）。
- 人が 1 回だけ行うこと（CI ではできない）:
  - crates.io の trusted publishing は、既にあるクレートにしか設定できない。新しい 5 つの名前は、最初の版（0.8.0）だけ
    人が API の鍵で `cargo publish` する。あわせて、各クレートの設定で trusted publishing（リポジトリ・workflow・
    environment）を登録する。
  - PyPI は、「pending publisher」として、プロジェクトの作成前に登録できる。
  - これらの手順を `packaging/README.md` に書く。

## 受け入れ条件

- 段階 0 の洗い出しの結果と、各項目の決着を、このチケットに書く。
- リポジトリの外の新しいプロジェクトで、次が動く。
  - `veil = { package = "veil-proxy", version = "0.8", features = ["full", "api"] }` で `examples/api/rust-hello` 相当がビルド・
    起動できる（Linux の `http3` を含む。項目 3 の設定は文書の通り）。
  - `pip install veil-api` で、Linux・macOS・Windows の FastAPI 風のサンプルが動く。
- タグ `v0.8.0-rc.1` で、workflow を最後まで通す。crates.io への rc の公開は取り消せないので、crates の段だけは
  `cargo publish --dry-run` にする。PyPI は TestPyPI へ送る。そのあと `v0.8.0` で本公開する。
- `docs/guide/library.md`（英日）・`crates/*/README.md` の依存の書き方を、crates.io・PyPI の形に更新する（git の形は残す）。
- 既存のプロキシの性能が退行しない。quiche を patch からパッケージの別名に替えても中身は同じだが、HTTP/3 の行を
  `suite_ab.sh` で確かめる。


## 実装の結果

### 段階 0 の洗い出しと決着

| 項目 | 結果 |
|---|---|
| 1. vendoring の wasmtime | `veil-proxy-wasmtime`（36.0.18）として公開する。`Cargo.toml.orig`（cargo の予約名）を除いた |
| 2. quiche の `[patch]` | `veil-proxy-quiche`（`=0.24.9-veil.1`）を各ターゲットの依存に直接書く。patch は外した |
| 3. `AWS_LC_SYS_NO_PREFIX` | 確かめた: リポジトリの外のプロジェクトで設定が無いと、veil の build.rs が設定を示して止まる。設定すると通る |
| （3 の調査で見つかった別の問題） | 利用者の解決では aws-lc-rs の最新が aws-lc-sys 0.46 を引き、veil の 0.45 と 2 つになって libcrypto が見つからずに失敗した。`aws-lc-sys = ">=0.45, <0.48"` にして 1 つに寄せた |
| 4. パッケージの大きさ | `include` で src・build.rs・README・LICENSE だけ。veil-proxy は 1.6MB。`src/` の `.rs` 以外（`include_str!` が読むもの）も入れる |
| 5. tokio-shim | crates.io の依存を crates.io の別名で patch することはできない（cargo の制約）。git の patch のまま、veil も同じタグから取るように文書に書いた |

- `cargo package --workspace`（vendoring の 2 つをメンバーに入れた作業用のコピー）で 5 つとも検証のビルドまで通った。
- リポジトリの外の新しいプロジェクトで、展開したパッケージを `[patch.crates-io]` で当てて `features = ["full", "api"]`（Linux の
  http3 を含む）がビルドでき、`examples/api/rust-hello` 相当が HTTP/1.1・h2c で応答した。
- `tools/release/publish-crates.sh`（`--dry-run` は `cargo package --workspace`）、release.yml の crates・wheels（7 種）・sdist・PyPI。
- 文書: README（英日）・library.md（英日）・`crates/README.crates-io.md`・`crates/veil-build/README.md`・`crates/veil-tokio/README.md`・
  `packaging/README.md`（最初の 1 回の人の準備）。

## 関連

F-231（版）、F-237（同じ workflow）、F-239（文書）、F-236（公開の前の監査）、F-172（quiche の差分）、B-47・B-55・B-125、
`crates/`、`third_party/`。
