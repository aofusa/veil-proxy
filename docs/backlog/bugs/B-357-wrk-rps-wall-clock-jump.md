# B-357: wrk の `Requests/sec` が WSL2 の壁時計の戻りで高く出て、HTTP/1.1 の行の比を揺らす

**状態: 修正済み（feat/v0120-perf、2026-11-10）**

v0.12.0 の最後の計測で、`proxy_ab.py`（アクセスログの条件）の同じバイナリの同じケースの rps が、回によって 2 倍近くに分かれていて見つけた
（`h1_file_3b`、キャッシュ無し、4 ワーカー: 87,378〜96,946 と 136,606〜167,584）。高い方の行だけ TSV の `wrk_timeouts` が 250 件前後あった。

## 事象

wrk の `Requests/sec` は完了数を `gettimeofday` の差（壁時計）で割る。aofish の WSL2 の壁時計は約 29 秒ごとに 1 秒の間隔で 2 回戻る
（180 秒の間に 6 回。−0.92 秒と −0.71 秒。単調時計との差を見る簡単なスクリプトで確かめた）。

- 計測の窓にこの戻りが入ると、実行時間が短く見える。`proxy_ab.py` の 4 秒の実行は 2.4 秒に見えて rps が 1.7 倍、`suite_ab.sh`（`run_perf.sh`）の
  10 秒の実行は 8.4 秒に見えて 1.2 倍になる（`CPU/要求` は要求数を rps から求めているので同じだけ小さく見える）。
- 同じ構成でも戻りが窓に入るかどうかで HTTP/1.1 の行が揺れ、交互の比が大きく振れる。v0.12.0 のスイートの測り直しでは HTTP/1.1 の比が 0.861〜1.230、
  同じ時刻に測った h2load（単調時計）の HTTP/2 は 0.978〜1.136 だった。
- 11 月 8 日の計測（`linux_proxy_ab_raw.tsv`）では戻りは 1 回 75 ms ほどで、`wrk_timeouts` のある 23 行の rps は他の行と同じだった。戻りの大きさは変わる。

h2load（`steady_clock`）・`veil-load`（`Instant`）・k6 は影響を受けない。

## 原因

`tools/perf/proxy_ab.py` の `parse_wrk` と `tools/perf/run_perf.sh` の `parse_wrk` が、wrk の `Requests/sec` の行をそのまま使っていた。

## 対応

どちらも要求数（`N requests in ...`）を設定の秒数（`-d`）で割る。壁時計の戻りは、完了した要求数には影響しない。`run_perf.sh` は `WRK_ARGS` の `-d` から秒数を読む
（読めないときだけ従来の行を使う）。

## テスト

- 戻りが入った形の wrk の出力（`1200000 requests in 8.38s`、`Requests/sec: 143198`）を両方の `parse_wrk` に渡して、120,000 になることを確かめた。
- 修正の後の `suite_ab.sh` で v0.11.0 との A/B をやり直した（[docs/perf/README.md](../../perf/README.md)）。
