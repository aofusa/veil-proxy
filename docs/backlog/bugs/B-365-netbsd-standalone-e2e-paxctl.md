# B-365: NetBSD の自己完結型 E2E で WASM を使う試験が `Permission denied` で落ちる

**状態: 修正済み（feat/beat-nginx、2026-11-13）**

feat/beat-nginx の全プラットフォームの確認（NetBSD 10.1 aarch64、`cargo test --no-default-features --features full-netbsd --test standalone_e2e`）で見つけた。
veil 本体ではなく試験のハーネスの不具合。

## 事象

自己完結型 E2E の 50 件のうち、WASM のモジュールを読む 7 件（`b186_*`・`b188_*`・`b189_*`・`f271_*`・`matrix_tests::feature_protocol_matrix`・
`parity_tests::b268_*`・`parity_tests::f285_*`）が、veil の起動で次のように失敗する。

```
Config load error: Failed to initialize WASM Filter Engine: wasm module 'baseline' failed to start: Permission denied (os error 13)
```

NetBSD は PaX MPROTECT がシステム全体で有効で、wasmtime の `mprotect` が EACCES になる（B-60）。E2E（`tests/e2e_setup.sh`）と単体試験
（`bsd-vm.sh unit` の cargo の runner）は veil・試験のバイナリに `paxctl +m` を掛けているが、自己完結型 E2E が起動する
`CARGO_BIN_EXE_veil` には誰も掛けていなかった。同じ VM の E2E（591 件）・単体（1,197 件）・統合（63 件）は通る。

## 対応

`tests/standalone/mod.rs` の `veil_bin()` が NetBSD のときだけ、最初の 1 回にバイナリへ `/usr/sbin/paxctl +m` を掛ける（`std::sync::Once`）。
失敗しても試験は続ける（警告を出す。WASM を使わない試験は影響を受けない）。他の OS の挙動は変えない。

## テスト

- NetBSD 10.1 aarch64（HVF）の `cargo test --no-default-features --features full-netbsd --test standalone_e2e` が全件通る（修正前は 7 件が失敗）。
