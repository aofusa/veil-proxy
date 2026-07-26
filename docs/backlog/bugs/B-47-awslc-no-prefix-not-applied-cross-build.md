# B-47: `AWS_LC_SYS_NO_PREFIX` がターゲット別に適用されず http3 有効時のクロスビルドが失敗する

## 事象

`--features http3`（および `full`）を有効にしたクロスプラットフォームビルドが、
ターゲットによって不定に失敗する。

- **Linux / FreeBSD**: `quiche` は BoringSSL 互換の**非プレフィックス**シンボルを要求するため
  `AWS_LC_SYS_NO_PREFIX=1` が必要。これが渡らないと `aws-lc-sys` がシンボルを
  プレフィックス付きでビルドし、リンク段で quiche が要求する `SSL_*` / `EVP_*` などが
  未定義となって失敗する。
- **Windows / macOS / OpenBSD**: `quiche` は内蔵 BoringSSL（`boring-sys`）を使うため、
  `aws-lc-sys` 側は**プレフィックスを維持**（`AWS_LC_SYS_NO_PREFIX=0`）しないと
  BoringSSL とシンボルが衝突する。

## 原因

1. **`.cargo/config.toml` の `[target.<triple>.env]` は cargo の設定スキーマに存在しない。**
   cargo の `[target.<triple>]` テーブルが解釈するのは `linker` / `runner` / `rustflags` /
   `ar` などで、`env` サブテーブルは**警告もなく黙って無視される**。
   よって以下は完全に無効だった:

   ```toml
   [target.x86_64-unknown-linux-gnu.env]   # ← cargo は読まない
   AWS_LC_SYS_NO_PREFIX = "1"
   ```

   最小再現: 任意のクレートに上記形式で変数を書き、build.rs から
   `std::env::var(...)` すると `Err(NotPresent)` になる。

2. **`build.rs` の `std::env::set_var("AWS_LC_SYS_NO_PREFIX", ...)` も効かない。**
   依存クレート（`aws-lc-sys`）のビルドスクリプトは、依存元（`veil`）の build.rs より
   **先に別プロセスとして**実行される。後から自プロセスの環境変数を書き換えても伝播しない。

3. 結果として実際に値が渡っていたのは `docker/Dockerfile.{glibc,musl}*` の
   `ENV AWS_LC_SYS_NO_PREFIX=1` だけだった。そのため
   **Docker での Linux ビルドだけがたまたま成功**し、ローカル cargo ビルド・
   QEMU VM 内の FreeBSD ネイティブビルドなど、その経路を通らないビルドは失敗していた。
   Windows / macOS は `packaging/scripts/build-cross.sh` が明示的に `unset` していたため
   既定値 false（= 0）となり、こちらもたまたま正しい値になっていた。

## 修正

`.cargo/config.toml` のグローバル `[env]` に、**ターゲット接尾辞付きの変数名**を列挙する。

`aws-lc-sys` のビルドスクリプトは `AWS_LC_SYS_<NAME>_<target_with_underscores>` を
非接尾辞版より優先して読む（`builder/main.rs` の `optional_env_crate_target` →
`env_name_for_target`）。cargo にターゲット別 env の仕組みが無い以上、これが
「ビルド方法（ローカル cargo / cargo zigbuild / cargo xwin / Docker / QEMU VM）に依らず
ターゲットだけで値が決まる」唯一の方法になる。

```toml
[env]
# linux / freebsd = 1
AWS_LC_SYS_NO_PREFIX_x86_64_unknown_linux_gnu = { value = "1", force = true }
AWS_LC_SYS_NO_PREFIX_x86_64_unknown_freebsd   = { value = "1", force = true }
# windows / macos / openbsd = 0
AWS_LC_SYS_NO_PREFIX_x86_64_pc_windows_msvc   = { value = "0", force = true }
AWS_LC_SYS_NO_PREFIX_x86_64_apple_darwin      = { value = "0", force = true }
AWS_LC_SYS_NO_PREFIX_x86_64_unknown_openbsd   = { value = "0", force = true }
# （aarch64 / musl を含む全 14 トリプル分を定義）
```

あわせて次を整理した。

- `build.rs` から無効な `ensure_aws_lc_no_prefix()` を削除（設定箇所を 1 箇所に集約）。
- `docker/Dockerfile.{glibc,musl,glibc.aarch64,musl.aarch64}` の
  `ENV AWS_LC_SYS_NO_PREFIX=1` を削除（設定の二重化を排除）。
- `packaging/scripts/build-cross.sh` の `-e AWS_LC_SYS_NO_PREFIX=""` +
  `unset AWS_LC_SYS_NO_PREFIX` を削除。
- `tests/{test_backends,grpc_server}/.cargo/config.toml` は親の**接尾辞付き**変数を
  `force = true` で打ち消すよう更新（非接尾辞版だけ 0 にしても親が優先されるため）。
- 親の `[env]` も**テーブル形式**（`{ value = "1", force = true }`）に揃えた。
  親が文字列・子がテーブルだと cargo の設定マージが
  `expected table, but found string` で失敗し、
  `tests/grpc_server` / `tests/test_backends` のビルド（= E2E 全体）が通らない。
  `force = true` にはシェルに残った古い `AWS_LC_SYS_NO_PREFIX_*` に負けない効果もある。

`universal2-apple-darwin` は cargo-zigbuild が `x86_64-apple-darwin` と
`aarch64-apple-darwin` を個別にビルドして lipo するため、両トリプル分を定義している。

## 検証

- `[target.<triple>.env]` が無視され `[env]` の接尾辞付き変数が build.rs に届くことを
  最小クレートで確認。
- `docker/Dockerfile.glibc`（`ENV` 削除後、`--features full`）でリンクまで成功することを確認
  = `[env]` 経由で `AWS_LC_SYS_NO_PREFIX_x86_64_unknown_linux_gnu=1` が効いている。
- ローカル `cargo build --features full`（修正前は失敗していた経路）が成功・warning 0。
- feature 組み合わせ 11 種（default / no-default / http2 / http3 / wasm / grpc-full /
  epoll / ktls / l4-proxy / jemalloc / system-allocator）すべて warning 0 でビルド成功。
- `cargo clippy --all-targets --features full -- -D warnings` / `cargo fmt --check` クリーン。
- `cargo test --bins --test integration_tests --features full` 53 passed / 0 failed。
- `./tests/e2e_setup.sh test` 533 passed / 0 failed。
- Windows / macOS / FreeBSD 向けの `docker/Dockerfile.{windows,macos,freebsd}` は
  **本チケット時点では未実行**（ホストのディスク・時間制約）。

## 関連

- F-131（クロスプラットフォーム TLS / BoringSSL quiche 対応）
- `docker/Dockerfile.{windows,macos,freebsd}` の新設と `tools/qemu/bsd-vm.sh` は本件と
  同じブランチで対応（ビルド環境の整備）。
