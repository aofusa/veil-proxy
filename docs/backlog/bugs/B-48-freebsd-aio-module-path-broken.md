# B-48: `--features aio`（FreeBSD POSIX AIO、F-127）がモジュールパス誤りでコンパイルできない

## 事象

FreeBSD 向けに `aio` フィーチャーを有効にしてビルドすると、`veil` の lib が
**12 個のコンパイルエラー**で失敗する。

```
error[E0433]: failed to resolve: could not find `aio` in `super`
   --> src/runtime/reactor/tcp/unix.rs:382:55
382 |     pub fn read<T: IoBufMut>(&self, buf: T) -> super::aio::AioReadFuture<T> {
    |                                                       ^^^ could not find `aio` in `super`
help: consider importing this module
 14 + use crate::runtime::reactor::aio;
```

に続いて、`read`/`write` の戻り値型が `{type error}` になることで
`src/http3_stream.rs` / `src/l4/proxy.rs` の呼び出し側に
`error[E0282]: type annotations needed` が 6 件連鎖する。

## 原因

`aio` モジュールは `crate::runtime::reactor::aio`（`src/runtime/reactor/mod.rs` の
`#[cfg(veil_aio)] pub(crate) mod aio;`）にある。一方、参照している
`src/runtime/reactor/tcp/unix.rs` は `crate::runtime::reactor::tcp::unix` なので、
`super::aio` は `crate::runtime::reactor::tcp::aio` を指してしまい解決できない。

F-127 で AIO 経路を書いた時点では TCP 実装が `reactor/tcp.rs`（`reactor` 直下の
単一ファイル）だったため `super::aio` が正しかった。その後 F-125（Windows 対応）で
`reactor/tcp/{mod,unix,windows}.rs` へ分割された際に、**`veil_aio` は既定オフで
CI・ローカルのどのビルドでも有効にならなかったため誰も気付かなかった**。

つまり `--features aio` は F-125 以降ずっと壊れていた（`veil_aio` を立てる構成が
存在しなかったため顕在化しなかった）。

## 検出経緯

B-47 の作業で packaging の BSD 向け feature セット `full-freebsd`
（`full` + jemalloc + **`aio`**）を新設し、`docker/Dockerfile.freebsd` で
`x86_64-unknown-freebsd` を Docker クロスビルドしたときに初めて顕在化した。

## 修正

`super::aio::` → `crate::runtime::reactor::aio::`（6 箇所、`src/runtime/reactor/tcp/unix.rs`）。
E0282 群は E0433 に起因する連鎖エラーなので、これだけで解消する。

## 再発防止

`full-freebsd` feature セットが `aio` を含むようになったため、
`packaging/scripts/build-cross.sh --target freebsd` /
`tools/qemu/bsd-vm.sh freebsd <arch> build` を通すたびに AIO 経路がコンパイルされる。

## 追記（2026-08-15）

`docker/Dockerfile.freebsd` および `packaging/scripts/build-cross.sh --target freebsd`
は B-49（未解決）により削除した。本件を最初に顕在化させた Docker クロスビルド経路
自体が無くなったため、上記「検出経緯」「再発防止」に書いた
`packaging/scripts/build-cross.sh --target freebsd` は現在は存在しない
（過去の検出経緯としてはそのまま残す）。修正済みの `super::aio::` →
`crate::runtime::reactor::aio::` はソースコードの変更であり Docker 削除の影響を
受けないため、本チケットのステータス（完了）は変わらない。`full-freebsd`
（`aio` を含む）のコンパイルは引き続き `tools/qemu/bsd-vm.sh freebsd <arch> build`
（QEMU VM 内ネイティブビルド）を通すたびに検証される。

## 関連

- F-127（FreeBSD POSIX AIO）
- F-125（`reactor/tcp` の Windows 対応分割 = 混入時期）
- B-47（本件の検出元。クロスビルド環境整備）
- B-49（FreeBSD Docker クロスビルドは 2026-08-15 に経路ごと削除。詳細は B-49 参照）
