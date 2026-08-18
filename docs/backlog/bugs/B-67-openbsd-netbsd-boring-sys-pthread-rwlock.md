# B-67: OpenBSD / NetBSD が boring-sys のビルド失敗で**一切ビルドできない**

**優先度**: P1（OpenBSD・NetBSD の全ビルド・E2E・パッケージングがブロックされる）
**ステータス**: 未修正（原因特定済み・回避策は不成立）
**発見日**: 2026-08-18（F-158 の作業中、OpenBSD E2E を回そうとして判明）
**関連**: F-136（quiche の boringssl-boring-crate 化）、B-55（wasmtime vendoring）

---

## 事象

OpenBSD 7.9 aarch64 で `cargo build` が `boring-sys v4.22.0` のビルドで失敗する。

```
error: failed to run custom build command for `boring-sys v4.22.0`
  .../boringssl/src/include/openssl/thread.h:81:9:
      error: unknown type name 'pthread_rwlock_t'
```

**`http3` feature を外しても再現する。** `boring = "4.3"` は
[Cargo.toml:498](../../../Cargo.toml) で **OpenBSD/NetBSD 共通の無条件依存**として
宣言されている（`optional` ではない）ため、feature 構成に関わらず必ずビルドされる。

したがって **OpenBSD と NetBSD は現在いかなる feature 構成でもビルドできない。**
これにより次がすべてブロックされる:

- OpenBSD / NetBSD の E2E（`tests/e2e_setup.sh`）
- OpenBSD / NetBSD の packaging（`--profile dist`）
- `tools/qemu/bsd-vm.sh openbsd|netbsd <arch> build`

## 原因

BoringSSL の `include/openssl/thread.h` が非 glibc 環境で
**`<pthread.h>` を include せずに `pthread_rwlock_t` を使っている**:

```c
#elif defined(OPENSSL_WINDOWS)
  typedef union crypto_mutex_st { void *handle; } CRYPTO_MUTEX;
#elif !defined(__GLIBC__)
  typedef pthread_rwlock_t CRYPTO_MUTEX;   // ← 81 行目。OpenBSD はここに入る
#else
  // glibc では pthread_rwlock_t が feature flag に隠れているため
  // 十分なサイズのパディング構造体を使う（static_assert で担保）
#endif
```

glibc 側は「feature flag に隠れている」ことを理由にパディング構造体で回避しているが、
**非 glibc 側は `pthread_rwlock_t` が可視である前提**になっている。

OpenBSD の `/usr/include/pthread.h:120` は
`typedef struct pthread_rwlock *pthread_rwlock_t;` を**無条件で**定義しており、
feature-test マクロによる隠蔽は**していない**。つまり原因は可視性マクロではなく、
**単に `thread.h` が `<pthread.h>` を include していないこと**である。

## 試した回避策（すべて失敗）

| 回避策 | 結果 |
|---|---|
| `CFLAGS=-D_BSD_SOURCE` | 失敗（可視性マクロの問題ではないため当然） |
| `CFLAGS=-std=gnu11` | 失敗（同上） |
| `CFLAGS='-D_POSIX_C_SOURCE=200809L -D_BSD_SOURCE'` | 失敗（同上） |
| `CFLAGS='-include pthread.h'` | **悪化**。`boring-sys` に加えて `ring` と `zstd-sys` のビルドまで壊れた（`CFLAGS` は全 C 依存クレートに一律に効くため、アセンブリを含むクレートが巻き添えになる） |

**グローバル `CFLAGS` による回避は原理的に不適切**である。効かせたいのは
boring-sys だけだが、cargo の `CFLAGS` はターゲット全体に効く。

## 推奨する修正

`third_party/wasmtime`（B-55）と同じ **vendoring 方式**が本命:

1. `boring-sys` を `third_party/` へ vendoring し、
   `thread.h` の非 glibc 分岐の直前に `#include <pthread.h>` を追加する
   （または当該分岐を glibc と同じパディング構造体方式に寄せる）。
2. Cargo の `[patch]` かパッケージ名変更 + ターゲット別依存で差し替える。

代替案として、**OpenBSD/NetBSD の `boring` 依存を本当に無条件にする必要があるかを
再検討する**価値がある。`src/http3_server.rs` が `boring::ssl::SslContextBuilder` を
直接使うのは `http3` 有効時だけなので、`http3` を切れば `boring` が要らない構成に
できるなら、`http3` 無効の OpenBSD/NetBSD ビルドだけでも救える
（Cargo の feature がターゲット非依存であることが障害になっている旨は
Cargo.toml のコメントに記載があるが、`dep:boring` をターゲット別 feature で
表現できないか再検討する）。

## いつから壊れたか

- `boring-sys 4.22.0` は **2026-07-22 の `ef669e9`** から Cargo.lock に固定されている。
- OpenBSD VM 内の最後の成功ビルド成果物は **2026-08-09 04:40** の
  `/usr/obj/veil-proxy/target/release/veil`。

つまり **同じ boring-sys 4.22.0 で 8/9 には成功していた**。
Cargo.lock はその後 `9be1c6f`（B-55）でしか変更されていないため、
**VM 側の環境ドリフト（OpenBSD 7.9 のコンパイラ/ヘッダ更新）が引き金**と考えられる。
再現条件を確定するには、8/9 時点のツールチェーンとの差分を確認する必要がある。

## F-158 との関係（無関係であることの根拠）

本件は F-158（HTTP/2 インライン初回 poll）とは**完全に無関係**である:

1. F-158 の変更は `src/` 内の純粋な Rust のみで、`Cargo.toml` / `Cargo.lock` を
   **1 行も変更していない**（`git diff b239dc1~1 -- Cargo.toml Cargo.lock` が空）。
2. 失敗しているのは**依存クレートの C ビルドスクリプト**であり、
   veil 自身のコンパイルより前段で止まっている。
