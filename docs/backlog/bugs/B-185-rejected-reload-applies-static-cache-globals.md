# B-185: リロードがルートの解決で拒否されても、静的配信のキャッシュのグローバル設定だけ新しい値になる

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 のデータプレーンのレビュー（`docs/artifacts/v0100/review-dataplane.md` の M-1）。F-272 で入った。

## 現象

SIGHUP で読んだ設定が `[route.traffic.cors]` の不正（`allow_credentials = true` と `"*"` の組・空の `allow_origins`）や
JWT の設定ミスで拒否されると、`reload_config` は失敗を返して前の `RuntimeConfig` を使い続けるが、`open_file_cache` の
有効・有効期間・最大件数、`static_file_cache` の有効・上限・`revalidate_mtime`・`watch` は拒否された設定の値でプロセス
全体に効いていた（「リロードは全部通るか何も変えない」が崩れる）。

## 原因

F-272 で、ルートの Backend の組み立て（`attach_static_watch`）がグローバルの `watch` の既定値を読むので、
`cache::configure_global_open_file_cache`・`configure_global_open_file_cache_watch`・`configure_global_static_content_cache`
をルートの解決（`route_traffic::resolve(..)?` で失敗し得る）より前に移していた。

## 直したこと

- ディスク監視の既定値は、読み込み中の設定から `WatchDefaults`（`src/config/loaded.rs`）に写して `build_backend_loaded` へ
  引数で渡す（`cache::open_file_cache_watch_enabled`・`static_file_cache_watch_enabled` も既定値を引数で受ける）。読むだけの
  グローバル（`OPEN_FILE_CACHE_GLOBAL_WATCH`）と `configure_global_open_file_cache_watch` は外した。
- グローバルのキャッシュ設定の書き込み（`apply_global_cache_settings`）は、ルートの解決の後（その後に失敗する処理は無い）へ
  戻した。起動時の `load_config` も同じ順にした。
- 監視の登録（`cache::watch::register`）は拒否された設定のルートでも残る（監視は外さない設計。F-272 のとおり、一度でも設定した
  ルートの数で抑えられる）。

## 試験

- 単体 `config::loaded::b185_reload_tests::rejected_reload_leaves_global_cache_settings_unchanged`（キャッシュの値を変え、
  CORS の不正で拒否される設定をリロードし、グローバルの値が変わらないこと。直す前は失敗）。
