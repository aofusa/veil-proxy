# B-69: Windows が F-155 以降ビルドできない（`accept_batch` が未実装）

**優先度**: P1（Windows の全ビルド・パッケージングがブロックされていた）
**ステータス**: **修正済み**
**発見日**: 2026-08-19（F-158 のパッケージング作業中）
**原因コミット**: `1a23c45`（F-155: FreeBSD の対 nginx パリティ Phase 1〜6）

---

## 事象

`packaging/scripts/build-cross.sh --target windows` が
`veil` 本体のコンパイルエラーで失敗する。

```
error[E0599]: no method named `accept_batch` found for struct `tcp::windows::TcpListener`
  --> src/l4/server.rs:307:58
  --> src/entry.rs:976:54
  --> src/entry.rs:1547:54
error: could not compile `veil` (lib) due to 3 previous errors
```

## 原因

F-155 が nginx の `multi_accept` 相当のバッチ accept（`TcpListener::accept_batch`）を
**readiness reactor バックエンド向け**に導入し、`entry.rs` / `l4/server.rs` の
accept ループをこれに切り替えた。しかし実装したのは
`src/runtime/reactor/tcp/unix.rs` **のみ**で、
`src/runtime/reactor/tcp/windows.rs` には追加されなかった。

Windows は `build.rs` により `veil_rt_reactor` + `veil_poller_wsapoll` が立つ
（`build.rs:93-94`）ため reactor 側の呼び出し側コードをコンパイルするが、
Windows 版 `TcpListener` に当該メソッドが無いためリンク以前にコンパイルが通らない。

**Windows は F-155 以降ずっとビルド不能だった。**
`packaging/output/` の Windows 成果物の日付が **2026-08-12**（F-155 の前）で
止まっていたことと整合する。

## 修正

`src/runtime/reactor/tcp/windows.rs` に unix 版と同じインタフェース・同じ規律で
`accept_batch` を実装した。あわせて **`raw_accept_one` 共通ヘルパを切り出した**。

これは AGENTS.md（F-155）の次の設計制約に従うためである:

> `Accept::poll` と `accept_batch` は `raw_accept_one` を共用し、
> accept4/macOS フォールバック・fd リーク防止の順序を二重管理しない。

Windows 版は従来 `Accept::poll` に accept の作法（`WinSock::accept` →
`FIONBIO` による非ブロッキング化 → `WSAEWOULDBLOCK` 判定）がインライン展開されていた。
これを `raw_accept_one` へ集約し、`Accept::poll` と `accept_batch` の双方が使う形にした
（`FIONBIO` の適用箇所が 1 つになる）。

**fd リーク防止の順序も unix 版に合わせた**: アドレス変換より先に `TcpStream` を
構築するため、`storage_to_sockaddr` が失敗しても `Drop` でソケットが閉じられる。
`accept_batch` は変換失敗の 1 件のみ破棄してバックログの受理を継続する
（`Accept::poll` は即座にエラーを返す）。

## 再発防止に関する所見

**この不具合は単体・統合・E2E のすべてを通過する。** Linux の既定ビルド
（`veil_rt_uring`）はもちろん、`--features epoll`（`veil_rt_reactor` +
`veil_poller_epoll`）でも `windows.rs` は 1 行もコンパイルされないためである。
F-145 が記録した「reactor は Linux 既定ビルドでは未コンパイル＝テストの空白地帯」
という教訓の**さらに一段深い版**であり、

> **`veil_rt_reactor` の中でも poller ごとに別のソースファイルが選択されるため、
> epoll が通ることは kqueue/wsapoll が通ることを何ら保証しない。**

`reactor/tcp/` に**プラットフォーム別のメソッドを追加する場合は、
unix.rs / windows.rs の両方に追加する**こと。
検出するには Windows/macOS のクロスビルド（`build-cross.sh`）を回すしかない
（macOS は `unix.rs` を共用するため今回は通っていた）。
