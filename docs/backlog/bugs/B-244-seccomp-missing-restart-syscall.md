# B-244: seccomp の許可リストに `restart_syscall` が無く、SIGSTOP/SIGCONT・ptrace の後に背景スレッドの sleep が panic する（strict ではスレッドが黙って消え、終了が止まる）

**状態: 修正済み（v0.10.0）**

## 出典

2026-10-27 の毎晩の container_security（run 40525405501、feat/v0100 c33710a3）の chaos-extended の `syscall_chaos_report.txt`。
B-242 の修正後の syscall_chaos は veil を通常起動してから `strace -f -p 1` で attach するので、どの注入でも次が出ていた:

```
thread '<unnamed>' (12) panicked at library/std/src/sys/thread/unix.rs:537:17:
  io_uring_enter:error=EBUSY runtime=panic recovered=skip
```

## 事象

std の `thread::sleep` は `clock_nanosleep` の戻り値が EINTR 以外なら `assert_eq!(r, libc::EINTR)` で panic する。
sleep 中のスレッドが SIGSTOP（`docker pause`・`kill -STOP`・ジョブ制御）や ptrace の attach で止められると、カーネルは
`-ERESTART_RESTARTBLOCK` を返し、再開時に `restart_syscall(2)` で続きを眠る。この syscall が veil の seccomp の許可リスト
（`src/security.rs` の 4 つの `ALLOWED_SYSCALLS`）にも、コンテナ用の `docker/assets/security/seccomp{,-epoll}.json` にも無かった。

- `seccomp_mode = "filter"`（EPERM）: `clock_nanosleep` が EPERM を返し、sleep している背景スレッド（設定・証明書のリロード、
  ヘルスチェック、ディスク監視など）が 4 本 panic して止まる。プロセスは生き残り、リクエストは返るので気づきにくいが、以後リロードが効かない。
- `seccomp_mode = "strict"`: 違反の動作は `0`（`SECCOMP_RET_KILL_THREAD`。コメントは KILL_PROCESS と書いているが値は KILL_THREAD）なので、
  sleep していたスレッドだけが黙って消える。panic の出力も無く、応答は返り続けるが、SIGTERM で終了しなくなった（SIGKILL が要った）。
- seccomp 無効でも、コンテナを `docker/assets/security/seccomp.json`（既定動作 ERRNO）で動かすと filter と同じになる。

手元の再現（`target/dist/veil`、`seccomp_mode = "filter"`、平文リスナー）: `kill -STOP <pid>; sleep 2; kill -CONT <pid>` で
4 スレッドが `library/std/src/sys/thread/unix.rs:595` で panic。`strict` では panic は出ず、その後 SIGTERM で終了しなかった。
修正後は filter・strict とも 3 回の STOP/CONT の後も panic 0・応答 200・SIGTERM で終了する。

## 修正（v0.10.0）

- `src/security.rs`: 4 つの `ALLOWED_SYSCALLS` に `restart_syscall`（x86_64 = 219、aarch64 = 128）を足した。
  `test_allowed_syscalls_match_libc_numbers` に `libc::SYS_restart_syscall` を足した（番号のずれを防ぐ。B-103）。
- `docker/assets/security/seccomp.json`・`seccomp-epoll.json` に `restart_syscall` を足した。
- `docs/guide/security.md`（英日）の許可 syscall の表に足した。

## 受け入れ条件

- `seccomp_mode = "filter"` と `"strict"` の veil に SIGSTOP/SIGCONT を送っても panic せず、応答を返し続け、SIGTERM で終了する。
- 毎晩の syscall_chaos で `runtime=panic` が出ない。
