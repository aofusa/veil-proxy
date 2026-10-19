# B-156: aarch64 の seccomp 許可リストに rseq が無く、glibc 2.35+ で起動直後に abort する

## 出典

v0.8.0-rc.6 の `release.yml` のイメージの試験（`tools/release/image-smoke.sh`）。`glibc/arm64` だけが起動直後に
終了コード 133（SIGTRAP）で落ち、他の 3 つ（`glibc/amd64`・`musl/amd64`・`musl/arm64`）は通った。

## 原因

glibc 2.35 以降は新しいスレッドごとに `rseq(2)` を登録し、`ENOSYS` 以外のエラーで失敗すると
`Fatal glibc error: rseq registration failed` で abort する。x86_64 の許可リストには `334, // rseq` があったが、
aarch64（io_uring・reactor の両方）には `293` が無く、seccomp（`Filter` = `EPERM`）の適用後に作るワーカー
スレッドで abort していた。

- イメージの設定（`docker/assets/conf.d/config.toml`）は `enable_seccomp = true`。
- E2E・プラットフォーム検証の Linux aarch64（Ubuntu の VM）は seccomp を有効にした設定で起動しないため見えなかった。
- musl は rseq を使わないので `musl/arm64` は通る。
- 配布物の `veil-0.8.0-aarch64-unknown-linux-gnu.tar.gz`・`veil_0.8.0_arm64.deb`・`veil-0.8.0-1.aarch64.rpm` も、
  seccomp を有効にすると同じく落ちる。

Linux aarch64 の VM（`tools/qemu/aarch64-vm.sh`）で、同じバイナリが seccomp 有効で
`Fatal glibc error: rseq registration failed`（exit 134）、無効で 200 を返すことを確かめた。

## 対応

- aarch64 の 2 つの許可リストに `293`（rseq）を足した。
- x86_64 にあって aarch64 に無かったもののうち、aarch64 にも存在する `124`（sched_yield）・`168`（getcpu）・
  `284`（mlock2）も足した（`Strict` = `KILL_PROCESS` だと即死する）。x86_64 だけの旧 syscall（`open`・`stat`・
  `poll` 等）と、seccomp の前に終わる権限降格（`setuid`/`setgid`）は対象外。
- `test_allowed_syscalls_match_libc_numbers` に `libc::SYS_*` で 4 つを足した（aarch64 のビルドで番号を固定する）。
- Linux aarch64 glibc の配布物を作り直した。
