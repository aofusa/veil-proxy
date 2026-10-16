# F-232: コード監査の第 1 段（ホットパスの重複検索・古いコメント・`allow(dead_code)`・UDP ソケットの重複と SAFETY）

**状態: 完了（feat/v080-release、2026-10-16）**

## 目的

監査の報告の Phase 1（危険が小さく効果が大きいもの）を片付ける。

報告は mesh の実装（F-215〜F-225）より前の main を読んで書かれている。そのため 2026-10-16 の main で全項目を確かめ直し、
今も当てはまるものだけを対象にした（下の表の「今の main」）。

## 項目

| # | 報告の指摘 | 今の main（2026-10-16） | やること |
|---|---|---|---|
| 1 | `drive_h2_streams` で同じ `sid` を最大 9 回 `streams.get_mut(&sid).unwrap()` | 8 回（`src/proxy.rs` の `drive_h2_streams`） | 下の「1 の設計」 |
| 2 | `reactor::executor::register` の doc が実装と逆 | そのまま。doc は「fd あたり `epoll_ctl` は生涯 1 回・`EPOLLET`」、本体は B-75 の「毎回 `EPOLL_CTL_MOD`・`EPOLLONESHOT`」 | doc を本体と B-75 に合わせて書き直す（本体のコメントが正） |
| 2 | `http3_server.rs` の冒頭に monoio の記述 | そのまま（1・3・8 行目） | 今の独自ランタイム（`runtime::udp`・io_uring / reactor）の説明にする。ほかのファイルにも `monoio` の記述が残っていないか、`git grep -n monoio -- src` で全部見る |
| 3 | `allow(dead_code)` が 5 箇所 | 無条件の `#[allow(dead_code)]` が 5 箇所: `src/udp/socket.rs` 2・`src/runtime/uring/udp_recv.rs` 2・`src/stream_channel.rs` 1。報告に無い `#[cfg_attr(not(feature = ..), allow(dead_code))]` も 19 箇所ある（`config.rs`・`metrics.rs`・`pool.rs`・`http_utils.rs` 等） | 下の「3 の設計」 |
| 3 | 理由の無い `#[allow(clippy::disallowed_methods)]` が約 15 箇所 | 行末に理由の無いものが 147 箇所（テストを含む。直前の行に理由がある場合を含むので、実数は台本で数える） | 理由の無いものだけ、1 行の理由を付ける |
| 4 | `udp/socket.rs` の `bind_reuseport` と `bind_reuseport_with_gso` が約 110 行重複・`SO_REUSEADDR` の戻り値を見ていない | そのまま（Linux 版 115・234 行目。他 OS 版にも同じ組がある） | `bind_reuseport(addr)` は `bind_reuseport_with_gso(addr, true)` に委ねる。`setsockopt` の失敗を全部見る |
| 4 | `unsafe` に `// SAFETY:` がほぼ無い | `udp/socket.rs` 71 箇所に 2、`runtime/udp.rs` 48 に 0、`reactor/tcp/unix.rs` 76 に 2、`uring/tcp.rs` 58 に 7 | 4 ファイルの全 `unsafe` ブロックに前提条件を書く。新しく `clippy::undocumented_unsafe_blocks` をこの 4 ファイルで `warn` にし、戻りを防ぐ |

## 1 の設計（`drive_h2_streams` の再検索）

- 再検索の大半は `.await`（`conn.flush_write_buf()` など）をまたいでいる。`conn` と `streams` は別の変数なので、`st` を
  1 回だけ束縛してもコンパイラは通す。
- ただし、ループの中で `streams` から要素を取り除く経路があると、束縛が無効になる。そこで、ストリームを処理する本体を
  `fn` に切り出し、引数で `&mut StreamState` を受ける形にする。取り除く判定は戻り値で返し、呼び出し側が行う。
  こうすると借用の範囲が型で決まる。
- 挙動は変えない。`unwrap` の panic の可能性が無くなるのは副次的な効果で、今の `unwrap` は「直前に keys から取った
  `sid`」なので実際には panic しない。
- HashMap の検索 8 回はストリームの数が少ないので小さい。効果は交互 A/B で確かめ、主張は「重複の解消」にとどめる
  （性能の改善とは言わない。差が出たときだけ書く）。

## 3 の設計（`allow(dead_code)`）

AGENTS.md の「`allow(dead_code)` は禁止」に合わせて、`cfg_attr` の形も含めて 0 にする。

- **FFI のバッキングストア**（`udp/socket.rs`・`udp_recv.rs`）: 生ポインタの先を保つためだけのフィールドである。
  `_iovs` のように `_` で始まる名前にするか、`PhantomPinned` と一緒の構造体に入れる。`_` で始まるフィールドは dead_code の
  警告が出ない。
- **`stream_channel.rs` の `Closed(T)`**: 値を返す API があればそれを使い、無ければ `Closed` の中身を持たない形にする。
- **`cfg_attr(not(feature))` の 19 箇所**: その feature のときだけ使う項目である。項目そのものに `#[cfg(feature = ..)]` を
  付ける（AGENTS.md の「プラットフォーム限定関数だけが使う定数にも同じ cfg を付ける」と同じ考え方）。
  - 設定の構造体のフィールドは、serde の互換のために feature が無くても読む必要がある（TOML のキーを拒否しない）。
    この場合は、`#[cfg]` で消すとキーを拒否するようになる。そこで、feature が無いときは「無視した」と警告を出す処理で
    読む（非対象 OS のキーと同じ扱い）。
- CI の clippy は、`--no-default-features`・default・`http2` だけ・`http3` だけ・`full`・`full,epoll` の全部で警告 0 を確かめる。

## 進め方

項目ごとに 1 コミットにする。2・3・4 はホットパスの機械語を変えない。

- 1 と 4 は `suite_ab.sh` で交互 A/B を行う。4 は UDP（HTTP/3）なので、http3 の行も見る。
- 4 の UDP ソケットは、Linux・FreeBSD・OpenBSD・NetBSD・macOS・Windows の別実装がある。各 OS の版を同じ形に揃え、
  `packaging/scripts/build-cross.sh` と BSD の VM（`tools/qemu/`）でビルドとテストを回す。

## 受け入れ条件

- （必須）既存のプロキシの性能が退行しない（`tools/perf/suite_ab.sh` の main との交互 A/B、比の中央値 0.98 以上・0.95 未満なし。
  両バックエンド、3B と 54KB、http3 を含む）。
- 次の検査が 0 件になる。
  - `git grep -nE 'allow\(dead_code\)' -- src crates`
  - `git grep -n monoio -- src`
- `cargo test`（単体・統合）、E2E（io_uring・epoll）、clippy の全 feature の組み合わせが通る。
- BSD・macOS・Windows のクロスビルドが通る。


## 実装の結果

- `drive_h2_streams` の再検索 9 回 → 1 回（`let Some(st) = streams.get_mut(&sid) else { continue }`）。
- `register` の doc を B-75 の実装に合わせた。`src/` の monoio の記述 67 箇所を消した（`git grep monoio -- src` は 0）。
- `allow(dead_code)` は 0（`git grep -nE 'allow\(dead_code\)' -- src crates` はコメントだけ）。
  `[http2]` は http2 feature 無しのとき生の値で受けて警告する。
- UDP: `bind_reuseport` の重複をやめ、`SO_REUSEADDR` の失敗を見る。`close` の前に errno を読む（12 箇所）。
- 4 ファイル（`udp/socket.rs`・`runtime/udp.rs`・`reactor/tcp/unix.rs`・`uring/tcp.rs`）の全 `unsafe` に SAFETY を付け、
  `#![warn(clippy::undocumented_unsafe_blocks)]`。理由の無い `disallowed_methods` の allow に理由。

## 関連

F-233、F-234、B-75、F-166、AGENTS.md の「コーディング規約」。
