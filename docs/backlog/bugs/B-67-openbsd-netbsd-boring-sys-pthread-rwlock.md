# B-67: 【誤起票・取り下げ】OpenBSD / NetBSD がビルドできない

**優先度**: —
**ステータス**: **取り下げ（Invalid）**。事象は実在せず、**起票者の実行手順の誤り**だった。
**起票日**: 2026-08-18 / **取り下げ日**: 2026-08-22

---

## 結論: OpenBSD / NetBSD は壊れていない

当初「`boring-sys v4.22.0` の vendored BoringSSL が
`thread.h:81: unknown type name 'pthread_rwlock_t'` で失敗し、
OpenBSD/NetBSD は全 feature 構成でビルド不能」と P1 で起票したが、**これは誤りである。**

**この問題は既知であり、`tools/qemu/bsd-vm.sh` に回避策が実装済みだった。**
実際 `packaging/output/` の成果物には成功記録が残っている:

```
target      : aarch64-unknown-openbsd
built on OS : openbsd 7.9
built at    : 2026-08-17T21:55:19Z
rustc       : rustc 1.94.1
```

同じ OS バージョン・同じ `Cargo.lock`（**Cargo.toml/lock は 2026-08-09 以降未変更**）で
5 日前に成功している。「環境ドリフトで壊れた」という当初の推測も誤りだった。

## 実際の原因: `bsd-vm.sh build` を迂回して直接 `cargo build` を叩いた

`tools/qemu/bsd-vm.sh` の `_guest_env_prefix()` は、OpenBSD/NetBSD のビルドに
**必須の環境変数**を組み立ててから `cargo build` を実行する:

| 環境変数 | 役割 |
|---|---|
| `CC_{x86_64,aarch64}_unknown_openbsd=/usr/local/bin/veil-cc` | **本件の回避策そのもの**（下記） |
| `BINDGEN_EXTRA_CLANG_ARGS_*_openbsd='-include pthread.h'` | bindgen は cc ラッパを経由しないため別途必要 |
| `CFLAGS_aarch64_unknown_{openbsd,netbsd}='-DOPENSSL_STATIC_ARMCAP …'` | B-59（aarch64 の `OPENSSL_cpuid_setup` 未定義リンクエラー） |
| `CARGO_HOME=/usr/obj/cargo` | OpenBSD の `/` は ~628M しかなく既定の CARGO_HOME が溢れる |
| `LIBCLANG_PATH=…` | bindgen |
| `RUSTFLAGS='-L /usr/local/lib'` | libstdc++ 互換リンク |
| `PATH=/usr/pkg/bin:…`（NetBSD） | pkgsrc の rust/cmake/llvm |

起票者は「`bsd-vm.sh <os> <arch> ssh` が stdin を転送しない」問題を回避するため
直接 `ssh root@127.0.0.1` へ切り替えた際、**ソース転送だけでなくビルドまで
直接 ssh で実行してしまい、この env prefix を丸ごと失った。**

## 既存の回避策（`veil-cc` ラッパ）

`bsd-vm.sh` の `toolchain` が OpenBSD ゲストへ設置する:

```sh
#!/bin/sh
# BoringSSL(boring-sys) は pthread_rwlock_t が <sys/types.h> から見える前提だが、
# OpenBSD では <pthread.h> にしかない。C ファイルのときだけ pthread.h を先に読ませる。
# アセンブリ(.S/.s) には付けない（付けると zstd-sys 等のアセンブルが壊れる）。
for a in "$@"; do
  case "$a" in
    *.S|*.s) exec /usr/bin/cc "$@" ;;
  esac
done
exec /usr/bin/cc -include pthread.h "$@"
```

起票者は回避策として `CFLAGS='-include pthread.h'` を試し、
**`ring` と `zstd-sys` のアセンブルまで壊れて「悪化」と結論づけた**が、
その失敗こそが `veil-cc` ラッパが存在する理由そのものだった
（`.S`/`.s` を除外すれば正しく動く）。既存の解を再発見しかけて取り逃していた。

## 教訓

- **BSD ゲストのビルドは必ず `tools/qemu/bsd-vm.sh <os> <arch> build` を使う。**
  直接 `ssh` で `cargo build` を叩いてはならない（必須 env を失う）。
  `bsd-vm.sh ssh` の stdin 非転送を回避して直 ssh にする場合も、
  **迂回してよいのはファイル転送だけで、ビルド実行は迂回しないこと。**
- **「ビルドが壊れた」と結論する前に、`packaging/output/` の `BUILD_INFO.txt` で
  直近の成功時刻・OS・rustc を確認する**（本件は 5 日前の成功記録が残っていた）。
- **既存のビルドスクリプトに同じエラーメッセージが書かれていないか grep する。**
  本件は `bsd-vm.sh` に `unknown type name 'pthread_rwlock_t'` が
  コメントとして明記されていた。
