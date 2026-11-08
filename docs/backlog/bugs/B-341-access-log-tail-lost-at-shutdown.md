# B-341: 終了時にアクセスログの最後の行が書かれずに消える

**状態: 修正済み（feat/v0120-perf、2026-11-08）**

[F-302](../features/F-302-access-log-hot-path.md) の計測の途中で、書かれた行の数を要求の数と突き合わせて見つけた。

## 事象

SIGTERM で止めると、止める直前に処理した要求のアクセスログの行が書かれずに消える。production の例（`examples/api/production`、
Rust、release、アクセスログを有効にして標準出力をファイルへ）に 1 本の接続で 3,000 要求を送り、すぐ SIGTERM を送ると、ファイルの行は
2,967（F-302 の前）・2,974（F-302 の第 2 版）だった。

## 原因

ライタースレッドは `BufWriter`（8KiB）へ書き、`flush_interval_ms` ごと（行が来ない間は `flush_interval_ms` 待ってから）にフラッシュする。
終了の手順（`entry::run_server` の最後）はライターを止めも待ちもしないので、プロセスが終わるときに `BufWriter` に残った行（8KiB まで）と、
まだ取り出されていない行（F-302 の前はチャネル、後は溜め）が消える。F-302 の前から同じ。

## 対応

全ワーカーの join の後に `access_log::shutdown_access_log_writer()` を呼ぶ。送り先を外してライターに「溜まった分を書いて終わる」よう
指示し（リロードと同じ止め方）、ライターが書き切って抜けたことを `Condvar` で待つ。書き込み先が詰まっている（標準出力のパイプを誰も
読まない等）ときに終了を止めないよう、待つのは 2 秒まで。ライブラリ（`veil::serve`・Python の `veil.run`）も同じ `run_server` を通る。

## テスト

- 単体: `shutdown_writes_buffered_lines_before_returning`。定期のフラッシュが来ない設定で 100 行足し、待ち合わせから戻った時点でファイルに
  100 行あることを確かめる。待たない実装では 0 行で失敗することを確かめた。
- E2E: `tests/api/library_e2e.sh`（rust）に「1 本の接続で 300 要求を送ってすぐ SIGTERM を送ると、標準出力のアクセスログが 300 行ある」を足した
  （`flush_interval_ms = 60000` で定期のフラッシュを外す）。Windows の `kill` は終了処理を走らせないので除く。
