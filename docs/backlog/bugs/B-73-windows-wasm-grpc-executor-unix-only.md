# B-73: Windows クロスビルドが `wasm/host/grpc_executor.rs` で壊れている（F-139 のリグレッション）

## 事象

`./packaging/scripts/build-cross.sh --target windows`（`CARGO_FEATURES=full`）が
**コンパイルエラー 9 件で失敗する**。

```
error[E0433]: failed to resolve: could not find `unix` in `os`
  --> src/wasm/host/grpc_executor.rs:51:14
error[E0412]: cannot find type `pollfd` in crate `libc`      --> src/wasm/host/grpc_executor.rs:871
error[E0425]: cannot find value `POLLIN` in crate `libc`     --> src/wasm/host/grpc_executor.rs:881
error[E0425]: cannot find value `POLLOUT` in crate `libc`    --> src/wasm/host/grpc_executor.rs:883
error[E0422]: cannot find struct ... `pollfd`                --> src/wasm/host/grpc_executor.rs:885
error[E0425]: cannot find function `poll` in crate `libc`    --> src/wasm/host/grpc_executor.rs:...
error[E0412]: cannot find type `nfds_t` in crate `libc`      --> src/wasm/host/grpc_executor.rs:...
error[E0599]: no method named `as_raw_fd` found for ... `std::net::TcpStream` (×2)
```

## 原因

F-139（WASM の gRPC 呼び出しを専用スレッドで駆動する）で追加した
`src/wasm/host/grpc_executor.rs` が、`#[cfg(unix)]` ゲート無しに

- `use std::os::unix::io::{AsRawFd, RawFd};`
- `libc::poll` / `libc::pollfd` / `libc::POLLIN` / `libc::POLLOUT` / `libc::nfds_t`

を使っている。これらは Windows に存在しない。

**B-69 と同じクラスの不具合**（「Linux で全テストが通ることは他プラットフォームの
コンパイルを何ら保証しない」）。単体 923 件・統合 54 件・E2E のすべてを通過する。
検出手段はクロスビルドのみ。

## 影響

- Windows 向け成果物（`packaging/output/`）が F-139 以降ビルドできない。
- Linux / FreeBSD / OpenBSD / NetBSD / macOS は `cfg(unix)` を満たすため影響なし。

## 改修案

1. `grpc_executor` の待機部分（`poll(2)` 駆動ループ）を `cfg(unix)` と
   Windows 実装（`WSAPoll`。`runtime::reactor::wsapoll` と同じ方針）に分ける。
   もしくは
2. モジュール全体を `#[cfg(unix)]` にし、Windows では
   `proxy_grpc_call`/`proxy_grpc_stream`/`proxy_grpc_send` ホスト関数が
   「この環境では未サポート」を返すようにする（機能差を README に明記）。

**再発防止**: B-69 の再発なので、リリース前チェックリストに
「`build-cross.sh --target windows` と `--target macos` を通す」ことを明記する
（AGENTS.md のディレクトリ表・作業フローに追記済み）。

## 対応（2026-08-27、完了）

**改修案 1 を採用**（Windows 実装を足す。「未サポートを返す」案は採らない）。

- `src/wasm/host/grpc_executor.rs` の `use std::os::unix::io::{AsRawFd, RawFd}` を、
  既存のクロスプラットフォーム抽象 `crate::runtime::handle::{AsRawFd, RawFd}` へ差し替え。
  Windows では `AsRawSocket` に対する blanket impl 経由で `std::net::TcpStream` に
  `as_raw_fd()` が生えるため、`ActiveCall::raw_fd()` は無変更で通る。
- `GrpcRunner::poll_all()` 内にインライン展開されていた `libc::poll` 呼び出しを
  `wait_sockets(&[(RawFd, bool)], timeout_ms)` ヘルパーへ切り出し、
  `#[cfg(unix)]`（`libc::poll`）と `#[cfg(windows)]`（`windows_sys` の `WSAPoll`。
  `RawFd`→`SOCKET` 変換は `runtime::handle::win::to_socket`）の 2 実装に分けた。
  呼び出し側のロジック（`timeout_ms` の算出・poll 後に全 call を `poll_once` する流れ・
  `done_keys` の回収）は無変更。ブロッキング待機である旨の理由付き
  `#[allow(clippy::disallowed_methods)]` は両ヘルパーへ引き継いだ。

### 検証

- `packaging/scripts/build-cross.sh --target windows`（`x86_64-pc-windows-msvc`）が
  **成功**し、`veil-artifact:x86_64-pc-windows-msvc` を再生成できることを確認した
  （F-139 以降ずっと失敗していた）。
- Linux 非退行: `cargo fmt --check` クリーン / `cargo clippy --lib --features full -- -D warnings`
  警告 0 / 単体 948・統合 54・E2E 544 すべて pass。
