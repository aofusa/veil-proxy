# B-220: seccomp の許可リストに `dup`・`unlink(at)`・`mkdir(at)` が無く、UDS のリスナーとディスクのスピルオーバーが seccomp と両立しない

**状態: 未修正（2026-10-27 起票。セキュリティレビュー SB-02）**

## 出典

2026-10-27 のセキュリティレビュー（v0.9.2 のコード読解）。`tests/e2e_tests/` に veil 自身の seccomp を有効にした試験が 1 件も無い
（`grep seccomp` が 0 件）ため見つかっていなかった。

## 事象

`security::ALLOWED_SYSCALLS`（x86_64・aarch64 とも）に次が無い。

| syscall | x86_64 | aarch64 | 使う経路 |
|---------|--------|---------|----------|
| `dup` | 32 | （無し。`dup3` 24） | `entry.rs::ListenerSource::obtain`（L1180 付近）: `[server].listen_unix` のリスナーを各ワーカーが **seccomp 適用後に** `dup(2)` する |
| `unlink` / `unlinkat` | 87 / 263 | 35 | `proxy/buffered.rs` の `remove_file`（ディスクのスピルオーバーの一時ファイル）、終了時の `unlink_unix_socket`（`entry.rs` L2002 付近） |
| `mkdir` / `mkdirat` | 83 / 258 | 34 | `buffering/handler.rs:69` の `create_dir_all`（スピルオーバーのディレクトリ） |

- `seccomp_mode = "filter"`（EPERM を返す）: UDS のワーカーが起動できない。スピルオーバーは一時ファイルを消せずディスクが埋まる。
- `seccomp_mode = "strict"`（SIGSYS）: UDS のワーカーでプロセスが死ぬ。スピルオーバーでは**最初にスピルする要求がプロセスを落とす**
  （本文の大きい要求を送るだけで、リモートから誘発できる）。終了時の UDS の `unlink` でも SIGSYS。

## 影響

`enable_seccomp = true` の運用で、UDS リスナー・ディスクのスピルオーバーを使うと起動できないか落ちる。strict ではリモートからの DoS。

## 改修案

- `ALLOWED_SYSCALLS` に `dup`・`dup3`・`unlink`・`unlinkat`・`mkdir`・`mkdirat`（aarch64 は `*at` 系と `dup3`）を足す。番号は `libc::SYS_*` で
  確かめ、`test_allowed_syscalls_match_libc_numbers` にも足す（B-103）。
- `docker/assets/security/seccomp.json`・`seccomp-epoll.json` にも同じ syscall を足す（コンテナの UDS・スピルオーバー）。
- `docs/guide/security.md`（英・日）の必要 syscall の表に足す。

## 受け入れ条件

- E2E: `enable_seccomp = true, seccomp_mode = "strict"` で、UDS のリスナー・ディスクのスピルオーバー（上限を小さくして確実にスピルさせる）・
  静的配信・Wasm を 1 本ずつ通し、プロセスが生きていること（F-279 の seccomp 有効の E2E）。
- 終了時に UDS のソケットファイルが消えていること。

## 関連

- [B-219](B-219-seccomp-landlock-worker-threads-only.md)、[B-154](B-154-static-first-request-404-when-openat2-blocked.md)（コンテナの seccomp に無かった openat2）、
  [F-279](../features/F-279-security-test-gaps.md)
