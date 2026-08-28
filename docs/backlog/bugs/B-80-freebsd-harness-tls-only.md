# B-80: FreeBSD 計測ハーネスの `h1_file_plain` が F-163 以降 0 rps になっていた

## 事象

`tools/perf/freebsd/run_perf_freebsd.sh` の `h1_file_plain` シナリオで、
**veil が 0.00 rps・全リクエストエラー**になる（nginx 側は正常）。

```
scenario        server  iter  rps       mbps      p50_ms   p99_ms    errors
h1_file_plain   nginx   1     81212.89  33996.80  0.419    0.766     0
h1_file_plain   veil    1     0.00      0.00      0.000    0.000     2175920
h1_file_plain   nginx   2     59190.77  24739.84  0.574    1.100     0
h1_file_plain   veil    2     0.00      0.00      0.000    0.000     1794961
h1_file_plain   nginx   3     59268.97  24821.76  0.573    1.180     0
h1_file_plain   veil    3     0.00      0.00      0.000    0.000     1734945
```

## 原因

**F-163 が `[server].tls_only` の既定を `true` に変更した**ため。

veil に平文 HTTP/1.1 を喋らせる経路は **メインリスナーのプロトコル検出**
（`detect_protocol_with_buffer` → `accept_plain`）**しか無い**
（`h2c_listen` は h2c 専用で、平文 HTTP/1.1 は
「Plain HTTP/1.1 not supported on H2C-only server」として切断される。F-155 で明文化済み）。

ところが `tls_only = true` のときはそのプロトコル検出（MSG_PEEK）**自体を行わない**ため、
平文 HTTP/1.1 の到達手段が完全に消える。ハーネスは F-163 より前に書かれており、
生成する計測用設定に `tls_only` を明示していなかった。

**ハーネス側の追随漏れであり、veil 本体の不具合ではない。**

## なぜ今まで気づかなかったか

FreeBSD ネイティブ計測の前回実施は **2026-08-07〜08**、F-163 は **2026-08-26** で、
**F-163 以降これが初めての FreeBSD 計測**だったため。
Linux 側の `tools/perf` は h2c 専用ポートを使う構成なので影響を受けない。

## 改修

ハーネスが生成する veil 設定の `[server]` に `tls_only = false` を明示する
（理由をコメントで併記）。

## 教訓

**既定値を変える変更（F-163）は、その既定に暗黙に依存している計測ハーネス・テストを
壊しうる。** しかも壊れ方が「0 rps」なので、集計だけ見ていると
「その構成は測れていない」ではなく「性能が出ていない」と誤読しやすい。
**errors 列が跳ねている行は、スループットの数字より先に見ること。**
