# B-345: ガイドに無いキー `[logging] max_log_size`・`[logging] file_path` が載っている

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

B-343 でログファイルの説明を書き直すときに見つけた。

## 事象

- `docs/guide/observability.md`（英日）の `[logging]` の表に `max_log_size`（最大ログファイルサイズ、既定 104857600）がある。
- 同じページの「JSON 形式の設定例」が `[logging] file_path = "/var/log/veil.json"` と書いている。
- `docs/guide/configuration.md`（英日）の設定例のコメントが「最大ログファイルサイズ」と `# file_path = "/var/log/veil.log"` を載せている。

どちらのキーも無い（`[logging]` は未知のキーを拒否しないので黙って無視され、例の通りに書くとログはファイルではなく標準出力へ出る）。
`max_log_size` は働かないキーとして b8e9bb34（2026-07）で消したが、ガイドの一部が残っていた。`[logging] file_path` は
`app_file_path`・`error_file_path` に分けたときの名残。

## 対応

ガイドから `max_log_size` を消し、例を `app_file_path`・`error_file_path` に直す。ログファイルは日ごとに切り替わり、大きさの上限は
無いことを書く（B-343 の「ログファイル」の節）。設定の読み込みは変えない（ガイドの例を写した設定を起動の失敗にしない）。
