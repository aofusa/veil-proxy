# B-193: inotify: ルートの中でディレクトリを移すと、監視が古いパスのまま残り、移った先に作ったディレクトリを追えない

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の L-6）。F-272。

## 現象

`root/a` を `root/b` へ `mv` した後に `root/b/new/` を作ると、`a` の watch（inode に付いたまま動く）が持つパスは `root/a` の
ままなので、`root/a/new` へ付けようとして ENOENT（警告が出る）になり、`root/b/new` の中の変更は `valid_duration_secs` まで
追従しなかった。`IN_MOVED_FROM` でルートの外へ出たディレクトリの watch も外れずに残っていた。

## 直したこと

- ルートの中のディレクトリの移動（`IN_ISDIR` と `IN_MOVED_FROM`・`IN_MOVED_TO`）を受けたら、そのルートの監視を外して今のパスで
  付け直す（ルートそのものの差し替えと同じ扱い。`reattach`）。移動はまれなので、cookie で組を作ってパスを書き換える案は
  採らなかった。
- 文書: `docs/guide/compression-cache.md`（英日）の対象の一覧に移動・削除を書いた。

## 試験

- 単体 `cache::watch::tests::inotify_follows_directory_moved_inside_root`（Linux の実際の inotify。移した後に作った
  ディレクトリの中の変更で世代が増え、古いパスの watch が残らないこと。直す前は通知が来ずに失敗）。
