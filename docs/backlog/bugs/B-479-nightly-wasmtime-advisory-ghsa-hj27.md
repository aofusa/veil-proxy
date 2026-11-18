# B-479: container-security の nightly の `cargo_audit` が vendoring の wasmtime 36.0.20 の勧告（GHSA-hj27-mpw9-ff4c）で落ちる

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

## 出典

main（92197f25）の `container-security-nightly` の定時実行（run 43997536152・44209899182）が、`run.sh`（musl・glibc）の
`cargo_audit` で失敗していた。fix/security-scan-fable-max を main へ入れた後、nightly を回す前に直前の結果を確かめて見つけた。
PR の CI（`ci`・`bindings`）は拾わない。

## 事象

- `run_cargo_audit.sh` の vendoring の照合（`vendored_ghsa_audit.py`。F-277）が、`third_party/wasmtime` の
  `wasmtime@36.0.20` を上流のリポジトリの勧告 GHSA-hj27-mpw9-ff4c（low、2026-11-16 公開）に一致させた。ホスト関数が
  WebAssembly からではなく埋め込み側から呼ばれたとき（ゲストがホスト関数を取り込んでそのまま書き出し、埋め込み側がそれを呼ぶ等）、
  `Caller::get_module_export` が `None` を返さずに panic する。36.0.21 未満（LTS）が対象。
- `pipefail` で照合の失敗がそのまま段階の失敗になり、その後の `cargo audit`（全部の `Cargo.lock`）は走っていなかった。
- veil は `Caller::get_module_export` を呼ばない（`src/`・`crates/` に無い）ので到達しない。ただし勧告の照合を通さないと
  nightly とリリースの検査が通らない。crates.io の `wasmtime`（Linux・macOS・Windows・FreeBSD/OpenBSD x86_64）も同じ 36.0.20 で、
  RustSec に載れば `cargo audit` がルートと `examples/api` の 8 つの `Cargo.lock` でも落ちる。

## 対応

- vendoring（`third_party/wasmtime`、`veil-proxy-wasmtime`）と crates.io の `wasmtime` を、同じ LTS の修正版 36.0.21 へ上げた。
  上流の 36.0.20 → 36.0.21 の差は `src/runtime/instance.rs` の 1 行（`Caller` の実体のモジュールをストアから引き、無ければ `None`）と
  `Cargo.toml` の依存の版（Cranelift 0.123.21）だけ。crates.io の `.crate` の sha256 を索引の `cksum` と照合してから取り込み、
  veil の差分（`Cargo.toml`・`build.rs`・`unix.rs`・`rustfmt.toml`）は、上流の 36.0.21 との差が上流の 36.0.20 との差と同じで
  あることを確かめた（`third_party/wasmtime/README.veil.md`）。
- `Cargo.lock`（ルート・`examples/api` の 8 つ）は `cargo update -p wasmtime --precise 36.0.21` で、wasmtime・Cranelift・Pulley・
  Winch の一族だけを上げた（ほかの依存は動かしていない）。

## テスト

- `vendored_ghsa_audit.py`: `ok: wasmtime@36.0.21 has no GitHub advisories`。`run_cargo_audit.sh` の docker の中身を
  この端末の `cargo audit --deny warnings` で回し、リポジトリの全部の `Cargo.lock`（ルート・例・試験・計測ツール・VDS）と
  vendoring の仮の `Cargo.lock` が通る。
- `cargo check -p veil-proxy-wasmtime`（veil と同じ feature）・`cargo clippy --all-targets`（`full`・`full,epoll`）が通る。
  単体 1,414（`wasm::` の 179 件を含む）・統合 63・自己完結型 E2E 58・E2E 595（Proxy-Wasm の 37 件を含む）が通る。

## 関連

- [B-263](B-263-nightly-advisories-and-musl-test-build-v0100.md)・[B-157](B-157-dependency-advisories-v083.md)（前回までの
  wasmtime の更新）、[F-277](../features/F-277-vendored-quiche-advisory-matching.md)（vendoring の勧告の照合）。
