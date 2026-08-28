# B-81: macOS クロスビルドが `bind_unix_listener` の `SOCK_NONBLOCK` で壊れている（F-164 のリグレッション）

## 事象

`./packaging/scripts/build-cross.sh --target macos`（`universal2-apple-darwin`、
`CARGO_FEATURES=full`、`--profile dist`）が **コンパイルエラー 2 件で失敗する**。

```
error[E0425]: cannot find value `SOCK_NONBLOCK` in crate `libc`
   --> src/server.rs:982:39
982 |             libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC,
    |                                       ^^^^^^^^^^^^^ help: a constant with a similar name exists: `O_NONBLOCK`
error[E0425]: cannot find value `SOCK_CLOEXEC` in crate `libc`
```

## 原因

F-164（UDS リスナー）で追加した `server::bind_unix_listener` が、`socket(2)` の
type 引数に `SOCK_NONBLOCK | SOCK_CLOEXEC` を渡している。
**これは Linux/BSD の拡張であり、macOS には存在しない**（libc にも定義が無い）。

**B-69（Windows の `accept_batch`）・B-73（Windows の `grpc_executor`）と同じクラスの
不具合で、本リポジトリでは 3 件目である。** Linux の単体・統合・E2E をすべて通過し、
検出手段は `build-cross.sh --target macos` のクロスビルドのみ。

**同じ対処が既にリポジトリ内に存在していた**: `runtime::reactor::tcp::unix.rs` の
`create_nonblocking_socket` は macOS 分岐を持ち、理由もコメントで説明されている
（`docs/artifacts/f125_windows_macos_design.md` の macOS 節 1）。
F-164 が新設した UDS 経路にその方針が適用されていなかった。

## 影響

- **macOS 向け成果物（`packaging/output/veil-<ver>-universal2-apple-darwin.tar.gz`）が
  F-164 以降ビルドできない。**
- Linux / FreeBSD / OpenBSD / NetBSD は `SOCK_NONBLOCK`/`SOCK_CLOEXEC` を持つため影響なし。
  Windows は `cfg(unix)` 外なので影響なし。

## 改修

`create_nonblocking_socket` と同じ形にする。

```rust
#[cfg(target_os = "macos")]
let fd = {
    let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM, 0) };
    if fd < 0 { return Err(io::Error::last_os_error()); }
    unsafe {
        libc::fcntl(fd, libc::F_SETFL, libc::O_NONBLOCK);
        libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC);
    }
    fd
};
#[cfg(not(target_os = "macos"))]
let fd = unsafe { libc::socket(libc::AF_UNIX, libc::SOCK_STREAM | libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC, 0) };
```

macOS 以外は 1 syscall のまま（挙動不変）。

## 再発防止

**「`cfg(unix)` なら macOS でも通る」と考えないこと。** macOS は unix だが
Linux/BSD の socket 拡張フラグを持たない。新しく `libc::socket`/`accept` 系を足すときは
`runtime::reactor::tcp::unix.rs` の既存分岐を必ず参照する。

リリース前チェックリスト（B-69 で追加済み）の
「`build-cross.sh --target windows|macos` を通す」を **macOS についても実際に実行する**こと。
本件は F-164 以降ずっと壊れていた（`packaging/output/` の macOS 成果物の日付が F-164 の
前で止まっていた）。
