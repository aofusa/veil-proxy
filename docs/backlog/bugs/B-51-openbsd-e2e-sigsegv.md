# B-51: OpenBSD で E2E テストバイナリが SIGSEGV で落ちる

## 事象

OpenBSD 7.9 amd64（QEMU VM、`tools/qemu/bsd-vm.sh openbsd x86_64 e2e`）で
`tests/e2e_setup.sh test` を実行すると、テストプロセスが **SIGSEGV** で異常終了する。

```
running 533 tests
test common::tests::test_echo_server ... ok
test common::tests::test_get_available_port ... ok
test common::tests::test_get_multiple_ports ... ok
test common::tests::test_simple_http_server ... ok
error: test failed, to rerun pass `--test e2e_tests`

Caused by:
  process didn't exit successfully:
  `/usr/obj/veil-proxy/target/debug/deps/e2e_tests-31cee158e824bcf2 --test-threads=4`
  (signal: 11, SIGSEGV: invalid memory reference)
```

`common::tests::*`（テストハーネス自身の自己テスト）は通っており、その直後に
プロセスごと落ちる。個別テストの assertion failure ではない。

## 切り分けの現状

- **ビルドは成功している**（`--no-default-features --features full-openbsd`、
  リリースビルド 71分56秒。`fetch` したバイナリは
  `ELF 64-bit LSB pie executable, x86-64, (OpenBSD)`）。
- 落ちるのは **E2E テストプロセス**。veil 本体・test_backends・grpc_server は
  別プロセスとして起動しており、health check は通っている
  （`Backend 1: OK` / `Proxy: OK` まで到達）。
- どのテストの実行中に落ちたかは、クラッシュのため出力から特定できていない。
  `--test-threads=1` で走らせて最後に出力されたテスト名を見るのが次の一手。
- OpenBSD 固有の要素として、rustls の暗号プロバイダが **ring**（他ターゲットは
  aws_lc_rs、F-122）、アロケータが**システム malloc**（`full-openbsd`）、
  ランタイムが kqueue reactor である点が挙げられる。OpenBSD の malloc は
  デフォルトで境界チェックが厳しく（`malloc.conf` の J/S フラグ）、
  他 OS では顕在化しないメモリ誤用を落とす傾向がある点にも注意。

## 再現手順

```bash
tools/qemu/bsd-vm.sh openbsd x86_64 all
# もしくは既存 VM で
tools/qemu/bsd-vm.sh openbsd x86_64 e2e
```

絞り込み:

```bash
tools/qemu/bsd-vm.sh openbsd x86_64 ssh \
  'cd /usr/obj/veil-proxy && CARGO_HOME=/usr/obj/cargo \
   cargo test --test e2e_tests --no-default-features --features full-openbsd \
   -- --test-threads=1 --nocapture 2>&1 | tail -50'
```

## 影響

- OpenBSD 向け配布物（`veil-<version>-x86_64-unknown-openbsd.tar.gz`）は
  **ビルドはできるが E2E で検証できていない**。
- 過去に「静的配信/プロキシとも HTTPS 200 で動作する」ことは確認されている
  （F-122 / packaging/README.md）ため、veil 本体が常に落ちるわけではないと思われる。
  クラッシュがテストハーネス側か veil 側かの切り分けが必要。

## 関連

- B-47（本件の検出につながった QEMU ビルド環境整備）
- B-50（FreeBSD で HTTP/3 の UDP が bind されない。別件）
- F-122（OpenBSD は rustls の ring プロバイダ）
- F-120 Phase 5（pledge / unveil）
