# B-409: `veil -t` が TLS の設定の組み立てと WASM のモジュールの読み込みをせず、起動で止まる設定を「successful」と報告していた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

B-406（WAF の例のフィルタ）を調べる途中で見つけた。起動時の検査（B-301）で「wasm module 'waf_filter' failed to start ...
all fuel consumed」と止まる設定を、`veil -t` は「test is successful」と報告した。

`veil -t`（`config::test_config_file`）が見ていたのは TOML の構文・設定値の検証・TLS の証明書と鍵のファイルがあるかだけで、
起動時に行う次のことをしていなかった。

- TLS の設定の組み立て: PEM の解析、証明書と鍵の対応、`cipher_suites` の名前の解決、`client_ca` の読み込み。壊れた PEM や
  入れ違えた鍵でも `-t` は通り、起動で止まる。
- `[wasm]` のモジュールの読み込み（コンパイル・リンク）と起動時のコールバック（`_start`・`proxy_on_vm_start`・
  `proxy_on_configure`）。燃料の尽きるモジュール・設定を受け付けないモジュール・壊れた `.wasm` でも `-t` は通る。

`systemctl reload` の前や配布物の更新の前に `veil -t` を通しても、起動・再起動で止まり得た（リロードは前の設定のまま
続くが、再起動では止まる）。

## 修正

- `veil -t` は `config::test_config_for_startup` を使う。`test_config_file` の検査の後、起動と同じ `load_tls_config` で TLS の
  設定を組み立て（`[tls]` があるとき）、起動と同じ `build_wasm_engine` で `[wasm]` のモジュールを読み込んで起動時の
  コールバックを走らせる。エラーは起動と同じ文になる（TLS は `[tls]: ` を前に付ける）。
- TLS の設定の組み立てはプロセスで共有する値（クライアント認証・セッション再開の方式）を記録するので、すぐ終わる `-t` の
  プロセスだけで行う。ライブラリが起動の前に呼ぶ `test_config_file` は従来どおり（起動でもう一度組み立てる）。
- 利用者ガイド（`docs/guide/running.md` と `ja`）の「検証内容」に書いた。`-t` もモジュールのコンパイルで AOT キャッシュを書く
  （起動と同じ。B-407 の名前）。

## 試験

単体 `config::cli::tests::b409_startup_test_rejects_unparsable_certificate`（ファイルがあるかの検査は通り、`-t` は
`[tls]: ` で失敗する）、`b409_startup_test_runs_wasm_module_startup`（`proxy_on_configure` が 0 を返すモジュールで `-t` が失敗する）。
