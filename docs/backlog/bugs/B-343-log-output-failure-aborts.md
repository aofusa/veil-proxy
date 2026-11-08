# B-343: ログの出力先へ書けないと、ログのスレッドが panic してプロセスが abort する

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

v0.12.0 の最終の検証で FreeBSD の VM の E2E・security-e2e を回したところ、veil が起動の直後に SIGABRT（終了コード 134）で落ち、
E2E の 11 件と security-e2e が失敗した。VM のディスクが満杯（105%）になっていたためで、空けると通ったが、ログを書けないだけで
プロキシ全体が落ちるのは veil の不具合なので起票した。

## 事象

ログの出力先（標準出力・標準エラー出力・`[logging]` のファイル）へ書けないと veil が abort する。Linux で
`veil -c config.toml >/dev/full 2>/dev/full` とすると、要求が無くても 1.5 秒以内に終了コード 134 で落ちる。ディスクの満杯のほか、
読み手のいなくなったパイプ（`EPIPE`）、容量の上限のある出力先で起こる。ftlog を使い始めてからずっとある（v0.11.0 にもある）。

## 原因

ftlog 0.2.16 の作りによる連鎖（gdb で panic の箇所を確かめた）:

1. ログのスレッドは root の writer（veil の `LogRoutingWriter`）が `Err` を返すと、`eprintln!("logger write message failed")` で
   知らせる。標準エラー出力も書けないと、その `eprintln!` が panic してログのスレッドが終わる。
2. 次にログを出したスレッド（起動中のメインスレッドの `pool::init_server_header` の info）は、キューが閉じたことを
   `eprintln!` で知らせようとして panic する。
3. その巻き戻しの途中で `LoggerGuard` の drop が `expect("logger queue closed when flushing")` で panic し、2 度目の panic で abort する。

`[logging]` の出力先がファイルのときに使っていた ftlog の `FileAppender` も、日付が変わって次のファイルを開けないと
`open(..).unwrap()` で panic する（権限・inode の枯渇・サンドボックス。capsicum は B-344）。切り替えのたびにタイムゾーンを
読み直し（複数スレッドのプロセスでは `/etc/localtime` を開く）、その失敗も `unwrap` する。起動時に開けないときも `unwrap` の
panic（終了コード 101）だった。

アクセスログのライターも、出力先を開けない・開き直せないことを `eprintln!` で知らせていた（標準エラー出力が書けないと
ライターのスレッドが panic する）。

## 対応

- `LogRoutingWriter`（ftlog の root）は出力先の失敗を ftlog へ返さない（`write`・`flush` とも成功を返す）。出力先ごと（app・error）に
  書けなかった行を数え、次に書けたときに `N log lines were dropped because the log output could not be written` を 1 行
  （app は WARN、error は ERROR。`format = "json"` なら JSON の 1 行）出してから続ける。
- `FileAppender` をやめて `DailyFile` に置き換える。ファイル名は ftlog と同じ（`veil.log` → `veil-20261108.log`、拡張子が無ければ
  `veil` → `veil-20261108`）。UTC からのずれは起動時に 1 回だけ（unix は `localtime_r` の `tm_gmtoff`）求め、地方時の 0 時に
  切り替える。次のファイルを開けなければ今のファイルへ書き続け、60 秒ごとに開き直す。
- 起動時にログファイルを開けなければ `veil: cannot open the log file <path>: <error>` を標準エラー出力へ出して終了コード 1 で止める
  （`init_logging` が `Result` を返す）。メッシュのサブコマンド（Envoy・Linkerd の置き換え）は標準出力・標準エラー出力だけを使うが、
  同じく受ける。
- アクセスログのライターの `eprintln!` を、失敗を無視する `writeln!(io::stderr(), ..)` に変える。

## テスト

- 単体（`logging::tests`）:
  - `unwritable_output_is_not_an_error_and_dropped_lines_are_reported`: 書けない間も `write` は成功を返し、書けるようになると
    捨てた行の数（3）を知らせる 1 行を出してから続ける。
  - `dropped_lines_notice_is_a_json_line_in_json_format`: JSON の形式では知らせる行も JSON の 1 行。
  - `dated_log_path_matches_ftlog_daily_names`: ftlog と同じファイル名（拡張子の有無・ドットを含む名前）。
  - `daily_file_rolls_over_and_keeps_writing_when_the_next_file_cannot_be_opened`: 次の日のファイルを開けないと今のファイルへ
    書き続け、開けるようになったら切り替える。同じ日のうちは切り替えない。
  - `daily_file_reports_an_error_when_the_first_file_cannot_be_opened`
  - `until_next_day_counts_to_local_midnight`: UTC+9 で地方時の 0 時までの時間。
  - 知らせる行を出さない変更で前の 2 つが、次のファイルを開けないと今のファイルを捨てる変更で `daily_file_rolls_over_...` が
    失敗することを確かめた。
- E2E（`tests/standalone_e2e.rs` の `b343_unwritable_log_output_does_not_abort`、Linux）: 標準出力・標準エラー出力を `/dev/full` に
  して起動し、info のログとアクセスログを出させながら 50 要求を処理する。1.5 秒後も生きていて要求を処理でき、SIGTERM で終了コード 0
  で終わる。修正前のコードでは起動中に終了コード 134 で落ちて失敗することを確かめた。
