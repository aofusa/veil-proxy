# B-62: NetBSD/aarch64 実機で E2E テストクライアントの quinn が panic → プロセス abort し、フル E2E スイートが完走しない

**状態: 対応済み（2026-08-29）。根本原因は libc クレート側にあり upstream 報告が残件**

## 事象

NetBSD 10.1 evbarm-aarch64（実機、QEMU/HVF on Apple Silicon）で E2E テストクレートを
実行すると、HTTP/3 クライアントとして使っている dev-dependency の `quinn` 0.11.9 /
`quinn-udp` 0.5.14 が `quinn-udp-0.5.14/src/cmsg/mod.rs:86` で panic する。

```
panicked at quinn-udp-0.5.14/src/cmsg/mod.rs:86:
assertion `left == right` failed
  left: 20
 right: 16
```

panic 後、`quinn::Endpoint` の `Drop` 実装内で `PoisonError` を `unwrap()` しており、
**デストラクタ内で二重 panic → プロセス abort（SIGABRT）**になる。この結果、
**E2E テストバイナリ全体が途中で落ちる**。

## 根本原因（2026-08-29 に特定）

**`libc` クレートの NetBSD/aarch64 向け `_ALIGNBYTES` の定義が誤っている。**

```
libc-0.2.189/src/unix/bsd/netbsdlike/netbsd/aarch64.rs:68
    pub(crate) const _ALIGNBYTES: usize = size_of::<c_int>() - 1;   // = 3（誤り）
```

NetBSD の `<aarch64/param.h>` の `ALIGNBYTES` は `sizeof(__register_t) - 1` で、
aarch64 では **7**（8 バイト境界）である。同じ libc の x86_64 側は
`size_of::<c_long>() - 1` と正しく定義されている。

この 4 バイトのずれにより

- `CMSG_LEN(4)` = `_ALIGN(sizeof(cmsghdr)=12) + 4` が **16**（正しくは 20）
- `CMSG_DATA` が返すペイロード位置も 4 バイト手前

となり、カーネルが返す cmsg（`len = 20`）と食い違う。失敗した assert の
`left: 20`（カーネル）/ `right: 16`（libc 計算）はこの差そのものである。

`quinn-udp` は `libc::CMSG_LEN` / `CMSG_DATA` / `CMSG_SPACE` へ委譲しているだけなので
**quinn-udp に非は無い**。debug_assert が正しくずれを検出している。

## veil 本体の問題ではない

veil が cmsg を使うのは `src/runtime/uring/udp_recv.rs`（io_uring 経路）**のみ**で、
これは **Linux 専用**である。NetBSD では kqueue readiness reactor 経路を通り
cmsg を扱わないため、**この libc の不備の影響を受けない**。

NetBSD **x86_64** では同じテストクレートが完走する（`_ALIGNBYTES` が正しいため）。

## 対処（実施済み）

テスト名による `--skip http3 --skip h3` では**取りこぼす**。
`test_alt_svc_upgrade_flow` のように名前に http3/h3 を含まない HTTP/3 テストがあり、
実機で試したところ依然 SIGABRT になった。

そこで **feature を落として HTTP/3 テストとクライアントをコンパイル対象から外す**:

- `Cargo.toml` に `full-netbsd-no-http3` を追加（`full-netbsd` から `http3` を除いたもの。
  `grpc-full` は `http3` を含むので `grpc` + `grpc-web` に置き換える）
- `tests/common/http3_client` を `#[cfg(feature = "http3")]` でゲート
- ゲートが漏れていた HTTP/3 テスト 4 件と定数 `PROXY_HTTP3_PORT` にゲートを追加
- `tools/qemu/bsd-vm.sh` の **e2e 段だけ** NetBSD/aarch64 でこのセットを使う

**配布バイナリのビルド（`build`/`fetch`）は `full-netbsd` のまま**で、
成果物の HTTP/3 は有効である。

### 実機結果

| | 対処前 | 対処後 |
|---|---|---|
| NetBSD/aarch64 E2E | SIGABRT で **1 件も完走せず** | **421 passed / 0 failed**（43.19s） |

HTTP/3 系テストはこのプラットフォームでは実行されない（コンパイル対象外）。

## 残件（本筋の対応）

- **libc クレートへ upstream 報告**する（`netbsd/aarch64.rs` の `_ALIGNBYTES` を
  `size_of::<c_long>() - 1` へ）。修正が入れば `full-netbsd-no-http3` は不要になる。
- libc をローカルで vendoring して直す案は見送った。libc は veil 本体の依存でもあり、
  `[patch.crates-io]` は全プラットフォームのビルドに波及する。**テスト専用の不具合の
  ために配布バイナリ全体の依存を差し替えるのは割に合わない**と判断した。

## 関連

- B-59（NetBSD/aarch64 の BoringSSL リンクエラー。本チケットの発見は同じ実機検証で得た）
- B-60（NetBSD の PaX MPROTECT による WASM 実行不能）
- F-140（NetBSD 対応）

## 解消（2026-10-09、feat/v080-limitations / F-176）

libc の修正を待たず、テスト用 HTTP/3 クライアントの依存 `quinn-udp` を vendoring し（`third_party/quinn-udp`）、
NetBSD/aarch64 だけ正しいアラインメント（`long` 由来の 7）で CMSG マクロを実装した。`full-netbsd-no-http3`
と、`bsd-vm.sh` の切り替えを削除した。NetBSD 10.1 aarch64（Apple Silicon + HVF）で `full-netbsd` の E2E が
**565/565 成功**（HTTP/3 を含む。従来は HTTP/3 抜きの 429 件のみ）。
