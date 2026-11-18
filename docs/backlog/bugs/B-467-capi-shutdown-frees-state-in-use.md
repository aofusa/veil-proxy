# B-467: C の veil_run が、ハンドラ用プールに残った要求の仕事（ハンドラ・ミドルウェア・after_send）が動いている間に停止の関数で状態を解放していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「Guard state of veil_kit_require_scope / veil_kit_jwt_authenticate is freed at shutdown while handler-pool jobs can
still run the guard」
から確認。

## 事象

- ハンドラ用プールはプロセスに 1 つで、キューを捨てない。`veil_run` はワーカーが終わるとすぐ停止の関数
  （`veil_config_on_shutdown`）を呼んで戻っていたので、キューに残った・動いている要求の仕事が、解放した状態を使えた。
- `veil_kit_require_scope`・`veil_kit_jwt_authenticate` は停止の関数でガードの状態（`Require`・`JwtGuard`）を
  解放するので、遅いハンドラでプールを埋めた後に停止すると、キューの要求の `jwt_before` が解放済みのメモリを読んだ
  （未定義の動作。認証を飛ばして C のハンドラが動くこともあり得る）。C の利用者が `veil_run` の後に解放する
  `user_data`（ルート・after_send・`veil_kit_every`）も同じ。

## 対応

- ハンドラ用プールへ渡した仕事（キューの分と実行中の分）を数える（`runtime::offload::wait_handler_jobs`。数えるのは
  ハンドラ用プールの仕事だけ）。
- `veil_run`・`veil_test_free` は、停止の関数を呼ぶ前にそれが 0 になるのを待つ。`[server] graceful_shutdown_timeout_secs`
  （最低 1 秒）を過ぎても残っていれば、解放すると動いている仕事が解放済みの状態に触れるので停止の関数を呼ばない
  （エラーのログを出す）。`veil.h` と利用者ガイド（`library.md` の英日）に書いた。

## テスト

- `runtime::offload::b467_tests::wait_handler_jobs_waits_for_queued_and_running_jobs`（実行中の仕事を待つ・期限で
  `false`・panic した仕事も終わりとして数える）。
