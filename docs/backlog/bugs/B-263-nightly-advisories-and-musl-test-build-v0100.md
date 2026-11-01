# B-263: v0.10.0 の最終候補で container-security の nightly が落ちる（wasmtime・pypi-publish の勧告、musl の単体試験のコンパイル、root でのマウント名前空間の試験）

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の最終候補（`44683566`・`bba4534c`）で `container-security-nightly` を `feat/v0100` に対して回したところ、次の 4 つで失敗した
（run 41252710371・41255615163）。PR の CI（`ci`・`bindings`）はどれも拾わない。

## 事象と修正

| 事象 | 原因 | 修正 |
|---|---|---|
| `run.sh` の `cargo_audit` が `wasmtime@36.0.19` の GHSA-6fr9-xmw9-5352（medium、2026-10-29 公開）で警告 | component model で、コンポーネントが自分で定義した資源の `borrow` を受け取ると、ストアの上限に数えられないホストのメモリが確保される（ゲストがホストのメモリを際限なく使える）。veil は core module しか読み込まないが、`component-model` の feature は有効 | crates.io の依存と vendoring 版（`third_party/wasmtime`、`veil-proxy-wasmtime`）を 36.0.20 にした。vendoring 版は `resources.rs` と `Cargo.toml` の版だけを取り込んだ（`third_party/wasmtime/README.veil.md`）。`examples/api/*` の `Cargo.lock` も更新 |
| SBOM の grype が `pypa/gh-action-pypi-publish` v1.14.2 の GHSA-5xjx-3w67-w9x9（Critical、2026-10-30 公開）で失敗 | Sigstore の鎖の CA の失効に追随していない版 | `release.yml` の固定を v1.14.3（`7245906f`）にした |
| musl のイメージのビルドの中の `cargo test --lib` がコンパイルできない | F-278 の試験の `libc::ioctl(fd, libc::SIOCGIFCONF, ..)`: 要求の型が glibc は `c_ulong`、musl は `c_int`（定数は両方 `c_ulong`） | `as _` で引数の型へ変換する |
| glibc のイメージのビルドの中の `cargo test --lib` で `security::sandbox_mount_tests` が失敗 | ビルドの中は root だが `CAP_SYS_ADMIN` が無く、`unshare(CLONE_NEWNS)` が `EPERM`（B-260 の試験） | 子が `EPERM` のときは専用の終了コードで返し、親は飛ばす（`bba4534c`） |

## 影響

- P1（公開を止める）: `release.yml` のイメージ（musl・glibc）のビルドも同じ `cargo test --lib` を走らせるので、そのままでは v0.10.0 のイメージが作れない。
  wasmtime の勧告は veil の使い方（core module のみ）では到達しないが、cargo-audit を通さないと公開の検査が通らない。

## 関連

- [B-157](B-157-dependency-advisories-v083.md)（前回の wasmtime の更新）、[B-260](B-260-sandbox-dev-nodes-become-empty-files.md)、F-278、[F-276](../features/F-276-v0100-verification-and-release.md)。
