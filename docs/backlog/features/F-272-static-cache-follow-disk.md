# F-272: 静的配信のキャッシュ（`open_file_cache`・`static_file_cache`）がディスクの変更に追従する（ファイルの変更通知）

**状態: 未着手（2026-10-25 起票・設計。親は [F-258](F-258-v0100-plan.md)）**

## 背景（調査で確認）

- `open_file_cache`（`src/cache/file_cache.rs`）・`static_file_cache`（`src/cache/content_cache.rs`）は有効期限（`valid_duration_secs`、
  既定 60 秒）まで古い fd・中身を返す。期限の確認は粗い時計との比較だけ。
- `static_file_cache` の `revalidate_mtime = true` は当たるたびに mtime の `stat` を 1 本（offload）出す。HTTP/2・HTTP/3 の静的配信を
  offload ゼロにする F-157 の前提が崩れる。`open_file_cache` には同等の再検証が無い（古い inode の fd を持ち続ける）。
- HttpArena の静的配信の検査は、ファイルと `.br`/`.gz` を **同じサイズで `mv` で差し替え**（inode が変わる）、framework は 2 秒以内・
  infrastructure は 30 秒以内に新しい中身を返し、元に戻したら元の中身を返すことを求める（static-tls）。フレームワークの standard は
  「静的配信はディスクに追従すること」。有効期限を 1 秒にすると要求ごとの開き直しが増えてキャッシュの意味が薄れる。
- 運用でも、デプロイでファイルを差し替えた直後に古いものを最大 60 秒返す。

## 制約

- 本体の機能。**設定で有効にしたルートだけ**（既定は今と同じ有効期限だけの動き）。
- ホットパス: キャッシュに当たった要求の経路に syscall を足さない。増えるのは **原子変数の読み取り 1 回**（下の世代番号）だけ。
- 通知の受け取りは専用のスレッド（データプレーンの外）。新しい io_uring のオペコードは使わない。

## 設計

### 1. 世代番号による無効化

- キャッシュの各エントリに「作ったときの世代」を持たせる。監視しているディレクトリ（ルートの `path` の配下）ごとに
  `AtomicU64` の世代を持ち、エントリの検証は `entry.gen == dir_gen.load(Relaxed)` の比較だけ（一致すれば有効期限の確認へ）。
  通知で変更を知ったら、該当するディレクトリの世代を 1 増やす（そのディレクトリのエントリが次の要求で作り直される）。
  ディレクトリ単位なのは、通知の対応づけを軽くし、エントリを個別に探さないため（変更は稀なので作り直しの費用は問題にならない）。
- `open_file_cache` の fd・`static_file_cache` の中身・事前圧縮の兄弟ファイル（`.br`・`.gz`）・キャッシュしたメタデータの全部に効かせる。

### 2. OS ごとの通知

| OS | 方式 | 補足 |
|---|---|---|
| Linux | inotify（`IN_CLOSE_WRITE`・`IN_MOVED_TO`・`IN_MOVED_FROM`・`IN_CREATE`・`IN_DELETE`・`IN_ATTRIB`・`IN_DELETE_SELF`）をディレクトリごとに付ける（再帰は起動時に辿り、`IN_CREATE` のディレクトリは追加で付ける） | 専用スレッドがブロッキングの `read` で受ける。seccomp の `ALLOWED_SYSCALLS` に `inotify_init1`・`inotify_add_watch`・`inotify_rm_watch` を足す（番号を `libc::SYS_*` で確かめ、`test_allowed_syscalls_match_libc_numbers` にも足す。B-103）。Landlock の読み取り許可の中のパスにしか付けない |
| FreeBSD・OpenBSD・NetBSD・macOS | kqueue の `EVFILT_VNODE`（`NOTE_WRITE`・`NOTE_DELETE`・`NOTE_RENAME`・`NOTE_ATTRIB`・`NOTE_EXTEND`）をディレクトリの fd に付ける。中身の上書き（rename を伴わない）は、`open_file_cache` が持っているファイルの fd にも付けて捉える | capsicum の下でも既に持っている fd に付けるだけなので動く（dirfd 相対の F-123 の作り）。OpenBSD の unveil の下でも同じ |
| Windows | `ReadDirectoryChangesW`（再帰、専用スレッド） | |

- 監視の上限（Linux の `max_user_watches`、kqueue の fd の数）に当たったら、そのルートは警告して従来の有効期限の動きに戻す（黙って追従しないままにしない）。
- 通知が溢れた（`IN_Q_OVERFLOW`）ら、全部の世代を増やす（全エントリを作り直す）。

### 3. 設定

```toml
[route.open_file_cache]
enabled = true
valid_duration_secs = 60
watch = true            # 新設。既定 false

[route.static_file_cache]
enabled = true
watch = true            # 新設。既定 false（open_file_cache と同じ値を推奨）
```

- `[performance]` の大域の `open_file_cache_*` にも `open_file_cache_watch` を足す。ライブラリの `App::serve_dir`（F-267）・Python の
  `StaticFiles`（F-268）は既定で `watch = true` にする（フレームワークの静的配信として追従するのが自然なため）。
- リロード: ルートが変われば監視を付け直す。

## 試験

- 単体: 世代の比較、通知 → 世代の増加 → エントリの作り直し、溢れのときの全体の作り直し。
- E2E（Linux の io_uring・epoll、FreeBSD・OpenBSD・NetBSD・macOS・Windows）: HttpArena と同じ手順（同じサイズで `mv` の差し替え・
  `.br` の兄弟・元に戻す）で、`watch = true` なら 2 秒以内に新しい中身、`false` なら今どおり期限まで古い中身。中身の上書き（`cp` で同じ inode）も。
  サンドボックス（seccomp・Landlock・capsicum・pledge/unveil・Seatbelt）を有効にした既定の設定で動く。
- 性能: `watch = true` で HTTP/1.1・HTTP/2・HTTP/3 の静的配信（3B・54KB）が `watch = false` と交互 A/B で同じ（増えるのは原子変数の読み取りだけ）。
  offload の回数がゼロのまま（F-157 の確認）。

## 文書

- `docs/guide/compression-cache.md`（英日）、`examples/config.toml`、`docs/guide/security.md`（seccomp に足した syscall）。

## 受け入れ条件

- 全 OS で差し替えに 2 秒以内に追従し、キャッシュに当たった要求の性能が変わらない。文書（英日）・`backlog.md` が更新されている。
