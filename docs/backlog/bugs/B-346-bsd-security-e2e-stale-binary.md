# B-346: BSD の VM の security-e2e が、前にビルドした release のバイナリのまま走って PASS する

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

v0.12.0 の最終の検証で見つけた。B-344 の確認を足した FreeBSD のサンドボックスの E2E が「ログのディレクトリの fd を登録した行が無い」で
失敗し、ログの行の位置（`src/entry.rs:1198` など）が今のソースではなく前のコミットのものだった。

## 事象

`tools/qemu/bsd-vm.sh <os> <arch> security-e2e`（`tools/qemu/bsd-security-e2e.sh` も同じ）は、VM の中に既にある release のバイナリ
（`target/release/veil`）をそのまま使う。ソースを同期せずビルドもしないので、前に `build` したときのコミットのバイナリで走り、今のソースの
試験にならない。`unit`・`e2e` はソースを同期してビルドするので、`unit` → `e2e` → `security-e2e` と続けると、サンドボックスの E2E だけが
古いバイナリで走って PASS する。

## 原因

「先に `build` しておく」ことを前提にしていた（`tools/qemu/README.md`・スクリプトの説明）が、確かめていなかった。

## 対応

`security-e2e` は `build`（ソースの同期と release のビルド）をしてから走らせる。ビルド済みなら cargo はすぐ終わる。説明も直した。

## テスト

FreeBSD aarch64 の VM で、`unit`・`e2e` の後に `security-e2e` を回し、今のソースのバイナリ（B-344 のログのディレクトリの登録の行がある）で
走ることを確かめる（結果は下に追記する）。
