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

## 修正

`docker/Dockerfile.freebsd` で **cc ビルダを明示的に強制**する。

```dockerfile
ENV AWS_LC_SYS_CMAKE_BUILDER=0
```

`get_builder` は `AWS_LC_SYS_CMAKE_BUILDER` が明示指定されていればその値に従う
（`Some(false)` → cc ビルダ）ため、cmake へのフォールバックを回避できる。
cc ビルダは `cc` クレート経由で `.S` を `zig cc` に渡すので正しく組み立てられる。

## 影響範囲

- **Docker クロスビルド（`x86_64-unknown-freebsd`）のみ**の問題。
- FreeBSD **VM 内のネイティブビルド**（`tools/qemu/bsd-vm.sh freebsd <arch> build`）は
  ネイティブ clang でビルドされるため影響しない。`aarch64-unknown-freebsd` は
  Rust Tier 3 でそもそも VM ネイティブビルド専用。

## 関連

- B-47（クロスビルド環境整備。本件の検出元）
- B-48（同じ FreeBSD クロスビルドで検出した `--features aio` のモジュールパス誤り）
- F-131（クロスプラットフォーム TLS）
