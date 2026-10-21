# F-243: `.cargo/config.toml` の `[env]` 無しで、Cargo.toml に書くだけで veil-proxy を使えるようにする

**状態: 設計（feat/v083-release、2026-10-21。v0.8.3 で公開）**

## 目的

利用者が `Cargo.toml` に `veil-proxy` を書くだけで、Linux の `http3`（`full` に含まれる）を含めてビルド・実行できるようにする。
`cargo install veil-proxy` も同じにする。

| 利用者の `.cargo/config.toml` | quiche の TLS（BoringSSL の API） | rustls の暗号 |
|---|---|---|
| `AWS_LC_SYS_NO_PREFIX_<triple>` が無い（既定） | **quiche に同梱の BoringSSL** をビルドしてリンクする | aws-lc-rs（プレフィックス付きの aws-lc-sys） |
| `AWS_LC_SYS_NO_PREFIX_<triple> = "1"` がある | rustls と **同じ aws-lc-sys** を使う（今と同じ） | aws-lc-rs（プレフィックス無しの aws-lc-sys） |

veil のリポジトリでのビルド（配布物・イメージ・CI・E2E・perf）は、`.cargo/config.toml` の `[env]` があるので下の行になる。
つまり、今とまったく同じバイナリになる。

## 今の状態（2026-10-21 の main = v0.8.2）

- Linux の quiche（`veil-proxy-quiche`）は `default-features = false` で、TLS の C ライブラリを自分ではリンクしない。
  - quiche の Rust のコードが呼ぶ `SSL_*` などのシンボルは、rustls（aws-lc-rs）が引く aws-lc-sys をプレフィックス無しで
    ビルドしたものが満たす（B-47）。
  - プレフィックス無しにするには、aws-lc-sys のビルドスクリプトに `AWS_LC_SYS_NO_PREFIX_<triple>=1` を渡す必要がある。
    渡せるのは環境変数（`.cargo/config.toml` の `[env]`）だけで、veil の build.rs からは設定できない
    （依存のビルドスクリプトは先に別プロセスで走る）。
- cargo は依存のクレートの `.cargo/config.toml` を読まない。そのため crates.io・git・path から使う利用者は、自分の
  `.cargo/config.toml` に `[env]` を書く必要がある。
  - 無ければ、veil の build.rs（`check_aws_lc_no_prefix`）が設定の例を示して止まる（F-238）。
  - 案内している場所: `src/lib.rs` の crate の doc、`docs/guide/library.md`（英日）、`crates/README.crates-io.md`。
- 非 Linux（FreeBSD・macOS・Windows・OpenBSD・NetBSD）は、quiche の `boringssl-boring-crate`（外部の `boring` crate が
  BoringSSL をビルド）を使う。aws-lc-sys はプレフィックス付きのまま共存している。こちらは `[env]` が無くても動く。
- `third_party/quiche` は crates.io の quiche 0.24.9 を取り込んだものである。ただし `deps/boringssl`（BoringSSL のソース 20MB）
  は取り込んでいない（README.veil.md）。build.rs の `boringssl-vendored` の分岐（cmake で BoringSSL をビルドする）は残っている。

## 設計

### 方式: quiche の build.rs が、aws-lc-sys と同じ環境変数を見て決める

cargo の依存と feature は、環境変数では切り替えられない。そこで、依存の形は 1 つに固定する。そのうえで、
**リンクするものを `veil-proxy-quiche` のビルドスクリプトが決める**。

- aws-lc-sys と quiche のビルドスクリプトには、同じ `[env]` が渡る。
- 判定は aws-lc-sys と同じ順・同じ解釈にする。
  - 変数: `AWS_LC_SYS_NO_PREFIX_<triple>` を先に見て、無ければ `AWS_LC_SYS_NO_PREFIX`。
  - 解釈は `parse_to_bool` と同じ: 先頭が `0`/`n`/`off`/`f` なら偽、数字/`y`/`on`/`t` なら真。
- こうすれば、「aws-lc-sys はプレフィックス無しなのに quiche も BoringSSL をリンクする」（重複シンボル）と、
  「どちらもリンクしない」（未定義シンボル）の食い違いは起きない。

`third_party/quiche/src/build.rs` の Linux の分岐は次のとおり（`boringssl-boring-crate`・`openssl` が無いとき）。

```text
DOCS_RS がある                → 何もしない（rustdoc はリンクしない。BoringSSL のビルドで docs.rs の時間を使わない）
NO_PREFIX が真                → 何もリンクしない（今と同じ。シンボルはプレフィックス無しの aws-lc-sys が満たす）
NO_PREFIX が偽・無い          → deps/boringssl を cmake でビルドし、libssl.a・libcrypto.a を static でリンクする
                                 （upstream の boringssl-vendored と同じ処理。QUICHE_BSSL_PATH があればそれを使う）
```

- quiche の build.rs は、2 つの変数と `DOCS_RS` に `cargo:rerun-if-env-changed` を出す。`[env]` を後から足したり消したりしたときに、
  aws-lc-sys と一緒にビルドし直させるためである。
- `boringssl-vendored` の feature を立てる案は、採らない。feature は利用者の環境変数と連動できないためである。
  今の `cfg!(feature = "boringssl-vendored")` の条件に、上の判定を足す。
- どちらを選んだかを `cargo:rustc-cfg` で quiche の中に残す（`quiche_bssl_vendored`）。さらに `pub const` で外から読めるようにする
  （`quiche::veil_tls_backend() -> &'static str` の 1 関数を足す）。
  - veil は起動時に、HTTP/3 のリスナーがあるときだけ `info!("HTTP/3: TLS backend: {}", ..)` を 1 行出す。
    値は `"BoringSSL (bundled with quiche)"` か `"aws-lc (shared with rustls)"`。
  - 試験と、利用者の問い合わせのときの切り分けに使う。
  - ホットパスではない（起動時の 1 回だけ）。

### 2 つの暗号ライブラリの共存（`[env]` が無いとき）

- aws-lc-sys はプレフィックス付き（`aws_lc_0_xx_y_SSL_new` のような名前）、quiche の BoringSSL はプレフィックス無しになる。
  名前が衝突しないので、同じバイナリに同居できる。
- これは FreeBSD・macOS・Windows で既に使っている形と同じである（F-136。BoringSSL が `boring` crate を通すか、quiche の
  build.rs から直接かの違いだけ）。
- **AGENTS.md の「Linux に `boring` を依存させない」は守る。** `boring` crate は使わず、quiche が自分で BoringSSL をリンクする。
  `[env]` があるときは BoringSSL をビルドもリンクもしないので、B-47 の重複シンボルは起きない。
- veil の Rust のコードは変えない。
  - Linux の HTTP/3 のコード（memfd 経由の証明書の読み込み・ホットリロード）は quiche の `Config` の API だけを使う。
    `boringssl-boring-crate` 専用の `with_boring_ssl_ctx_builder` は使っていないので、どちらの方式でも同じに動く。
  - `src/` には aws-lc-sys を直接呼ぶ箇所が無い（`grep aws_lc_sys` で 0 件）。HTTP/3 の乱数は `aws_lc_rs::rand` で、
    プレフィックスの有無に関係しない。

### `third_party/quiche` に BoringSSL のソースを入れる

- crates.io の quiche 0.24.9 の `deps/boringssl` を、そのまま `third_party/quiche/deps/boringssl` に入れる。
  - quiche 0.24.9 が想定している BoringSSL の版なので、組み合わせの検証が済んでいる。
  - 生成済みのアセンブリ（`linux-x86_64` 等）が含まれるので、Go は要らない。cmake と C/C++ のコンパイラで足りる。
- 大きさ: ソースは 20MB、upstream の quiche の `.crate` は圧縮後 3.8MB。crates.io の上限（10MB）に収まる。
  リポジトリは 20MB 大きくなるが、ほかに置き場所が無い（理由は下の「採らない案」）。
- `Cargo.toml` の `include` に `deps/boringssl` を足す。版を `0.24.9-veil.2` に上げる。`README.veil.md` に差分として書く。
- ライセンス: BoringSSL の `LICENSE` を含め、`COPYING` と並べて README.veil.md に明記する。

### 採らない案

| 案 | 採らない理由 |
|---|---|
| Linux でも `boring` crate を常に依存にする | `[env]` があるとき（veil 自身のビルド）、プレフィックス無しの aws-lc と BoringSSL のシンボルが重複してリンクが壊れる（F-136 の実験・B-47）。依存は環境変数で外せない |
| `[env]` が無いときも aws-lc-sys を共有する（quiche の FFI の宣言をプレフィックス付きの名前へ向ける） | 依頼は「`[env]` が無ければ quiche の既定の BoringSSL」。さらに、プレフィックスは aws-lc-sys の版を含むので、vendoring の差分が版ごとに変わり大きくなる |
| BoringSSL のソースをビルドのときに取ってくる | docs.rs・オフラインのビルド・社内のミラーで失敗する |
| veil の build.rs で値を見て止める（今） | 依頼に反する |

### veil 側の変更

- `build.rs` の `check_aws_lc_no_prefix` を消す（panic しない）。
- `Cargo.toml`: `quiche` の版を `=0.24.9-veil.2` にする。target 別の 4 か所すべてで変える（非 Linux はリンクが変わらない）。
- `.cargo/config.toml` の `[env]` は残す。veil のビルドは今と同じ（共有）方式のままにする。
- `tools/release/publish-crates.sh`: 版が変わったので `veil-proxy-quiche` が先に公開される（今の「版が変わったときだけ公開」の
  仕組みのまま）。`cargo package` で `.crate` が 10MB 未満であることを確かめ、超えたら止める行を足す。

### 文書

- `src/lib.rs` の crate の doc: 「HTTP/3 on Linux」の節を書き直す。
  - 何も書かなくてよいことを書く。
  - 既定では quiche に同梱の BoringSSL をビルドするので、**cmake と C++ のコンパイラが要る**ことを書く。
  - `[env]` を書くと aws-lc-sys を共有することと、その利点を書く（暗号ライブラリが 1 つになり、ビルドが短く、バイナリが小さい）。
  - 文は F-241 に従い英語で、チケット番号を書かない。
- `docs/guide/library.md`（英日）・`crates/README.crates-io.md`・`docs/guide/building.md`（英日）: 同じ内容にする。
- `AGENTS.md`「TLS・暗号」の quiche の行に追記する: Linux の quiche は `AWS_LC_SYS_NO_PREFIX` の値で、aws-lc-sys の共有
  （veil のビルド）と同梱の BoringSSL（`[env]` の無い利用者）を quiche の build.rs が選ぶ。
- `third_party/quiche/README.veil.md`: 差分の節を足す（build.rs の判定・`deps/boringssl`・`veil_tls_backend`）。

## 試験

### 段階 0: 方式が成り立つかの確認（実装の最初に行う）

- 次の 2 つの外部のプロジェクトを作る。
  - 場所はリポジトリの外（`docs/artifacts/f243/` の下に作り、veil の `.cargo/config.toml` が見えないことを
    `cargo config get env` で確かめる）。
  - 依存は `veil = { package = "veil-proxy", path = "<リポジトリ>", default-features = false, features = ["full", "api"] }`。
  1. `[env]` 無し
  2. `[env]` あり（`AWS_LC_SYS_NO_PREFIX_x86_64_unknown_linux_gnu = "1"`）
- 両方で次を確かめる。
  - `cargo build --release` が通る。
  - `nm` で `SSL_new` の出どころを見る: 1 は BoringSSL、2 は aws-lc。
  - aws-lc のプレフィックス付きのシンボルが、1 だけにある。
  - `cargo tree -i boring` が空。
- 1 のビルドの時間と、バイナリの大きさの増分を記録する（チケットに追記する）。

### 自動の試験

- `tests/api/library_e2e.sh` に `external-env` と `external-noenv` の 2 つの場合を足す。
  - 上の外部のプロジェクトで、HTTP/1.1・h2c・**HTTP/3**（quinn のクライアント、または `curl --http3` があればそれ）で 200 を確かめる。
  - 起動ログの `HTTP/3: TLS backend:` の値が期待どおりかを確かめる。
- 公開物の形での確認として、`tools/release/publish-crates.sh --dry-run` が作った `.crate` を展開する。それを path 依存にした
  外部のプロジェクトで、`[env]` 無しのビルドと HTTP/3 の疎通を確かめる。
  - 確かめるのは、`include` に `deps/boringssl` が入っていることである。
  - `release.yml` の crates の dry-run のジョブ（rc のタグ）に、この確認を足す。
- musl（`x86_64-unknown-linux-musl`）と aarch64 で、`[env]` 無しのビルドが通ることを確かめる（Docker のクロスビルド。
  aarch64 は `tools/qemu/` の VM で HTTP/3 の疎通まで）。
- リポジトリの外で `[env]` 無しにビルドした veil のバイナリで、`./tests/e2e_setup.sh test` を回す。
  - ビルドは `cargo install --path <リポジトリ> --root <外の場所> --features full` を、リポジトリの外の作業ディレクトリから実行する。
    cargo は作業ディレクトリから設定を探すので、veil の `.cargo/config.toml` は読まれない。
  - E2E は `VEIL_BIN_OVERRIDE=<外のバイナリ> VEIL_E2E_SKIP_VEIL_BUILD=1` で、そのバイナリを使う。
  - BoringSSL の方式で、seccomp を有効にした HTTP/3 の E2E も通ることを確かめる。BoringSSL と aws-lc で使う syscall が違うかもしれない。
- 既存の試験（単体・統合・E2E の両バックエンド・`library_e2e.sh all`・`nonregression`）は、`[env]` ありの方式で今と同じに通る。

### 性能

- veil のビルドは今と同じ方式なので、性能の A/B は要らない。
- 確かめ方: v0.8.2 と 0.8.3 の `--profile dist` のバイナリで、`.text` の大きさと `SSL_*` の出どころが同じであること。
- `[env]` 無しの方式の HTTP/3 の性能は参考の値として測る（`tools/perf` の h3 の 3B・54KB、共有の方式との交互 A/B）。
  大きく劣るなら、文書の「`[env]` を書く利点」に数字を書く。

## 受け入れ条件

- `.cargo/config.toml` の無い外部のプロジェクトで、`veil-proxy`（`full`・`api`）が Linux の x86_64・aarch64・musl でビルドでき、
  HTTP/3 に応答する。
- `[env]` を書いた外部のプロジェクトは、aws-lc-sys を共有したまま動く（BoringSSL をリンクしない）。
- veil のリポジトリのビルドで、BoringSSL がビルドもリンクもされない（ビルドのログと `nm` で確かめる）。
- `veil-proxy-quiche 0.24.9-veil.2` の `.crate` が 10MB 未満で、展開したものから `[env]` 無しでビルドできる。
- 0.8.3 の公開の後、crates.io から入れた新しいプロジェクトで同じことを確かめる（受け入れの最後の手順）。

## 関連

- [F-238](F-238-publish-crates-and-pypi.md)（今の build.rs の検査を入れた）
- [B-47](../bugs/B-47-awslc-no-prefix-not-applied-cross-build.md)（`[env]` の背景）
- [F-136](F-136-bsd-tls-cert-hot-reload-capability-mode.md)（BoringSSL とプレフィックス付きの aws-lc の共存）
- [F-172](F-172-quiche-stream-frame-coalescing.md)（`third_party/quiche` の vendoring）
- [F-241](F-241-docsrs-english-only.md)・[F-245](F-245-version-0-8-3.md)
