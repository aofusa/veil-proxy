# B-89: FreeBSD で単体テストが SIGSEGV（tikv-jemalloc が libthr の malloc フックを上書き）＋静的配信の封じ込め 2 件

## 事象

BSD の VM で**初めて単体テスト（`--lib`）を回した**（これまで BSD は E2E のみ）ところ、
FreeBSD 14.3 aarch64 で `cargo test --lib --features full-freebsd` が
**SIGSEGV でプロセスごと落ちた**。`--test-threads=1` では落ちず、2 件の失敗だけが残った。

### 1. SIGSEGV（並列実行時、ほぼ全モジュールで再現）

core を lldb で見ると、新しいスレッドの開始時（`pthread_setname_np` の `strdup`）に
**libc 内蔵の jemalloc** が `tcache_arena_associate` で落ちている。

`full-freebsd` は `jemalloc` feature（tikv-jemallocator を global allocator にする）を
含んでいた。**FreeBSD 向けにビルドされた jemalloc は libthr のフック
`_malloc_thread_cleanup` / `_malloc_prefork` / `_malloc_postfork` をプレフィックス無しで
定義する**（`nm` で実行ファイル側に `T _malloc_thread_cleanup` 等があることを確認）。
これらは libc の同名シンボルを上書きするため、libthr はスレッド終了時に libc 側ではなく
tikv 側のフックを呼ぶ。**libc の jemalloc はスレッドキャッシュを片付けられず、解放済みの
TLS を指したままアリーナのリストに残り**、次のスレッド生成でそれを辿って落ちる。

本番バイナリも同じ構成だった。veil はワーカー・offload・リロード等でスレッドを生成するため、
スレッドの終了と生成が重なると同じ破壊が起こり得る（E2E のサーバはスレッドの出入りが
少なく表面化していなかった）。fork 前後のフックも同様に libc 側が呼ばれていなかった。

**修正**: FreeBSD の `malloc(3)` は元から jemalloc なので、`full-freebsd` から `jemalloc` を
外してシステムアロケータを使う。再発防止に `jemalloc` feature を FreeBSD で有効化すると
`compile_error!` にする。修正後は並列実行を 5 回連続で全通過（修正前は 3 回中 3 回 SIGSEGV）。

### 2. ルート内を指す絶対シンボリックリンクが 404 になる（`allows_symlink_within_root`）

FreeBSD の `O_RESOLVE_BENEATH` は、Linux の `RESOLVE_BENEATH`（`EXDEV`）と同じく、
**リンク先がルート内でも絶対シンボリックリンクを `ENOTCAPABLE` で拒否する**。
Linux（F-153）はこの場合だけそのリクエスト限りで `canonicalize()` + 含有チェックへ
フォールバックするが、FreeBSD の経路（`security::capsicum::stat_static` /
`open_static_ro`）は `ENOTCAPABLE` を最終判定として扱っていたため、
`current -> /srv/www/releases/vNNN` 型のデプロイが 404 になっていた。

**修正**: capability mode の外では `ENOTCAPABLE` のとき `None` を返して従来経路で再判定する
（`retry_outside_capmode`）。capability mode 内は絶対パスの `realpath` が使えないため従来どおり拒否。

### 3. per-route 封じ込め検査が FreeBSD で常に「許可」だった（`forbidden_outside_route_containment_does_not_read_body`）

`cache::sendfile_base_contains` は FreeBSD で `static_serving_active()` の間、比較を
省略して常に `true` を返していた（dirfd 経路では生パスが返るため canonical 形と比較できない、
という理由）。しかし**登録済みルートの外にあるパスや 2. の再判定で従来経路へ落ちたパス**も
同じ検査を通るため、ディレクトリルートのクロスルート封じ込め（B-65）が FreeBSD では効いて
いなかった。2. の修正と組み合わさるとルート外を指すシンボリックリンクが素通りになり得る。

**修正**: 省略をやめ、FreeBSD では「canonical の base 配下」**または**「生の base_path 配下」
を許可条件にする。dirfd 経路の生パスはカーネルが封じ込めを保証済み、従来経路のパスは
シンボリックリンクを解決済みの canonical 形なので、生の base_path に一致するのはルート内に限られる。

## 検証

FreeBSD 14.3 aarch64（QEMU/HVF）で単体 935・統合 54 が全通過（並列 5 回連続）。
Linux の単体テストは無影響（変更はすべて `cfg(target_os = "freebsd")` または FreeBSD の feature セット）。

## 教訓

E2E は `tests/e2e_tests.rs` しか走らせないため、BSD でライブラリの単体テストが壊れていても
気づけなかった。`tools/qemu/bsd-vm.sh <os> <arch> unit` を追加し、BSD でも単体テストを回す。
