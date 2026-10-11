# B-115: SIGTERM で graceful shutdown しない（ネイティブは即終了、コンテナでは無視される）

**状態: 完了（feat/f185-memory-leak-check）**

## 事象

F-185 のリーク試験で sanitizer ビルドを SIGTERM で止めると、120 秒待っても終了しなかった。調べると本番ビルドでも
SIGTERM を一度も処理していなかった（最初のコミットから）。

- ネイティブ（systemd・`kill` 等）: SIGTERM の既定動作でその場で終了する。`Received shutdown signal` も
  `Server shutdown complete` も出ず、処理中の接続は切られ、終了処理（証明書のゼロ化・UDS の unlink・
  OpenTelemetry の最終 flush）も走らない。
- コンテナ（veil が PID 1）: PID 1 はハンドラの無いシグナルを無視するので、SIGTERM が届いても何も起きない。
  `docker stop` は猶予 10 秒後の SIGKILL、Kubernetes は `terminationGracePeriodSeconds` 後の SIGKILL で止まる。

ドキュメント（`docs/guide/running.md`・`features.md`）は「SIGINT / SIGTERM で安全に終了する」と書いていた。
SIGINT（Ctrl+C）は正しく動いていた。

## 原因

`server::setup_signal_handler` が `ctrlc::set_handler` を使っていたが、ctrlc は `termination` feature が
無いと SIGINT しか登録しない。`termination` を有効にすると SIGTERM に加えて SIGHUP も同じハンドラ（終了）に
なり、SIGHUP の設定リロードと衝突するので使えない。

E2E の停止処理（`tests/e2e_setup.sh` の `stop_servers`）は SIGTERM の 5 秒後に SIGKILL へフォールバックし、
ネイティブ起動では SIGTERM で（既定動作により）すぐ終了するため、気付けなかった。

## 改修

- unix は signal-hook の `iterator::Signals` で SIGINT / SIGTERM を受ける専用スレッド（`veil-signal`）を作り、
  ログを出して `SHUTDOWN_FLAG` を立てる。SIGHUP は従来どおり signal-hook のフラグ（設定・証明書のリロード）。
- ctrlc は Windows 専用の依存にした（Ctrl+C / Ctrl+Break）。
- 統合テスト `b115_shutdown_signals`（`tests/integration_tests.rs`）: バイナリを起動して SIGTERM・SIGINT で
  終了コード 0 と graceful shutdown のログ、SIGHUP では終了せず設定を読み直すことを確かめる。

修正後、ネイティブ（seccomp・Landlock 有効）と sanitizer ビルドのコンテナで、SIGTERM から graceful shutdown して
終了コード 0 で終わることを確認した。

## 関連

- SIGTERM が効くようになったことで、drain の待ちが切れたときに abort する既存の不具合（SIGINT では以前から
  起きていた）を通りやすくなったため、同じブランチで直した（B-118）。
- graceful shutdown はアイドルなキープアライブ接続も待つため、クライアントが接続を保持していると上限
  （既定 30 秒）まで終わらない。改善は F-187。
