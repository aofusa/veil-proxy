# B-405: main の CI が Rust 1.100 の clippy と、io_uring の無いイメージのビルドの中の GSO の単体試験で落ちた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx を main へ入れた後（77160e9e）の CI で見つけた。seccomp strict の E2E の失敗は B-404。

## 事象と原因

1. `ci` の `clippy (full)`: CI の stable が Rust 1.100.0（2026-11-09）になり、`needless_range_loop` が
   `src/veil_alloc/segment.rs` の `take_run`（`for i in first..first + n { self.slot_head[i] = first as u8; }`）で出た。
   手元の 1.99.1 では出ない。
2. `container-security-nightly` の `run.sh (glibc)`・`run.sh (musl)`・`SBOM + vulnerability scan`: いずれもイメージのビルドの中の
   `cargo test --lib` で、F-305・F-309・B-361 の GSO の単体試験 3 件（`http3_server::tests::gso_*`）が
   `Failed to initialize io_uring ring: PermissionDenied` で落ちた。ビルドのサンドボックスは io_uring を拒否する（AGENTS.md の
   「io_uring を必須にしない」）。3 件とも `runtime::block_on` を使うのに、io_uring を使えるかを見ていなかった（F-305 の試験は
   io_uring の送出の経路を飛ばすだけで、`block_on` 自体が落ちる）。SBOM のジョブは最初のイメージのビルドで止まり、走査まで
   行っていない。

## 修正

1. `slot_head[first..first + n].fill(first as u8)` にした（同じ処理）。Rust 1.100.0 の clippy（`full`・`full,epoll`、
   `--all-targets`）で他に指摘が無いことを確かめた。
2. 3 件の試験の先頭で `runtime_available()`（`http3_stream` の試験と同じ判定。io_uring のビルドではリングを作れるか）を見て、
   使えなければ飛ばす。F-305 の試験は io_uring のビルドでは sendmmsg と io_uring の送出の両方の経路を確かめる。

## 確認

- `cargo +1.100.0 clippy --all-targets --features full -- -D warnings`・`full,epoll`、1.99.1 の同じもの。
- io_uring を拒否した環境（`systemd-run --user -p SystemCallFilter=~io_uring_setup -p SystemCallErrorNumber=EPERM`）で
  `cargo test --lib --features full` の試験のバイナリを全部回し、失敗が無い（GSO の 3 件は飛ばす）。
