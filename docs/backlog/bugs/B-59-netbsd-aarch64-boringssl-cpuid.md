# B-59: NetBSD/aarch64 で BoringSSL の `OPENSSL_cpuid_setup` が未定義参照になりリンクが失敗する

**状態: 回避済み（upstream 対応待ち）**

## 事象

NetBSD 10.1 evbarm-aarch64（実機、QEMU/HVF on Apple Silicon）で
`cargo build --release --no-default-features --features full-netbsd`
を実行すると、**リンク工程**で失敗する。

```
error: linking with `cc` failed: exit status: 1
  ld: .../libquiche-*.rlib(crypto.c.o): in function `do_library_init':
      crypto.c:(.text.startup.do_library_init+0x0): undefined reference to `OPENSSL_cpuid_setup'
```

## 原因

`http3` の quiche 0.24.9 は `boringssl-boring-crate` feature（外部 `boring` 4.22 /
boring-sys の vendored BoringSSL）を使う（AGENTS.md のとおり NetBSD は
aws-lc-sys ではなく boring 側）。BoringSSL の aarch64 CPU 機能検出
`OPENSSL_cpuid_setup` は OS ごとの実装ファイル（`crypto/cpu_aarch64_linux.c` /
`_apple.c` / `_win.c` / `_fuchsia.c` / `_freebsd.c` / `_openbsd.c` 等）に分かれて
おり、**NetBSD 向けの実装が無い**ためどれもコンパイルされず、`crypto.c` の
`do_library_init` から参照だけが残って未定義シンボルになる。

x86_64 は CPUID 経路が OS 非依存なので NetBSD/x86_64 では起きない（実測でも
x86_64 は正常にビルド・E2E 成功）。

**これは B-55（wasm）とは無関係の既存問題**であり、`http3`（quiche）を外しても
解消しない: `boring` は NetBSD/OpenBSD 向け `[target.*.dependencies]` に
**無条件依存**として入っているため、どの feature 構成でもリンク対象になる
（実測で確認済み）。つまり **NetBSD/aarch64 はこれまで一度もリンクに成功して
いなかった**（F-140 の実 VM 検証が未実施だったため顕在化していなかった）。

## 回避策（適用済み）

`OPENSSL_STATIC_ARMCAP` を定義すると BoringSSL は実行時 CPU 機能検出を行わず
`OPENSSL_cpuid_setup` を参照しなくなる。NEON は ARMv8 で必須なので
`OPENSSL_STATIC_ARMCAP_NEON` を静的に立てる:

```
CFLAGS_aarch64_unknown_netbsd='-DOPENSSL_STATIC_ARMCAP -DOPENSSL_STATIC_ARMCAP_NEON'
```

`tools/qemu/bsd-vm.sh` の `_guest_env_prefix()` に netbsd × aarch64 のときだけ
付与するよう組み込み済み（build / e2e の両方に効く）。これで
`full-netbsd`（wasm・http3 込み）のリリースビルドが成功することを実機で
確認した。

## トレードオフ

AES/PMULL/SHA2 等の ARMv8 暗号拡張の**実行時検出を行わなくなる**ため、それらを
使う最適化経路が無効になり NetBSD/aarch64 の TLS/QUIC 暗号処理は遅くなる
（正しさには影響しない）。必要なら `OPENSSL_STATIC_ARMCAP_AES` 等をターゲット
CPU が確実に対応している前提で追加する余地はあるが、配布バイナリでは安全側に
倒している。

## 本筋の対応（upstream）

BoringSSL に NetBSD 向けの `OPENSSL_cpuid_setup` 実装（NetBSD の
`sysctl machdep.cpuN.*` / `aarch64_id_aa64isar0` 相当の参照、あるいは既存の
sysreg 経由実装の流用）を入れるのが本来の解。boring / quiche のバージョン
更新時には本回避策が不要になっていないか確認すること。

## 関連

- B-55（wasmtime が BSD の一部プラットフォームをサポートしていない件。本チケットは
  B-55 の検証中に発見）
- F-140（NetBSD 対応。本チケットの事象は F-140 の実機検証から得られた知見）
