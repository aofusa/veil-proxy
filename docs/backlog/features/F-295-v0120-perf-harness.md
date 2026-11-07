# F-295: v0.12.0 の性能改善の親チケットと計測基盤（`tools/perf/`）

**状態: 完了（feat/v0120-perf、2026-11-07）**

## 目的

外部ベンチマーク（HttpArena・the-benchmarker）への提出で見えた遅延・スループットの問題を v0.12.0 で解消する。
対象は 4 領域で、それぞれを子チケットで扱う。

| 領域 | チケット | 外部の値（v0.10.1） | 目標 |
|---|---|---|---|
| JSON + 圧縮 | [F-296](F-296-json-comp-throughput.md)・[B-331](../bugs/B-331-brotli-huge-mapping-per-request.md) | json-comp 36.7k req/s（CPU 約 900%） | 最低でも数倍、理想は数十万 req/s |
| コア制限下の遅延 | [F-297](F-297-core-limited-latency.md)・[B-332](../bugs/B-332-reactor-hot-connection-starves-others.md) | latency-500k-8cpu rate 0.7455、p99 数百万 µs | 8 CPU 固定で rate を維持し p99 を大きく下げる |
| gRPC | [F-298](F-298-grpc-unary-fast-path.md)・[B-333](../bugs/B-333-h2-trailers-flush-per-stream.md) | unary の p99 が悪い | スループットを落とさず p99/p99.9 を改善 |
| Python | [F-299](F-299-python-gil-lane.md)・[B-334](../bugs/B-334-python-thread-state-per-call.md)・[B-335](../bugs/B-335-access-log-writer-wake-per-request.md) | 25.5k plaintext / 22.6k json（2 スレッド、offload=True） | 50k 以上、できれば 70k（1 プロセス、2〜数スレッド） |
| io_uring（途中で発見） | [F-300](F-300-iouring-batched-submission.md) | 既定の Linux ビルドの 1 要求あたりの CPU が epoll 版より 4 割多い | 全シナリオで epoll 版と同等以上 |

守ること: データプレーンは Rust のまま（io_uring / epoll / kTLS / rustls）。Python は `offload=True` を既定に保つ。
multiprocess は採らない（GIL はハンドラプールと永続イベントループで吸収する）。改善は交互 A/B の中央値で確かめる。

## 現状（計測基盤の不足）

- `tools/perf/` の既存のハーネスは Docker 上で nginx と比べるもので、API ライブラリの経路（ハンドラ・圧縮・gRPC）と
  「固定レートの遅延」「コア数の制限」を測れない。
- `wrk`・`h2load` は固定レートの測定（Coordinated Omission の補正）と gRPC の遅延分布を取れない。

## 改修内容

- `tools/perf/veil-load/`: 計測用の負荷ツール（libc だけに依存、本体のワークスペースへ入れない）。
  HTTP/1.1・h2c・gRPC（5 バイトの枠付きの本文）、閉ループと固定レート（予定時刻から測るので CO を含む）、
  複数パスの巡回、CPU の固定、TSV 出力。
- `tools/perf/api-app/`: HttpArena の `veil-api` と同じ形のハンドラ（`/baseline11`・`/json/{count}`・
  unary gRPC）を持つ API サーバ。A/B は同じソースを改修前の作業ツリーへ置いてビルドする。
- `tools/perf/api_ab.py`: 領域ごとのシナリオを交互に回して TSV と中央値の表を出すドライバ。シナリオは
  `json`（識別・gzip・br を分けて直列化と圧縮のコストを分離）、`latency`（サーバを CPU に固定し負荷と分けた
  固定レート）、`grpc`（閉ループの遅延分布）、`python`（`tools/api-bench/python` のアプリ）。
- 1 要求あたりのシステムコール数を数える LD_PRELOAD の小片（`tools/perf/syscall_count/`）。

## 計測の前提

- ホスト: aofish の WSL（AMD Ryzen AI Z2 Extreme、8C/16T、カーネル 6.6）。ノート PC は他の作業の負荷で A/B に使えない。
- サーバを CPU 0-7、負荷を CPU 8-15 に分ける（`lscpu -e` の CORE 列で兄弟が同じ物理コアになる）。
- 8 CPU の飽和スループットは外部の latency-500k-8cpu の結果とほぼ同じ（v0.11.0: 378k req/s。外部: 372k）。

## 計測で踏んだこと

- **固定レートの負荷が軽すぎた。** 最初の `veil-load` は空いた接続を LIFO で使い、次の予定まで回り続けていた。同じ接続ばかり使うと
  読み書きのキャッシュが温まり、回り続けると（`epoll_wait(0)`）ループバックの相手を起こす費用がサーバの `send(2)` から消えて
  システム時間が半分になる。改修前の版が外部の結果（rate 0.7455）を再現せず 500k を維持してしまったことで気づいた。全接続を FIFO で回し、
  次の予定まで `epoll_pwait2` で眠る形に直した。**これより前の固定レートの HTTP/1.1 の数値は無効。**
- `--syscalls`（LD_PRELOAD）は 1 回ごとに費用が足されるので、飽和に近い固定レートの遅延を見る回では付けない（p99 が 1 桁変わる）。
- gRPC は閉ループだと遅延がスループットの裏返しになるので、固定レートの行（`grpc-rate-<N>k`）を足した。
- `docs/perf/` の最終の数値は、WSL の木をリポジトリとハッシュで突き合わせてから測った（フューチャの大きさを出す一時パッチが残った
  ビルドで測った回があった）。

## 受け入れ条件

- 上の 4 領域をそれぞれ 1 コマンドで回せて、A/B の中央値の表が出る。
- 手順が `tools/perf/README.md` にある。結果の要約が `docs/perf/` にある。

## 依存・リスク

- `[[bench]]`・`[[example]]` は足さない（コンテナビルドが壊れる）。ツールは空の `[workspace]` を持つ独立クレートにする。
