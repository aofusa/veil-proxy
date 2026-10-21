# F-243: `.cargo/config.toml` の `[env]` 無しで、Cargo.toml に書くだけで veil-proxy を使えるようにする

**状態: 設計（feat/v083-release、2026-10-21。同日に方式を「同梱の BoringSSL」から「aws-lc-sys の Rust のバインディング経由」に
改めた。v0.8.3 で公開）**

## 目的

利用者が `Cargo.toml` に `veil-proxy` を書くだけで、Linux の `http3`（`full` に含まれる）を含めてビルド・実行できるようにする。
`cargo install veil-proxy` も同じにする。

- **主目標:** `.cargo/config.toml` の `[env]`（`AWS_LC_SYS_NO_PREFIX_<triple>`）を書かなくて済むこと。
- **制約:** quiche は今と同じ aws-lc（rustls と共有の 1 つの aws-lc-sys）を使い続ける。次のものに影響を出さない。
  - 既存のプロキシのバイナリ（配布物・イメージ・性能）
  - API ライブラリの利用（Rust・Python・C）
  - `[env]` を既に書いている利用者
- BoringSSL は使わない（2026-10-21 の指示: 主目標は「Cargo.toml だけ」で、BoringSSL は必須ではない）。

## 今の状態（2026-10-21 の main = v0.8.2）

- Linux の quiche（`veil-proxy-quiche`、`default-features = false`）は、BoringSSL の API（`SSL_*`・`EVP_*` など）を
  **自分の `extern "C"` の宣言**で呼ぶ。TLS の C ライブラリは自分ではリンクしない。
  - そのシンボルは、rustls（aws-lc-rs）が引く aws-lc-sys が満たす。名前が一致する必要があるので、aws-lc-sys を
    **シンボルの接頭辞無し**でビルドしている（`AWS_LC_SYS_NO_PREFIX_<triple>=1`、B-47）。
- aws-lc-sys の既定は接頭辞付き（`aws_lc_0_46_0_EVP_AEAD_CTX_init` のような名前）である。実装の段階 0 で分かったが、
  接頭辞が付くのは libcrypto だけで、libssl の `SSL_*` はどちらでも接頭辞無しである。接頭辞無しにするには環境変数しか無い。
  cargo は依存の `.cargo/config.toml` を読まないので、利用者が自分の `[env]` に書く必要がある。
  - 書かないと、veil の build.rs（`check_aws_lc_no_prefix`）が設定の例を示して止まる（F-238）。
- **aws-lc-sys の Rust のバインディングは、接頭辞を隠している。**
  - aws-lc-sys は bindgen の `ParseCallbacks`（`builder/sys_bindgen.rs` の `StripPrefixCallback`）で、関数名から接頭辞を外す。
    そのうえで `#[link_name = "aws_lc_0_46_0_EVP_AEAD_CTX_init"]` のように付けて出す。
  - そのため、Rust から `aws_lc_sys::EVP_AEAD_CTX_init` と書けば、接頭辞の有無に関係なく正しいシンボルにリンクされる。
    aws-lc-rs が `[env]` 無しで動くのも、この仕組みによる。
  - 接頭辞無しのビルドでは `link_name` が付かない。今の veil のビルド（`target/debug/build/aws-lc-sys-*/out/bindings.rs`）では 0 件だった。
- veil は Linux で既に `aws-lc-sys = { version = ">=0.45, <0.48", features = ["ssl", "bindgen"] }` に依存している。
  - `ssl` は libssl と、その API の Rust のバインディング。
  - 目的は版を rustls と揃えること（F-238）。
  - つまり、libssl の Rust のバインディングは、今のビルドで既に作られている。
- quiche の BoringSSL の経路（Linux で使う部分）が宣言している関数は 86 個ある。
  - 内訳: `tls/mod.rs` 48、`tls/boringssl.rs` 23、`crypto/boringssl.rs` 11、`crypto/mod.rs` 3、`rand.rs` 1。
  - 今の aws-lc-sys の `bindings.rs` と突き合わせると、84 個は同じ名前である。
  - 残りの `sk_num`・`sk_value` は、aws-lc のバインディングでは `OPENSSL_sk_num`・`OPENSSL_sk_value` という名前である。

## 設計（採る方式）: quiche の FFI を aws-lc-sys の Rust のバインディングへ向ける

### `veil-proxy-quiche`（`third_party/quiche`）

- veil 専用の feature `aws-lc-sys` を足す。中身は `["dep:aws-lc-sys"]`。
  - 依存は `aws-lc-sys = { version = ">=0.45, <0.48", features = ["ssl", "bindgen"], optional = true }`。
    veil の依存と同じ範囲・同じ feature にする。
  - aws-lc-sys には `links = "aws_lc_<版>"` があるので、cargo は必ず 1 つの版に解決する。2 つにはならない。
- `src/tls/aws_lc.rs` を足す。86 個の関数を、quiche の今の宣言と同じ名前・同じシグネチャの `#[inline(always)] unsafe fn` で定義する。
  - 各関数は `aws_lc_sys::<名前>` を呼ぶだけにする。

    ```rust
    #[inline(always)]
    pub(crate) unsafe fn SSL_new(ctx: *mut SSL_CTX) -> *mut SSL {
        aws_lc_sys::SSL_new(ctx.cast()).cast()
    }
    ```

  - quiche の不透明な型（`SSL`・`SSL_CTX`・`EVP_AEAD` 等）は、ポインタの `cast()` で aws-lc-sys の型と相互に変換する。
    どちらも C の同じ構造体へのポインタである。
  - コールバック（`SSL_CTX_set_*_callback`・`SSL_QUIC_METHOD` の関数表など）は、関数ポインタの型が quiche と aws-lc-sys で違う。
    ABI は同じ（`extern "C"`、引数はポインタと整数）なので、`transmute` で渡す。
    - 置く場所と根拠: `transmute` は aws_lc.rs の中の関数ごとに置く。根拠（C の宣言が同じであること）は `// SAFETY:` に書く。
    - 構造体（`SSL_QUIC_METHOD`）は、aws-lc-sys の型の値をその場で組み立てて渡す。
  - `sk_num`・`sk_value` は `OPENSSL_sk_num`・`OPENSSL_sk_value` を呼ぶ。
- 今の `extern "C" { ... }` の宣言は、次の形にする。

  ```rust
  #[cfg(not(feature = "aws-lc-sys"))]
  extern "C" { /* 今と同じ */ }
  #[cfg(feature = "aws-lc-sys")]
  use crate::tls::aws_lc::*;
  ```

  呼び出し側のコードは変えない。
- `build.rs`: `aws-lc-sys` の feature では何もリンクしない（今の `default-features = false` と同じ）。リンクは aws-lc-sys が行う。
- 版を `0.24.9-veil.2` に上げる。`README.veil.md` に差分の節を足す。
  - 書くこと: feature・aws_lc.rs・`transmute` の一覧・quiche を上げるときの手順。
  - quiche を上げるときの手順: 宣言が増えたら aws_lc.rs にも足す。足し忘れはコンパイルエラーで分かる。

### veil 側

- Linux の quiche の依存に `features = ["aws-lc-sys"]` を足し、版を `=0.24.9-veil.2` にする。
  - 非 Linux（FreeBSD・macOS・Windows・OpenBSD・NetBSD）は `boringssl-boring-crate` のままにし、版だけ上げる。
- `build.rs` の `check_aws_lc_no_prefix` を消す。
- `.cargo/config.toml` の Linux の `[env]`（`NO_PREFIX=1`）は **残す**。
  - 残すと、veil のリポジトリでのビルドは今と同じ接頭辞無しのシンボル名になる。
  - 理由は 2 つ。配布物・イメージ・perf のバイナリの差を最小にするため。もう 1 つは、`tests/test_backends`・`tests/grpc_server` の
    子の設定との整合のため（B-47）。
  - 接頭辞の有無はリンクの名前が違うだけで、機械語は同じである。消しても動作は変わらない見込みだが、0.8.3 では変えない。
- `[env]` を書いた利用者も、書かない利用者も、同じコードでビルドできる。
  - 書いた場合: 接頭辞無しのバインディング。今と同じ。
  - 書かない場合: 接頭辞付きのバインディング。

### 影響が無いことの根拠

| 対象 | 根拠 |
|---|---|
| 暗号の実装 | quiche が呼ぶ C の関数は、今と同じ aws-lc の同じ関数（名前の接頭辞の有無だけが違い得る） |
| 性能（ホットパス） | 包みの関数は `#[inline(always)]` で、呼び出し先は同じ `extern "C"` の関数。AEAD の暗号化・ヘッダ保護の呼び出しも直接の呼び出しのまま |
| ビルドの要件 | aws-lc-sys の構成（`ssl`・`bindgen`）は今と同じ。cmake・libclang などの要件は増えない。BoringSSL のソースも入れない |
| 依存の数 | 増えない（aws-lc-sys は既に veil の依存） |
| 非 Linux | quiche の feature は変えない（`boringssl-boring-crate`） |
| API ライブラリ（Python・C） | wheel と C の zip は veil のリポジトリの中でビルドする（`[env]` が効く）ので今と同じ。PyPI の sdist から入れる場合は、`[env]` が無くてもビルドできるようになる |
| `[env]` を書いた既存の利用者 | 今と同じ構成になる（接頭辞無し） |

### 採らない案

| 案 | 採らない理由 |
|---|---|
| `[env]` が無いとき quiche に同梱の BoringSSL をビルドする（この日の最初の設計） | 暗号ライブラリが 2 つになる。cmake・C++ が要り、ビルドが長く、バイナリが大きく、リポジトリが 20MB 増える。主目標には要らない |
| quiche の `extern` の宣言に `#[link_name]` で接頭辞を書く（build.rs が aws-lc-sys の版から接頭辞を作る） | 接頭辞に aws-lc-sys の版が入るので、build.rs で版を推測する。aws-lc-sys が名前の付け方を変えると黙って壊れる。バインディング経由ならその仕組みを aws-lc-sys に任せられる |
| リンカの `--defsym` で接頭辞無しの別名を作る | 依存のクレートの `cargo:rustc-link-arg` は最終のバイナリのリンクに渡らない（そのクレート自身のターゲットにしか効かない） |
| Linux でも `boring` crate を使う | 暗号ライブラリが 2 つになる。`[env]` を書いた構成では重複シンボルになる（B-47・F-136）。AGENTS.md「Linux に boring を依存させない」 |
| veil の build.rs で値を見て止める（今） | 主目標に反する |

## 段階 0: 方式の確認（実装の最初に行う。だめなら方式を見直す）

1. `third_party/quiche` に `aws-lc-sys` の feature と `tls/aws_lc.rs` を入れる。`cargo build --features full` が通ることを確かめる。
   - 見ること: 86 個すべてが aws-lc-sys のバインディングにあるか。シグネチャが違うものの一覧（`const`/`mut`・`usize`/`c_int`・
     コールバックの型）を記録する。
2. veil のリポジトリでの今のビルド（`[env]` あり）で、性能に関わるところが変わらないことを確かめる。
   - `--profile dist` のバイナリで、quiche の暗号化の経路の機械語が v0.8.2 と同じこと（`objdump` の比較）。
     対象: `Connection::send` の AEAD の呼び出し、`crypto::Seal::seal_with_u64_counter`。
   - 呼び出し先のシンボル名が同じこと（`nm`）。
3. リポジトリの外の新しいプロジェクトで確かめる。
   - 作り方: `docs/artifacts/f243/` の下に作る。`.cargo/config.toml` は無し。`cargo config get env` で veil の設定が見えないことを確かめる。
     依存は `veil-proxy` を path で、`full`・`api`。
   - `cargo build --release` が通る。
   - `nm` で、quiche の呼ぶ libcrypto の関数が接頭辞付きのシンボル（`aws_lc_<版>_EVP_AEAD_CTX_seal_scatter`）にリンクされ、
     接頭辞無しの同名が無い（libssl の `SSL_*` は接頭辞無しのまま）。
   - `cargo tree -i boring` が空。
4. 同じプロジェクトに `[env]` を足した場合も、通って接頭辞無しになることを確かめる。

## 試験

- 既存の試験は、`[env]` ありの今の構成で今と同じに通ること。
  - 単体・統合
  - E2E の両バックエンド（`./tests/e2e_setup.sh test`、`VEIL_E2E_FEATURES="full,epoll"`）
  - `tests/api/library_e2e.sh all`・`nonregression`
  - HTTP/3 の h2load（quiche の送出を変えたら h2load でも確かめる。AGENTS.md）
- `tests/api/library_e2e.sh` に `external-env` と `external-noenv` の 2 つの場合を足す。
  - 段階 0 の外部のプロジェクトを使い、HTTP/1.1・h2c・**HTTP/3** で 200 を確かめる。
  - 起動したバイナリについて、`nm` で接頭辞の有無を確かめる。
- リポジトリの外で `[env]` 無しにビルドした veil のバイナリで、E2E を回す。
  - ビルドは `cargo install --path <リポジトリ> --root <外の場所> --features full` を、リポジトリの外の作業ディレクトリから実行する。
    cargo は作業ディレクトリから設定を探すので、veil の `.cargo/config.toml` は読まれない。
  - E2E は `VEIL_BIN_OVERRIDE=<外のバイナリ> VEIL_E2E_SKIP_VEIL_BUILD=1 ./tests/e2e_setup.sh test` で回す。seccomp を有効にした HTTP/3 も含む。
- 公開物の形でも確かめる。
  - `tools/release/publish-crates.sh --dry-run` が作った `.crate` を展開し、path 依存にした `[env]` 無しの外部のプロジェクトで、
    ビルドと HTTP/3 の疎通を確かめる。
  - `release.yml` の crates の dry-run のジョブ（rc のタグ）に、この確認を足す。
- musl（`x86_64-unknown-linux-musl`）と aarch64 でも、`[env]` 無しのビルドを確かめる。
  - 手段は Docker のクロスビルド。aarch64 は `tools/qemu/` の VM で HTTP/3 の疎通まで確かめる。
- 性能: 段階 0 の 2 で機械語が同じなら、A/B は要らない。違いが出たら、HTTP/3 の 3B・54KB の交互 A/B
  （`tools/perf`、v0.8.2 と比較）で確かめる。

## 文書

- `src/lib.rs` の crate の doc の「HTTP/3 on Linux」の節を消す（設定は要らない）。
  - 注意として 1 文だけ残す: libclang（bindgen）が要る。今と同じ要件。
  - 文は F-241 に従い英語で、チケット番号を書かない。
- `docs/guide/library.md`（英日）・`crates/README.crates-io.md`・`docs/guide/building.md`（英日）から `[env]` の手順を消す。
  - 代わりに書くこと: 「書いてあっても害は無い（シンボル名が接頭辞無しになるだけ）」。
- `AGENTS.md`「TLS・暗号」の quiche の行に追記する。
  - 内容: Linux の quiche は aws-lc-sys の Rust のバインディング経由で aws-lc を呼ぶ（`veil-proxy-quiche` の `aws-lc-sys` の feature、
    `tls/aws_lc.rs`）。`[env]` の `NO_PREFIX` は veil のビルドのシンボル名を揃えるために残しているだけで、利用者には要らない。
- `third_party/quiche/README.veil.md`: 差分の節を足す。

## 受け入れ条件

- `.cargo/config.toml` の無い外部のプロジェクトで、`veil-proxy`（`full`・`api`）がビルドでき、HTTP/3 に応答する。
  - 対象: Linux の x86_64・aarch64・musl。
  - quiche と rustls は 1 つの aws-lc-sys を共有する（`cargo tree` で aws-lc-sys が 1 つ、`boring` が無い）。
- `[env]` を書いた外部のプロジェクトも、今と同じ構成でビルド・応答する。
- veil のリポジトリでのビルドで、quiche の暗号の経路の機械語と、シンボルの名前が v0.8.2 と同じ。
- 既存の試験がすべて通る。
- `veil-proxy-quiche 0.24.9-veil.2` を展開したものから、`[env]` 無しでビルドできる。
- 0.8.3 の公開の後、crates.io から入れた新しいプロジェクトで同じことを確かめる（F-245）。

## 関連

- [F-238](F-238-publish-crates-and-pypi.md)（今の build.rs の検査を入れた）
- [B-47](../bugs/B-47-awslc-no-prefix-not-applied-cross-build.md)（`[env]` の背景）
- [F-136](F-136-bsd-tls-cert-hot-reload-capability-mode.md)（非 Linux の BoringSSL）
- [F-172](F-172-quiche-stream-frame-coalescing.md)（`third_party/quiche` の vendoring）
- [F-241](F-241-docsrs-english-only.md)・[F-245](F-245-version-0-8-3.md)
