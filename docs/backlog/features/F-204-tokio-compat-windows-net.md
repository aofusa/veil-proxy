# F-204: Windows の `veil::tokio` に `net`・`signal` を実装する

**状態: 未着手**

## 背景

F-203（各 OS での API の E2E）で、Windows の `veil::tokio`（tokio 互換レイヤー、F-198）には `net` と `signal` が無いことを
確かめた（`src/tokio/net.rs`・`signal.rs` は `cfg(unix)`。約 3,100 行が `RawFd`・libc のソケット呼び出しで書かれている）。
このため hyper のサーバ・axum・reqwest・sqlx など、`tokio::net` を使うクレートは Windows では shim の上でビルドできない
（`tools/tokio-compat/ecosystem` は sqlx-core の `tokio::net::TcpStream` で止まる）。veil の `App`・ハンドラ・gRPC・
`tokio_service` で載せる tower のサービス（ソケットを自分で開かないもの）・`tokio-compat-bridge`（本物の tokio）は Windows でも動く。

## やること

- WSAPoll の reactor（`src/runtime/reactor/tcp/windows.rs`）の上に `TcpStream`・`TcpListener`・`TcpSocket`・`UdpSocket`・
  `lookup_host` を実装する（`SOCKET` と `AsRawSocket`。Unix ドメインソケット・`AsyncFd` は対象外）。
- `signal::ctrl_c` と `signal::windows`（`ctrl_break` 等）。
- `tools/tokio-compat/run_tokio_tests.py` と `ecosystem` を Windows で回し、net の合格率を Linux と並べる。
