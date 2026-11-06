# B-331: brotli の圧縮が要求ごとに 8 MiB の領域を mmap/munmap する

## 事象

API の応答圧縮で br を選ぶと、サーバの CPU の半分以上がカーネル時間になり、スループットが無圧縮の 1/12 になる
（HttpArena の json-comp が 36.7k req/s の原因）。256 接続・8 スレッドで 155,729 rps（無圧縮）→ 12,572 rps（br）。
stime 14.25 s が utime 11.4 s を上回る。

## 影響

`[api] compression`（ライブラリの既定は br を優先）と、同じ関数を使う経路すべて。プロキシの応答圧縮の br も確認する。

## 調査メモ

`brotli::BrotliCompress` は入力を 4096 バイトずつ流す。2 回目のチャンクでリングバッファが全量（`lgwin = 22` で約 8 MiB）へ
伸びるので、1 MiB 超の確保が veil-alloc の huge 経路（mmap/munmap を直接）に落ち、要求ごとの `mmap` + ページフォールト + `munmap` に
なる。8 スレッドで `mmap_lock` も奪い合う。

マイクロベンチ（Zen 5、1 スレッド、平均入力 6431 B、q4）:

| 方法 | 時間 | 出力 |
|---|---|---|
| `BrotliCompress` | 141〜148 µs | 1241 B |
| ワンショット（全量を入力して `FINISH`） | 39 µs | 1241 B |

ワンショットはリングバッファを本文の大きさだけで確保するので同じバイト列を 3.8 倍速く出す。

- ハッシュ表を 0 埋めせずに使い回す案は不可（古い内容が `backward_references` の範囲外アクセスで panic）。
- `lgwin` を 16 以下にすると q5 以上で別のハッシャ（40〜42）が選ばれ、Rust 移植では極端に遅い（1289 µs）。q5 以上は 17 以上を保つ。

## 改修案

`compress_stream` に本文全量を渡し `BROTLI_OPERATION_FINISH` で 1 回で圧縮する。出力は本文より少し大きい容量を先に取る。
`size_hint` に本文の長さを渡す。同じ `BrotliCompress` の使い方をしている箇所も同じ関数へ寄せる。

## 受け入れ条件

- 要求ごとの mmap/munmap が消える（strace）。応答のバイト列は改修前と同じ。
- `api_ab.py --scenario json` の br の行が数倍以上。
