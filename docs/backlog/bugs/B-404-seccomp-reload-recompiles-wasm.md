# B-404: seccomp（strict）の下で設定をリロードすると、WASM のモジュールのコンパイルでプロセスが落ちた

**状態: 修正済み（feat/beat-nginx、2026-11-14）**

## 事象

main（77160e9e）の nightly（container-security-nightly）の `e2e (io_uring, seccomp strict)`・`e2e (epoll, seccomp strict)` で、
veil が試験の途中で `Bad system call (core dumped)` で落ち、以後の試験が接続できずに失敗した（16 件の失敗の後は 8443 への接続拒否）。
前回の nightly（a040cf79）では通っていた。seccomp を掛けない `e2e (io_uring)`・`e2e (epoll)` は通った。

手元で `VEIL_E2E_SECCOMP=log ./tests/e2e_setup.sh test` を回すと、カーネルの監査ログに `comm="veil" syscall=10`（`mprotect`）が
30 件出た。

## 原因

B-383（同じブランチ）で、設定のリロードのたびに `[wasm]` からエンジンを作り直し、モジュールをコンパイルし直すようにした。seccomp の
フィルタ（F-25）は `mmap`・`mprotect` の `PROT_EXEC` を拒否するので、Cranelift がネイティブのコードを実行可能にする
`mprotect(PROT_EXEC)` が拒否される。

- `strict` は `SECCOMP_RET_KILL_PROCESS` なのでプロセスごと落ちた（SIGHUP を送る `test_http3_goaway_graceful_reload` 等）。
- `filter` は `EPERM` でエンジンを作れず、`[wasm]` を使う設定ではどのリロードも失敗していた。

起動時のコンパイルはフィルタを掛ける前なので問題にならない。v0.12.0 までのリロードは起動時のエンジンを使い続けていた（B-383 の問題）
ので、この落ち方は公開した版には無い。

## 修正

- seccomp を掛けた後の実行可能なメモリの扱いを `security::exec_memory()`（`ExecMemory`）で見られるようにした。`filter`・`strict` は
  `Denied`、`log` は `Logged`（通して記録する）。
- リロード（`config::loaded::reload_wasm_engine`）は、ネイティブのコードが要る `[wasm]`（有効で Pulley でない）なら
  `compiled_wasm_engine` で、今のエンジンが同じ `[wasm]`・同じモジュールと `data_files` のファイルから作られたかを見る。
  同じならそれを使い続ける（`_start`・`proxy_on_configure` は走らせ直さない）。違えば `filter`・`strict` は
  「seccomp forbids executable memory after startup, ...」でリロードを断る（前の設定のまま）。B-383 の目的（リロードで足した・
  変えたフィルタが黙って効かないことは無い）は保つ。
- `log` も同じ判断をし、違うときはコンパイルを通して警告する。以前は `log` のリロードが毎回コンパイルしたので、`strict` では
  起きない `mprotect(PROT_EXEC)` がカーネルの記録に残っていた（`log` で確かめてから `filter` へ移る手順の妨げ）。
- エンジンは作った元の `[wasm]`（`toml::Value`）と、モジュール・`data_files` のファイルの印（inode・大きさ・mtime・ctime）を持つ
  （`wasm::source::WasmSource`。モジュールを読む前に取る）。中身は読まないので、Landlock の読み取りの許可が無いファイルでも
  調べられる。
- `interpreter = true`（Pulley）は実行可能なメモリが要らない（wasmtime は Pulley の `.text` を実行可能にしない）ので、従来どおり
  作り直す。Pulley かどうかの判断は `wasm::uses_pulley` の 1 箇所にまとめた（エンジンの作成・AOT キャッシュの名前・リロード）。
- 利用者ガイド（`docs/guide/wasm.md`・`docs/guide/security.md` と `ja`）に書いた。

## 試験

- 単体: `wasm::source::tests::detects_section_module_and_data_file_changes`、
  `config::loaded::b383_wasm_section_tests::b404_compiled_engine_is_reused_only_without_changes`。
- 自己完結型 E2E `b404_reload_under_seccomp_keeps_compiled_wasm`: seccomp strict で起動し、`[wasm]` が同じリロードはルートの追加を
  反映してフィルタも走り続ける。`[wasm]` を変えたリロードは断られて前の設定のまま。`interpreter = true` へ切り替えるリロードは反映される。
- E2E: `VEIL_E2E_SECCOMP=strict` を io_uring・epoll で回す（594 件）。`VEIL_E2E_SECCOMP=log` で、カーネルの記録に veil の
  `mprotect` が出ない（修正前は 30 件）。
