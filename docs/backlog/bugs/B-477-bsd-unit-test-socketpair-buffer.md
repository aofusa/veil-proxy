# B-477: BSD で HTTP/1.1 の本文の上限の単体試験が、AF_UNIX の小さい送信バッファへの書き込みで止まる

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max の確認で、FreeBSD 14.3 の VM の `cargo test --lib --features full-freebsd` が止まったことから
見つけた（main の 92197f25 から。[B-372](B-372-chunked-framing-unbounded.md) の試験）。

## 事象

- `proxy::h1::b186_overflow_tests::read_full_request_body_bounds_chunked_framing` は、socketpair のクライアント側へ 20〜30KiB の
  本文（長いチャンク拡張・多数のトレーラ）を同じスレッドで先に全部書いてから、サーバ側で読む。
- AF_UNIX の送受信バッファの既定は Linux が 200KiB ほどだが、FreeBSD は 8KiB（`net.local.stream.sendspace`・`recvspace`）で、
  macOS・OpenBSD・NetBSD も小さい。読み手が回らないので書き込みが空きを待ったまま止まった（`procstat -kk` で
  `sbwait ← sosend_generic ← kern_sendit`）。
- リリースの BSD の job（`tools/release/bsd-guest-build.sh`）も単体試験を回すので、25 分の打ち切りとやり直しで次のリリースの
  FreeBSD・OpenBSD・NetBSD のビルドが失敗するところだった。製品のコードの不具合ではない。

## 対応

- 試験の socketpair を作る補助（`pair`）で、両端の `SO_SNDBUF`・`SO_RCVBUF` を 256KiB に広げる（`kern.ipc.maxsockbuf`
  の既定 2MiB の範囲。設定に失敗したら試験を落とす）。

## テスト

- FreeBSD 14.3 の VM で `cargo test --lib --test integration_tests --no-default-features --features full-freebsd`
  （`bsd-vm.sh freebsd x86_64 unit`）が止まらずに通ることを確かめた（単体 1,289 件・統合 63 件。修正前は上の試験で止まり、
  それまでに 1,288 件が通っていた）。
- Linux の `cargo test --lib --features full -- proxy::h1::b186_overflow_tests` も通る。
