# B-344: FreeBSD の capability mode では、日付が変わるとログファイルを切り替えられずに落ちる

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

B-343 を調べる中で ftlog の `FileAppender` を読んで見つけた。

## 事象

FreeBSD で capsicum の capability mode（`[security] enable_capsicum = true`・`capsicum_capability_mode = true`）を使い、`[logging]` の
`app_file_path`・`error_file_path` を指定していると、地方時の 0 時を過ぎて最初のログの行で veil が abort する（v0.11.0 にもある）。

## 原因

capability mode ではパスで `open(2)` できない（`ECAPMODE`）。ftlog の `FileAppender` は日付が変わると次のファイル
（`veil-YYYYMMDD.log`）をパスで開き、失敗を `unwrap` で panic する（その前のタイムゾーンの読み込みも `/etc/localtime` を開いて
`unwrap` する）。ログのスレッドが panic すると B-343 の連鎖で abort する。アクセスログ（F-178）・証明書（F-136）・設定ファイル（F-178）は
cap_enter の前に開いたディレクトリの fd から開き直していたが、`[logging]` のファイルは対象外だった。

## 対応

- B-343 で `FileAppender` を `DailyFile` に置き換え、切り替えで panic しなくなった（開けなければ今のファイルへ書き続け、60 秒ごとに
  開き直す。タイムゾーンは起動時に 1 回だけ読む）。
- FreeBSD では cap_enter の前に `[logging]` のファイル（app・error）の親ディレクトリを開いて登録し（`capsicum::init_log_file_dirfd`。
  権利は `CAP_LOOKUP`・`CAP_CREATE`・`CAP_WRITE`・`CAP_SEEK`・`CAP_FSTAT`・`CAP_FCNTL` に絞る。同じディレクトリは 1 つの fd を共有する）、
  `DailyFile` は次のファイルをその fd から `openat(.., O_RESOLVE_BENEATH)` で作る（`capsicum::open_log_file_append`。未登録の
  ディレクトリは通常の open）。
- 登録できなければ error を出して起動を続ける（日付が変わっても今のファイルへ書き続ける）。

## テスト

- 単体（FreeBSD、`capsicum::named_file_dir_tests`）:
  - `log_file_dir_opens_dated_files_in_the_registered_dir`: 登録したディレクトリの fd から日付の付いたファイルを作って追記できる。
    同じディレクトリの 2 回目の登録で fd を増やさない。未登録のディレクトリは `None`。
  - `log_file_dir_opens_the_next_day_file_in_capability_mode`: fork した子で cap_enter し、絶対パスでは開けない（`ECAPMODE`）が
    `open_log_file_append` では次の日のファイルを作って書けることを確かめる。
