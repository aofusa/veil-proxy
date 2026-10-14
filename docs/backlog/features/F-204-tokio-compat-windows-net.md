# F-204: Windows の `veil::tokio` に `net`・`signal` を実装する

**状態: 未着手（設計済み 2026-10-14）**

## 背景

F-203（各 OS での API の E2E）で、Windows の `veil::tokio`（tokio 互換レイヤー、F-198）には `net` と `signal` が無いことを
確かめた（`src/tokio/net.rs`・`signal.rs` は `cfg(unix)`。約 3,100 行が `RawFd`・libc のソケット呼び出しで書かれている）。
このため hyper のサーバ・axum・reqwest・sqlx など、`tokio::net` を使うクレートは Windows では shim の上でビルドできない
（`tools/tokio-compat/ecosystem` は sqlx-core の `tokio::net::TcpStream` で止まる）。veil の `App`・ハンドラ・gRPC・
`tokio_service` で載せる tower のサービス（ソケットを自分で開かないもの）・`tokio-compat-bridge`（本物の tokio）は Windows でも動く。

## 現状の確認（2026-10-14）

- `src/tokio/mod.rs` で `cfg(unix)` なのは `net` と `signal` の 2 モジュールだけ（`io`・`sync`・`time`・`task`・`fs`・
  `runtime` は Windows でもビルドできる）。
- `net.rs` の中身は 3 層に分かれている:
  1. 準備待ち（`Wait`・`WaitAll`・`WaitSlot`・`IoSource`）: `runtime::tcp::wait_readable_fd` / `wait_writable_fd` を呼ぶだけ。
     Windows の reactor（`src/runtime/reactor/tcp/windows.rs`）にも同じ名前・同じ意味の関数があり、`runtime::handle::RawFd`
     は Windows では `SOCKET`。**この層はほぼそのまま使える。**
  2. TCP・UDP の型（`TcpStream`・`TcpListener`・`TcpSocket`・`UdpSocket`・`tcp::{OwnedReadHalf, ...}`・`lookup_host`）:
     ロジックは移植可能だが、`libc::recv`/`send`/`sendto`/`recvfrom`/`accept4`/`connect`/`getsockopt`/`setsockopt`/
     `shutdown` を直接呼んでいる（`libc::` が 217 か所）。
  3. Unix 専用（`UnixStream`・`UnixListener`・`UnixDatagram`・`UnixSocket`・`UCred`・`AsyncFd`）: Windows では tokio も提供しない。
- `signal.rs` は signal-hook のスレッドで数えて `Notify` で起こす。veil 本体は Windows では `ctrlc` で Ctrl-C を受ける
  （`src/server.rs`）。
- `windows-sys` 0.59 は既に Windows 専用依存（`Win32_Networking_WinSock` 等）。

## 設計

### net

1. `src/tokio/net.rs` を `src/tokio/net/` に分ける（挙動は変えない移動だけのコミットを先に置く）:
   - `mod.rs`: 準備待ち（1 層目）と TCP・UDP の型（2 層目）。プラットフォームに依存する呼び出しは全部 `sys::` を通す。
   - `sys_unix.rs`: 今の `libc::` 呼び出しをそのまま移す（`recv`・`send`・`sendto`・`recvfrom`・`accept`・`connect`・
     `sockopt`・`shutdown`・ソケット作成。macOS の `SOCK_NONBLOCK` 無し分岐（B-81）・`SO_NOSIGPIPE` もここ）。
   - `sys_windows.rs`: `windows-sys` の Winsock で同じ関数を書く。`WSAEWOULDBLOCK` を `io::ErrorKind::WouldBlock` に揃え、
     ソケット作成は `reactor/tcp/windows.rs` の `create_nonblocking_socket`（`FIONBIO`）を使う（重複させない）。
   - `unix.rs`: 3 層目（`cfg(unix)` のまま）。
2. ハンドルの trait は OS ごとに付ける: unix は `AsRawFd`/`AsFd`/`FromRawFd`/`IntoRawFd`、Windows は
   `AsRawSocket`/`AsSocket`/`FromRawSocket`/`IntoRawSocket`（tokio と同じ）。`TryFrom<std::net::*>` は両方。
3. Windows で意味が違うもの（tokio の Windows 版に合わせる）:
   - `TcpSocket::set_reuseport` は提供しない（tokio も `cfg(unix)`）。`set_reuseaddr` は `SO_REUSEADDR`。
   - 非ブロッキング `connect` は `WSAEWOULDBLOCK` → 書き込み可能待ち → `SO_ERROR` で結果を取る。WSAPoll は失敗した
     connect を `POLLHUP|POLLERR` で返す版があるので、`SO_ERROR` を必ず読む（B-126 の NetBSD と同じ教訓）。
   - ローカルの `shutdown` を WSAPoll は通知しない（B-88）。`shutdown(Read|Both)` のときは同じソケットの読み取り待機者を
     起こす（reactor の `shutdown` と同じ処理を呼ぶ）。
   - `SIGPIPE` は無い（`MSG_NOSIGNAL` 相当は不要）。
   - `peek`・`recv_from`・`set_ttl`・`set_linger`・`take_error`・`set_nodelay`・keepalive は Winsock に揃える。
4. `lookup_host` は既に offload の `ToSocketAddrs`（移植可能）なので `mod.rs` に置く。

### signal

- `signal::ctrl_c()` と `signal::windows::{ctrl_c, ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown}` を
  `SetConsoleCtrlHandler`（`windows-sys` に `Win32_System_Console` を足す）で実装する。ハンドラは種類ごとのカウンタを増やして
  `Notify` で起こす（unix 版の `Slot` を共有）。
- ハンドラは複数登録でき、後から登録したものが先に呼ばれる。tokio と同じく「受け手がいる種類は TRUE（処理済み）を返す、
  いなければ FALSE（次へ）」にして、veil 本体の `ctrlc` の終了処理（受け手がいないとき）を壊さない。
- `signal::unix` は今のまま `cfg(unix)`。

### 試験

- Windows（`ssh aofish`）で `tools/tokio-compat/ecosystem`（hyper サーバ・h2c クライアント・reqwest・tower-http・
  axum・sqlx sqlite）を通す。今は対象外の `tests/api/library_e2e.sh` の ecosystem 節を Windows でも回す。
- `tools/tokio-compat/run_tokio_tests.py` の `net` 系を Windows で回し、合格率を Linux と並べて記録する。
- unix は移動だけなので、Linux（io_uring・epoll）・macOS・FreeBSD で `cargo test --features api` と ecosystem が
  今と同じ結果になることを確かめる（F-203 の手順）。

## 受け入れ条件

- Windows で ecosystem の全項目（postgres を除く）が ok。
- unix の結果が変わらない。`--features full` のビルド（本体）に変化が無い（`library_e2e.sh nonregression`）。
- `docs/guide/library.md`（英日）の Platforms の表から「Windows は `veil::tokio` の net 無し」の注記を外す。

## 見積もり

`sys_windows.rs` が 500〜700 行、移動が 3,100 行（差分は `libc::` の置き換え）。
