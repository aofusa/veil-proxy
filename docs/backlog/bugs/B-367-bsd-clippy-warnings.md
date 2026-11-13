# B-367: BSD（FreeBSD・NetBSD）の clippy `-D warnings` が veil の BSD 専用のコードで失敗する

**状態: 修正済み（feat/beat-nginx、2026-11-13）**

feat/beat-nginx の全プラットフォームの確認（[B-363](B-363-concentrate-dead-code-non-linux.md) の後の BSD 3 OS aarch64 の VM、
rustc・clippy 1.96）で見つけた。どれも feat/beat-nginx より前からあるコードで、CI（Linux）の clippy では
コンパイルされない（`target_os` で分かれている）ので見えていなかった。挙動の不具合ではない。

## 事象

`cargo clippy --all-targets --no-deps --no-default-features --features full-<os> -- -D warnings` が失敗する。

| OS | 場所 | lint |
|---|---|---|
| FreeBSD | `src/runtime/reactor/sendfile.rs`・`src/tls_reload.rs` | `io_other_error`（`io::Error::new(ErrorKind::Other, ..)`） |
| FreeBSD・NetBSD | `src/proxy/forward.rs`（Wasm の応答本文のフェーズの条件） | `nonminimal_bool`（clippy 1.96。1.99 は出さない） |
| NetBSD | `src/runtime/reactor/executor.rs`（kqueue の `EV_ERROR` の判定） | `unnecessary_cast`（NetBSD は `kevent.flags`・`EV_ERROR` が u32 で、u32 へのキャストが同じ型への変換になる） |
| FreeBSD | `src/runtime/reactor/tcp/unix.rs`（`BrokerConnect`）・`src/security.rs`（capsicum の `attach_jail`） | `items_after_test_module`（試験のモジュールの後に項目がある） |
| FreeBSD | `src/security.rs` の capsicum の試験 4 件 | `disallowed_methods`（`std::fs::write`・`read_to_string`。他の試験は理由付きの allow がある） |

OpenBSD の rustc（ports）には clippy が無い。`cargo check --all-targets --no-default-features --features full-openbsd` は警告 0。

`--no-deps` を付けないと、vendoring の wasmtime（`third_party/wasmtime`。NetBSD・FreeBSD/OpenBSD aarch64 だけが使う）も
暗黙のワークスペースメンバー（rust-lang/cargo#6745）として lint され、第三者のコードの `uninlined_format_args` 22 件で止まる。

## 対応

- `io::Error::other(..)` にする。
- 条件は `!(HEAD || 本文の無い状態 || gRPC) && 本文を受けるモジュールがある` の形にする（評価の順は変えない。設定の読み込みは
  最後のまま）。
- `EV_ERROR` の判定はキャストを外す（`kevent.flags` と `EV_ERROR` の型は各 OS で揃っている。macOS・FreeBSD・OpenBSD・
  DragonFly は u16、NetBSD は u32）。
- 試験のモジュールの後にあった `BrokerConnect`・`attach_jail` を試験の前へ移す（中身は変えない）。
- capsicum の試験 4 件に、他の試験と同じ理由付きの `#[allow(clippy::disallowed_methods)]` を付ける。
- `tools/qemu/README.md` に、VM の中の clippy は `--no-deps` を付けて回すことを書く。

## テスト

- FreeBSD 14.3・NetBSD 10.1 aarch64（HVF）の `cargo clippy --all-targets --no-deps --no-default-features --features full-<os> -- -D warnings`
  が通る（修正前は上の lint で失敗）。OpenBSD 7.8 aarch64 の `cargo check --all-targets` は警告 0 のまま。
- Linux の `full`・`full,epoll` の clippy・単体試験が通る。
