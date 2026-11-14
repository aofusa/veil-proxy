# B-383: 設定のリロードで足した・変えた Proxy-Wasm のフィルタが黙って効かなかった

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

feat/beat-nginx の 2 回目のセキュリティスキャン（`CLAUDE-SECURITY-20261114-024333`）の指摘 F5 から確認。

## 事象

- `reload_config` は新しいルートを入れる一方で、WASM のエンジンは起動時のもの（`current.wasm_filter_engine`）を
  引き継いでいた。新しいルートの `modules` は新しいファイルの `[[wasm.modules]]` に対してだけ検査される。
- 要求の時点の `resolve_effective_modules` は、古いレジストリに無いモジュールを `filter_map` で落とし、残りが
  空なら `FilterResult::Continue` を返す。リロードで足した認可のフィルタ・差し替えた `.wasm`・`data_files` は
  何も記録されないまま効かず、そのルートは守られないまま要求を通していた。
- 利用者ガイド（`docs/guide/wasm.md`、B-301）は「起動とリロードで `_start`・`proxy_on_configure` を走らせる」と
  書いていたが、リロードでは走っていなかった。
- `[wasm]` が無い・`enabled = false` のときにモジュールを並べたルート・`[[l4]] wasm_modules` も、エンジンが無い
  ので黙ってフィルタ無しで動いていた。

## 対応

- `load_config_without_tls`（リロードの唯一の入口）で `[wasm]` からエンジンを作り直す（`reload_wasm_engine`・
  `build_wasm_engine`。起動時の `load_config` と同じ関数）。モジュールの `_start`・`proxy_on_vm_start`・
  `proxy_on_configure` が失敗したら、プロセス全体の値を書き換える前にリロードを断る（前の設定のまま）。
  共有データ（`proxy_*_shared_data`）の表はモジュール名ごとにプロセスで持つので、リロードでも引き継ぐ。
- FreeBSD の capability mode では起動後に `.wasm` を開けないので、起動時のエンジンを使い続け、`[wasm]` が
  起動時と違うリロードは `check_capability_mode_reload` で断る（`STARTUP_WASM_SECTION` と比べる）。
- `validate_route_config`・`validate_config` は、`[wasm]` が無い・無効なのにモジュールを並べたルート・L4 の
  リスナーを拒否する（「lists WASM modules but [wasm] is not enabled」）。
- 利用者ガイド（`docs/guide/wasm.md`・`ja`）にリロードの振る舞い・capability mode の制限を書いた。

## テスト

- `config::validate::b383_tests::route_modules_require_enabled_wasm`・
  `config::loaded::b383_wasm_section_tests::wasm_section_value_detects_changes`。
- 自己完結型 E2E `b383_wasm_filter_added_by_reload_runs`（フィルタの無い設定で起動し、SIGHUP でフィルタを足した
  設定へ替えると次の要求からフィルタが応答を書き換える）。
- FreeBSD の capability mode の分岐は Linux ではコンパイルされない（BSD の VM で確かめる）。
