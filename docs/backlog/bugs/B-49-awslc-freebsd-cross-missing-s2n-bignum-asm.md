# B-49: FreeBSD クロスビルドで aws-lc-sys の s2n-bignum アセンブリが組み立てられずリンクに失敗する

## 事象

`docker/Dockerfile.freebsd`（`cargo zigbuild --target x86_64-unknown-freebsd`、
`--features full-freebsd`）で veil をリンクする段階で、`aws-lc-sys` 由来の
**s2n-bignum のシンボルが大量に未定義**になる。

```
ld.lld: error: undefined symbol: curve25519_x25519_byte
ld.lld: error: undefined symbol: curve25519_x25519base_byte
ld.lld: error: undefined symbol: edwards25519_scalarmulbase
ld.lld: error: undefined symbol: edwards25519_decode
ld.lld: error: undefined symbol: bignum_madd_n25519
ld.lld: error: undefined symbol: bignum_mod_n25519
ld.lld: error: undefined symbol: bignum_neg_p25519
（他多数）
```

Linux / macOS / Windows のクロスビルドでは発生しない。FreeBSD ターゲット固有。

## 原因

2 つの条件が噛み合って発生する。

1. **aws-lc の C 側は FreeBSD でも s2n-bignum を「使える」と判断する。**
   `aws-lc/crypto/fipsmodule/curve25519/internal.h`:

   ```c
   #if ((defined(OPENSSL_X86_64) && ...) || defined(OPENSSL_AARCH64)) && \
       (defined(OPENSSL_LINUX) || defined(OPENSSL_APPLE) ||             \
        defined(OPENSSL_OPENBSD) || defined(OPENSSL_FREEBSD) ||         \
        defined(OPENSSL_NETBSD)) &&                                     \
       !defined(OPENSSL_NO_ASM)
   #define CURVE25519_S2N_BIGNUM_CAPABLE
   #endif
   ```

   `OPENSSL_FREEBSD` が立つため、C 側はこれらのアセンブリ実装を**呼ぶ**。

2. **一方で `.S` が 1 つも組み立てられていない。**
   `aws-lc-sys` のビルダ選択（`builder/main.rs` の `get_builder`）は、
   `bindgen` フィーチャーが有効（= `is_bindgen_required()` が真。veil は
   `aws-lc-sys = { features = ["ssl", "bindgen"] }` を指定している）だと
   **cc ビルダを試さずに cmake ビルダへ落ちる**。
   その cmake ビルダが FreeBSD クロス構成では ASM 言語を有効化できず、
   `builder/cc_builder/linux_x86_64.rs` が列挙している
   `third_party/s2n-bignum/s2n-bignum-imported/x86_att/curve25519/*.S` などが
   ビルド対象に入らない。

   結果、「C は呼ぶが実体が無い」状態になりリンクで落ちる。

### 切り分け

`zig cc` 自体は FreeBSD ターゲット向けに `.S` を正しく組み立てられることを実測で確認した
（つまり zig 側の制約ではない）。

```
$ zig cc -target x86_64-freebsd-none -c .../x86_att/curve25519/bignum_madd_n25519.S -o f.o
$ nm f.o
0000000000000000 T bignum_madd_n25519      # ← linux ターゲットと同一に定義される
```

## 試したこと（いずれも解決せず）

| 試行 | 結果 |
|---|---|
| `AWS_LC_SYS_CMAKE_BUILDER=0`（cc ビルダを強制） | **不可**。`panicked: "cc_builder for libssl not supported"`。veil は quiche とシンボルを共有するため `aws-lc-sys = { features = ["ssl"] }` が必須で、cc ビルダは libssl を作れない |
| `CMAKE_SYSTEM_NAME=FreeBSD` を外し cmake-rs に TARGET から導出させる | **効果なし**。同じ undefined symbol 群で失敗 |
| `AWS_LC_SYS_NO_ASM=1` | **不可**。aws-lc-sys が `AWS_LC_SYS_NO_ASM only allowed for debug builds!` で panic する（release ビルドでは使えない） |

補足: 未定義シンボルには `curve25519_x25519_byte` のように
`builder/cc_builder/linux_x86_64.rs` の一覧に**そもそも載っていない**（arm 側にしか
`.S` が無い）ものも含まれる。つまり「x86_64 用 asm が組み立てられなかった」だけでなく、
FreeBSD ターゲットでの C 側のコード選択と aws-lc-sys が用意する asm の対応自体が
噛み合っていない可能性が高い。aws-lc-sys 側の対応が要る。

## 状態: 未解決（保留）

`x86_64-unknown-freebsd` の **Docker クロスビルドは現状できない**。
`docker/Dockerfile.freebsd` と `packaging/scripts/build-cross.sh --target freebsd` は
残してあるが、実行すると上記のリンクエラーで失敗する（スクリプト冒頭で警告を出す）。

**FreeBSD の公式なビルド経路は QEMU VM 内のネイティブビルド**
（`tools/qemu/bsd-vm.sh freebsd {x86_64,aarch64} build`）であり、そちらは
ネイティブ clang が `.S` を組み立てるため本問題の影響を受けない。

### 次に試す価値がある案

- aws-lc-sys を新しめのバージョンへ上げて FreeBSD クロス対応の改善を確認する。
- cmake ビルダへ明示的に `-DCMAKE_ASM_COMPILER` / `enable_language(ASM)` 相当を
  渡せるか（`AWS_LC_SYS_CMAKE_*` 系の追加 env）を調べる。
- FreeBSD の base.txz から sysroot を用意し、zig ではなくネイティブ clang +
  `--sysroot` でクロスする（cmake のアーキ判定が素直に通る可能性）。

## 影響範囲

- **Docker クロスビルド（`x86_64-unknown-freebsd`）のみ**の問題。
- FreeBSD **VM 内のネイティブビルド**（`tools/qemu/bsd-vm.sh freebsd <arch> build`）は
  ネイティブ clang でビルドされるため影響しない。`aarch64-unknown-freebsd` は
  Rust Tier 3 でそもそも VM ネイティブビルド専用。

## 関連

- B-47（クロスビルド環境整備。本件の検出元）
- B-48（同じ FreeBSD クロスビルドで検出した `--features aio` のモジュールパス誤り）
- F-131（クロスプラットフォーム TLS）
