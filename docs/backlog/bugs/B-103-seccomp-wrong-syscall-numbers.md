# B-103: seccomp 許可リストの syscall 番号が一部まちがっていた（x86_64 の prctl など）

## 事象

E2E サニタイザ（ASAN、`-Zbuild-std` の debug-assertions 付き std）で veil が負荷中に abort した。

```
thread 'veil-fs-offload' panicked at std/src/sys/thread/unix.rs:462:9:
assertion `left == right` failed  left: 1  right: 0
fatal runtime error: failed to initiate panic, error 5, aborting
ERROR: AddressSanitizer: SEGV ... (libc abort)
```

オフライン symbolize の結果、`std::sys::thread::unix::set_name`（スレッド名の設定）が
`EPERM` を返していた。オフロードスレッドは seccomp 適用後に遅延生成されるため、
`prctl(PR_SET_NAME)` が seccomp で拒否されていた。

## 原因

`src/security.rs` の `ALLOWED_SYSCALLS` は番号の手書きで、コメントと番号がずれていた。

| アーキ | 書いていた番号（コメント） | 実際の syscall | 正しい番号 |
|---|---|---|---|
| x86_64（io_uring / reactor） | 147（prctl） | sched_get_priority_min | **157** |
| aarch64（io_uring / reactor） | 227（mremap） | msync | **216** |
| aarch64 | 146〜149（setresuid / getresuid / setresgid / getresgid） | setuid / setresuid / getresuid / setresgid（1 つずれ） | **147〜150** |
| aarch64 | 63（uname） | read（別途許可済みの重複） | 160（既に別行で許可） |

x86_64 では **prctl がずっと拒否されていた**。リリースビルドの std はスレッド名の設定失敗を
無視するので表面化せず（seccomp 適用後に作ったスレッドが無名になるだけ）、debug-assertions
付きの std でだけ abort になった。aarch64 の mremap 拒否は glibc / mimalloc が
フォールバックするため表面化していなかった。

## 修正

番号を正しくし、`libc::SYS_*`（アーキごとの正しい番号）で必須 syscall が許可リストに
入っていることを確認する単体テスト `test_allowed_syscalls_match_libc_numbers` を追加した
（x86_64 / aarch64、io_uring / reactor の 4 表すべてに効く）。

## テスト

- 単体: `security::tests::test_allowed_syscalls_match_libc_numbers`
- E2E サニタイザ（ASAN）: findings 0（修正前は abort）
