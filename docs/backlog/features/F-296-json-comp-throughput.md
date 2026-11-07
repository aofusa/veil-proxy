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
2. `[api] compression = true` の既定の優先順を `["gzip", "br"]` にする（ハンドラの応答だけ。`[route.compression]` の既定は変えない）。

## 調べて採らなかったこと（aofish WSL、8 スレッド、256 接続、25/40/50 件の巡回）

本番と同じ条件で品質を振った。列の補正は `rps × (1471 / 応答の大きさ)²`（HttpArena の補正の形）。

| 設定 | rps | 応答の平均 | 補正後 |
|---|---|---|---|
| br q2 | 67.4k | 1556 B | 60.2k |
| br q3 | 62.0k | 1503 B | 59.4k |
| br q4（既定の品質） | 47.8k | 1471 B | 47.8k |
| br q5 | 38.3k | 1374 B | 43.9k |
| br q6 | 4.5k | 1363 B | 4.4k |
| gzip L1 | 99.8k | 1899 B | 59.9k |
| gzip L2 | 76.0k | 1677 B | 58.5k |
| gzip L3 | 72.2k | 1597 B | 61.2k |
| **gzip L4（既定の品質）** | **68.0k** | **1492 B** | **66.1k** |
| gzip L6 | 63.3k | 1476 B | 62.9k |

- gzip L4 は br q3 より速くて小さく、br q4 とほぼ同じ大きさで CPU が 3 割少ない。品質は変えず、順だけを変える。
- `flate2` の zlib-rs バックエンド: 最良の L6（72.3k、1508 B）で補正後 68.8k。+4% のために依存を足さない。
- gzip の圧縮器のスレッドローカルでの使い回し: 1.01×（`reset` も同じだけ 0 埋めする）。外した。
- br のハッシュ表の縮小: q4 以下は crate の中で 2^16〜2^17 の固定の表で、差し替えられない。
- 0 埋めしない再利用: 前回の値が残ると brotli が panic する（表の値をそのまま添字に使う）。
- br q6 以上は応答ごとに 2 MiB 以上のハッシュ表を確保し、veil-alloc の huge（`mmap`/`munmap`）を通るので 1 桁落ちる。
  huge は手元に残さない設計（F-183）なので、アロケータは変えずに文書へ書いた（`docs/guide/library-production.md`）。

残りの CPU（gzip の 119 µs/req）の 6 割は miniz_oxide の LZ77 で、圧縮器を替えない限り縮まない。HttpArena の veil-api の
`#[serde(flatten)]` を外すと直列化が少し軽くなるが、効果は数 % で、これは利用者のコードの側の話。

## 計測

`python3 tools/perf/api_ab.py --scenario json`（無圧縮・gzip・br を別の行で。交互 A/B の中央値）。

## 受け入れ条件

- br + gzip の rps が改修前の数倍以上。無圧縮に対する圧縮の増分が小さくなる。
- 応答の大きさが大きく悪化しない（`gzip, br` は 1471 B → 1492 B。補正後の値で比べて良い方を選ぶ）。
- `mmap`/`munmap` が 1 要求ごとに起きない（strace で確認）。

## 結果（aofish WSL、3 ラウンドの交互 A/B の中央値）

| 行 | 改修後 | 改修前 | 比 | CPU/req |
|---|---|---|---|---|
| 無圧縮 | 179,590 | 160,474 | 1.12× | 43.8 / 49.3 µs |
| gzip | 66,671 | 66,154 | 1.01× | 118.9 / 119.3 µs |
| `gzip, br`（HttpArena の形） | 66,487 | 13,432 | **4.95×** | 119.4 / 283.9 µs |

`gzip, br` の p99 は 29.8 ms → 8.0 ms。補正後の値は 4.8×。8 論理 CPU での値で、64 論理 CPU の外部計測は
`mmap` のロックの競合が無くなった分だけ改修前より伸びが良いはず（外部の再計測は未実施）。

## 依存・リスク

- 大きな本文（1 MiB 以上）のメモリ: ワンショットは出力を全量持つが、これは現状の `compress` と同じ。
