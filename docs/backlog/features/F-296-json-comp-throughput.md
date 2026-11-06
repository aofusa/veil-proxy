# F-296: JSON + 圧縮のスループット（json-comp）

親: [F-295](F-295-v0120-perf-harness.md)。原因の本体は [B-331](../bugs/B-331-brotli-huge-mapping-per-request.md)。

## 現状

HttpArena の json-comp（`GET /json/{count}?m=`、`Accept-Encoding: gzip, br`、事前計算・応答キャッシュは禁止）で
36.7k req/s。axum 268k、actix 529k、fastapi 101k。ランキングは bytes per response の二乗で補正される。

## 再現と分解（改修前、サーバ 8 スレッド CPU 0-7、負荷 8 スレッド CPU 8-15、256 接続、25/40/50 件の巡回）

| 応答 | rps | p99 | 応答の平均 |
|---|---|---|---|
| 無圧縮 | 155,729 | 2.77 ms | 6621 B |
| gzip | 61,060 | - | 1492 B |
| br + gzip（br が選ばれる） | 12,572 | 29.8 ms | 1471 B |

1 要求あたり: 直列化と送信 約 51 µs、gzip +80 µs、br +590 µs。br の CPU の半分以上がカーネル時間（mmap/munmap とページフォールト）。

## 仮説と結果

1. br の遅さは圧縮そのものではない。→ 同じ入力で `BrotliCompress` が 141〜148 µs、ワンショットが 39 µs、出力は同じ（[B-331](../bugs/B-331-brotli-huge-mapping-per-request.md)）。
2. gzip も単体（約 20 µs）に比べて本番の経路（約 80 µs）が遅い。→ 調査中（改修内容 3）。
3. 無圧縮の 51 µs も重い（Rust の `serde(flatten)` の直列化と 6.4 KB の送信）。→ `flatten` は serde の `Content` バッファを経由するので遅いが、これはハンドラ（利用者のコード）の書き方。本体側の余計なコピーの有無を見る。

## 改修内容

1. br はワンショットの `compress_stream`（[B-331](../bugs/B-331-brotli-huge-mapping-per-request.md)）。ウィンドウは本文の大きさから決めてメモリを抑える。
2. 圧縮のバッファをスレッドローカルで使い回し、応答ごとの確保と 0 埋めを減らす。品質の既定はそのまま（圧縮率の補正が二乗で効く）。
3. gzip の差を調べて縮める（`flate2` の `reset` のコスト、出力バッファの確保）。
4. レベルの既定の見直しは計測で決める（速度と圧縮率の両方を二乗の補正に入れて比べる）。

## 計測

`python3 tools/perf/api_ab.py --scenario json`（無圧縮・gzip・br を別の行で。交互 A/B の中央値）。

## 受け入れ条件

- br + gzip の rps が改修前の数倍以上。無圧縮に対する圧縮の増分が小さくなる。
- 応答の大きさが改修前より悪化しない（同じ品質で同じバイト列）。
- `mmap`/`munmap` が 1 要求ごとに起きない（strace で確認）。

## 依存・リスク

- 大きな本文（1 MiB 以上）のメモリ: ワンショットは出力を全量持つが、これは現状の `compress` と同じ。
