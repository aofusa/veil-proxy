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

---

## 真因（2026-07-27 確定）

`--test-threads=1` で落ちるテストを特定した:

```
test common::http2_client::tests::test_http2_client_creation ... ok
test common::http3_client::tests::test_http3_client_creation ... [Segmentation fault]
```

コアダンプの backtrace:

```
#0  aws_lc_0_41_0_curve25519_x25519base_byte_alt ()
    at .../aws-lc-sys-0.41.0/aws-lc/third_party/s2n-bignum/s2n-bignum-imported/
       x86_att/curve25519/curve25519_x25519base_alt.S:491
491	        movq    (%r10), %rax
```

**aws-lc-sys の s2n-bignum アセンブリが OpenBSD で動かない**（F-122 が本体で
aws-lc-rs を避けている理由そのもの）。本体は OpenBSD だけ `ring` を使うが、
**dev-dependencies が aws-lc-rs を引き込んでいた**ため、テストクライアントだけが
aws-lc-rs を使っていた。引き込み元は 4 つ:

| crate | 引き込み方 |
|---|---|
| `tonic` | `features = ["tls-aws-lc"]` |
| `hyper-rustls` | 既定 feature に `aws-lc-rs` が含まれる |
| `tokio-rustls` | 既定 feature に `aws_lc_rs` が含まれる |
| `rcgen` | `features = [..., "aws_lc_rs"]` 固定 |

`resolver = "2"` により本体ビルドへは波及しないが、**テストビルドには波及する**。

## 修正

- `Cargo.toml`: 上記 4 crate を `[target.'cfg(not(target_os = "openbsd"))'.dev-dependencies]`
  と `[target.'cfg(target_os = "openbsd")'.dev-dependencies]` に分割し、OpenBSD 側は
  `ring` backend を使う。`quinn` の `rustls` feature は `rustls-ring` の別名なので分割不要。
- `tests/`: `rustls::crypto::aws_lc_rs` の直参照を target 別エイリアス `test_crypto`
  （非 OpenBSD = aws_lc_rs / OpenBSD = ring）に置換。本体 `src/tls_provider.rs` の
  選択と一致する。

検証: `cargo tree --target x86_64-unknown-openbsd --no-default-features
--features full-openbsd -i aws-lc-rs` が "nothing to print" になること
（Linux 側は従来どおり aws-lc-rs が入ることも確認済み）。

## 追補: `tests/test_backends` にも同じ問題があった（2026-07-27）

上記の修正後に OpenBSD で E2E を通したところ、今度は **TLS エコーバックエンド
（`tests/test_backends`）が SIGSEGV** で落ちた。

```
tests/e2e_setup.sh: line 1: 65350 Segmentation fault (core dumped) \
  ... "${SCRIPT_DIR}/test_backends/target/debug/test-backends" > /tmp/test_backends.log
```

`tests/test_backends` は**独立したワークスペース**（自前の `Cargo.toml` /
`Cargo.lock`）で、`tokio-rustls = "0.26"` を既定 feature のまま使っていたため
`aws_lc_rs` が入っていた。veil 本体・E2E テストクライアントと同じ target 分割を
このクレートにも入れて OpenBSD では `ring` を使うようにした。

**教訓**: OpenBSD で aws-lc を排除するときは、
`Cargo.toml`（本体 + dev-deps）だけでなく **`tests/` 配下の独立クレート**
（`tests/test_backends`、`tests/grpc_server`）も確認すること。
`tests/grpc_server` は `tonic` を TLS feature 無しで使っているため影響しない。
