# B-192: 起動時の設定で作られた静的配信の監視スレッド（veil-fswatch）が seccomp・Landlock の外で動く

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の L-5）。F-272。

## 現象

seccomp（`PR_SET_SECCOMP`、TSYNC なし）と Landlock（`landlock_restrict_self`）は、掛けたスレッドとその後に作るスレッドにしか
効かない。起動時の設定に `watch = true` があると、監視スレッドは設定のロード（`load_config`、サンドボックスを掛ける前）の中で
作られるので、制限なしのまま動き続けていた（`/proc/<pid>/task/<tid>/status` の `Seccomp:` が 0）。seccomp の許可リストに
inotify を足したのは、リロード（サンドボックス後のスレッドから起動）のときにしか効いていなかった。監視スレッドは要求を
受けないので直ちに悪用はできないが、静的ルートに書き込める者が作るディレクトリ名を辿る長寿命のスレッドが制限の外にあった。
`entry.rs` のコメント「seccompはプロセス全体に適用」も実際と違っていた。

## 直したこと

- `cache::watch::hold_threads()`（起動時の設定のロードの前）と `release_threads()`（seccomp・Landlock を掛けた後）を足し、
  その間に作るはずの監視スレッド（inotify・kqueue）は作らずに預かる。通知は inotify・kqueue の fd に溜まるので取りこぼさない。
  リロードでは今までどおりすぐ作る（サンドボックスの後のスレッドから作るので制限が掛かる）。
- TSYNC（`SECCOMP_FILTER_FLAG_TSYNC`）でプロセス全体に掛ける案は採らなかった（既に動いている他の背景スレッド（ログ等）にも
  許可リストが効くようになり、別の不具合の元になる）。
- `entry.rs` のコメント、`docs/guide/security.md`・`compression-cache.md`（英日）、`examples/config.toml` の seccomp の注意を
  直した。

## 試験

- E2E `b192_fswatch_thread_runs_under_seccomp`（`tests/standalone_e2e.rs`、Linux。seccomp を `log` で有効にして `watch = true`
  で起動し、`veil-fswatch` スレッドの `Seccomp:` が 2 であること。直す前は 0）。
