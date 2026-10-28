# F-278: seccomp の引数フィルタを `socket`・`ioctl`・`prctl`・`clone3`・`tgkill`・`setuid` 系にも広げ、許可リストの長さを静的に検査する

**状態: 完了（v0.10.0）**

## 今の状態

- `security::build_seccomp_filter` の引数フィルタは `mmap`/`mprotect` の `PROT_EXEC`（F-25）だけ。
- `socket`（任意のファミリ）、`ioctl`（任意の request）、`prctl`（任意の option）、`clone`/`clone3`（`CLONE_NEWUSER` 等）、`tgkill`（任意の pid）、
  `setuid`/`setgid`/`setgroups`（降格後も許可）が無条件に通る。io_uring の opcode の制限（13 個）と比べると syscall 側が粗い。
- BPF の `jt`/`jf` は u8。許可リストが約 250 を超えると `as u8` で黙って壊れる（今は 100 強）。

## 改修内容

- `socket`: `AF_INET`・`AF_INET6`・`AF_UNIX`・`AF_NETLINK`（必要なら）だけ。`ioctl`: `FIONBIO`・`TIOCGWINSZ`・kTLS・`SIOCGIFADDR` など使う request を列挙。
  `prctl`: `PR_SET_NAME`・`PR_SET_NO_NEW_PRIVS`・`PR_SET_SECCOMP`（適用時）など。`clone`/`clone3`: `CLONE_NEWUSER`・`CLONE_NEWNS` を拒否。
  `setuid` 系: 降格後は拒否（起動の順序で seccomp は降格の後なので丸ごと外せる可能性）。
- `const_assert!(ALLOWED_SYSCALLS.len() < 240)` を 4 種の表すべてに。
- 引数フィルタは `ALLOWED_SYSCALLS` と同じく `test_allowed_syscalls_match_libc_numbers` の方式で定数を `libc` と照合する（B-103）。

## 受け入れ条件

- seccomp 有効の E2E（F-279）が全経路で通る。`tools/container_security/security/run_full_features_security.sh` の seccomp 禁止 syscall の発火試験に
  `socket(AF_PACKET)` などを足して拒否される。

## 修正（v0.10.0）

socket・ioctl・prctl・clone・clone3・tgkill の引数の検査、set*id の除去、syscall ごとのブロックで u8 のジャンプ幅の制約を解消。単体（引数の検査を子プロセスで確認・レイアウト）+ strict seccomp の E2E 576 件で確認。container_security の発火試験への追加は F-279 で扱う

## 関連

- [F-25](F-25-seccomp-bpf-prot-exec-validation.md)、[B-219](../bugs/B-219-seccomp-landlock-worker-threads-only.md)、[B-220](../bugs/B-220-seccomp-missing-dup-unlink-mkdir.md)
