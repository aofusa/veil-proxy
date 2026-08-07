# B-63: FreeBSD の POSIX AIO 経路（`aio` feature）が HTTP/2 の小レスポンス高並行で完全停止する

**状態: 回避済み（`aio` を `full-freebsd*` の既定から除外）／根本原因は未調査**

## 事象

FreeBSD 14.3 arm64（QEMU/HVF on Apple Silicon、4 vCPU / サーバ 2 コア固定）で
`--features aio` を含むビルド（`full-freebsd-aarch64`、**従来の既定**）を使い、
静的ファイル（3 バイト）を HTTP/2 over TLS で高並行に取得すると、veil が
**応答を返さなくなり、そのまま復帰しない**。

```
h2load -t2 -c32 -m16 -n 40000 https://127.0.0.1:4443/small.html
  → 45 秒でタイムアウト（1 件も完了しない）
```

このとき veil プロセスは **CPU をほとんど消費していない**（累積 0.44 秒）まま、
全スレッドが sleep 状態で止まっている:

```
  PID STAT    TIME COMMAND
34810 I    0:00.44 ./target/release/veil -c /tmp/veilperf/conf/veil_file.toml

  PID    TID COMM       TDNAME     CPU  PRI STATE   WCHAN
34810 113897 veil       -           -1   97 sleep   uwait
34810 125278 veil       logger      -1   88 sleep   uwait
34810 125279 veil       ctrl-c      -1   97 sleep   usem
34810 125280 veil       -           -1   88 sleep   select
34810 125281 veil       -           -1   88 sleep   select
34810 125282 veil       -           -1   97 sleep   select
34810 125283 veil       -           -1   88 sleep   select
```

スピンではなく**完了通知の取りこぼしによるデッドロック**の様相である
（`aio_read`/`aio_write` の `EVFILT_AIO` 完了イベントを待ったまま起床しない）。

同一条件で `aio` を外したビルドでは同じ負荷が **148,679 rps / エラー 0 件**で完走する。

`tools/perf/freebsd/run_perf_freebsd.sh` の小レスポンス計測でも、`aio` 有効ビルドは
HTTP/2 で 55,104 件エラー・2,375 rps、h2c で 28,843 件エラーといった形で同じ問題が
表面化していた（大きなレスポンス = 54KB では顕在化しない）。

## 併せて判明した性能上の問題

`aio` 経路は**性能的にも利点が無い**。POSIX AIO は 1 回の I/O に
`aio_read`/`aio_write`（submit）+ `aio_error` + `aio_return` の **3 syscall** を要し、
readiness 経路（`read`/`write` 1 発）より高コストである。DTrace 実測（54KB 静的
ファイル / HTTP/1.1 TLS / 約 174k リクエスト）でも 1 リクエストあたり
`aio_read` 1 / `aio_write` 1 / `aio_error` 2 / `aio_return` 2 = **6 syscall** を
消費していた。

スループット実測（FreeBSD 14.3 aarch64、サーバ 2 コア固定）:

| 計測 | `aio` あり | `aio` なし |
|---|---|---|
| HTTP/1.1 TLS・3 バイト | 105,520 rps | **187,374 rps（+77.6%）** |
| HTTP/1.1 TLS・54KB | 21,540 rps | 21,291 rps（有意差なし） |
| L4 TCP・3 バイト | 約 37,000 rps | **約 202,000 rps** |
| HTTP/2 TLS・3 バイト | 停止（本チケット） | 148,679 rps |

大きな転送でも改善しないため、AIO 経路には**そもそも採用の根拠が無い**。

## 対応

`Cargo.toml` の `full-freebsd` / `full-freebsd-aarch64` から `"aio"` を除外した
（F-145 と同じ変更にて）。feature 自体は残しているため、`--features aio` で
従来どおり明示的に有効化はできる。

## 未調査（根本原因）

`src/runtime/reactor/aio.rs` の完了待機ロジック（`SIGEV_KEVENT` による `EVFILT_AIO`
通知と `aio_error`/`aio_return` の刈り取り）に、HTTP/2 のように **1 コネクション上で
多数のストリームが同時に read/write を発行する**状況で起きる取りこぼしがあると
推測されるが、未調査。`aio` は既定オフになり実運用経路から外れたため優先度は低い。

調査の入口:

- 同一 fd に対する複数 AIO リクエストの `EVFILT_AIO` 通知と、fd 単位で管理している
  待機者テーブルの対応付け（1 fd 複数 in-flight の取り扱い）。
- `aio_error` が `EINPROGRESS` を返したときの再待機登録。
- kqueue changelist バッチ化（F-141）との相互作用（`EV_ONESHOT` の再登録漏れ）。

## 再現手順

```bash
# FreeBSD ゲスト内
cargo build --release --no-default-features --features full-freebsd-aarch64,aio
sh tools/perf/freebsd/run_perf_freebsd.sh -r 1 -d 10 -p /small.html h2_file_tls
```
