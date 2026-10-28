# API サーバの比較ベンチマーク

veil の API ライブラリを、同じ 2 つのエンドポイント（`GET /plaintext` → `Hello, World!`、`GET /json/{name}` →
`{"message":"Hello, <name>"}`）で次と比べる。結果とその読み方は `docs/guide/library.md` の「Performance」節。

| ディレクトリ | 中身 |
|---|---|
| `common.rs` | axum の Router と起動方法（`serve`・`percore`）。axum-tokio・axum-veil が `#[path]` で共有する |
| `axum-tokio/` | axum + 本物の tokio（基準） |
| `axum-veil/` | 同じ axum のコードを veil の tokio 互換レイヤーで（`[patch.crates-io] tokio` を `crates/veil-tokio/tokio-shim` に置き換え）。加えて `mount`（veil の `App` に axum の Router を `tokio_service` で載せる）と `native`（veil のネイティブハンドラ） |
| `python/` | FastAPI（uvicorn）と veil の Python バインディングで同じエンドポイント |
| `run_bench.py` | 計測ドライバ（交互計測・TSV・中央値の表） |
| `allocs.py` | veil のハンドラ経路の 1 要求あたりの確保回数（`axum-veil` の `count-allocs` ビルドを使う。F-202） |

起動モード（Rust）:

- `serve`: マルチスレッドのランタイム（ワーカー N 本）で `axum::serve` を 1 つ動かす。tokio では work stealing で全ワーカーに
  分散するが、veil::tokio はタスクが spawn したスレッドに固定されるので、受理したスレッド 1 本で全接続を処理する。
- `percore`: スレッドごとにカレントスレッドのランタイムと `SO_REUSEPORT` のリスナーを持つ（スレッド・パー・コア）。
- `mount` / `native`: veil の HTTP サーバ（ワーカーごとに受理）。`[server] threads` = N。

## ビルド

```bash
(cd tools/api-bench/axum-tokio && cargo build --release)
(cd tools/api-bench/axum-veil  && cargo build --release)
(cd tools/api-bench/axum-veil  && CARGO_TARGET_DIR=target-epoll cargo build --release --features epoll)   # reactor 版

V=target/py-bench-venv                       # run_bench.py の既定（BENCH_VENV で変更可）
python3 -m venv $V && $V/bin/pip install -r tools/api-bench/python/requirements.txt
(cd crates/veil-python && VIRTUAL_ENV=$PWD/../../$V PATH=$PWD/../../$V/bin:$PATH maturin develop --release)
```

負荷ツールは docker イメージ `local/wrk-openssl`（`WRK_IMG` で変更可）。

## 実行

```bash
python3 tools/api-bench/run_bench.py --group rust   --rounds 3 --out docs/artifacts/api_bench_rust.tsv
python3 tools/api-bench/run_bench.py --group python --rounds 3 --out docs/artifacts/api_bench_python.tsv
python3 tools/api-bench/run_bench.py --summary docs/artifacts/api_bench_rust.tsv      # 表だけ作り直す
```

- サーバは `BENCH_SERVER_CPUS`（既定 `0,1`）に taskset、wrk は `BENCH_CLIENT_CPUS`（既定 `2,3`）に docker の cpuset で分け、
  wrk のスレッド数はクライアントの CPU 数。**ハイパースレッドの兄弟を両側にまたがせない**（`lscpu -e` の CORE 列。
  サーバとクライアントが同じ物理コアを分け合うと、どちらも飽和しない）。
- サーバのスレッド数は `BENCH_THREADS`（既定 2）。uvicorn は `--workers` がプロセス数、veil の Python はスレッド数。
- ラウンドごとに対象の順番を回転・反転して交互に測る。`--only` で対象名の部分一致に絞れる。
- 列: `rps`・`p50_ms`・`p99_ms`（wrk）、`cpu_pct`（サーバのプロセスツリーの utime + stime）、`cores_pct`（サーバに割り当てた
  コアの busy。ループバックのソフト割り込みを含む）、`us_per_req`（`cores_pct` から求めた 1 要求あたりの CPU 時間）、`rss_mb`。
- 計測前に他の負荷が無いことを確かめる（`top`。tokio のテストハーネスの取り残しが CPU を使い続けていたことがある）。
- 待ち受けは `127.0.0.1:$BENCH_PORT`（既定 3000。`allocs.py` も同じ）。対象を起動する前にそのポートが応えたら止まる
  （Grafana の既定ポートでもあり、使われたまま測ると全対象が同じ数字になる。B-218）。`cpu_pct` が 0 の行は対象を測れていない。

## HTTP/2・HTTP/3（F-206）

```bash
(cd tools/api-bench/axum-veil && CARGO_TARGET_DIR=target-h3 cargo build --release --features http3 && \
   CARGO_TARGET_DIR=target-h3-epoll cargo build --release --features http3,epoll)
python3 tools/api-bench/run_bench.py --group rust --proto h2c --only "percore,native" --out ...   # h2load（docker local/h2load）
python3 tools/api-bench/run_bench.py --group rust --proto h3 --out ...                          # h2load-h3（local/h2load-h3）
```

- `--proto h2c` は h2load の prior knowledge（`-c` 接続 × `-m`（`H2LOAD_STREAMS`、既定 10）本の同時ストリーム）。axum も
  `http2` feature で h2c を受ける。
- `--proto h3` は veil のネイティブハンドラだけ（axum に HTTP/3 は無い）。`target/api-bench-{cert,key}.pem` に自己署名の
  証明書を作り、`BENCH_TLS_CERT`・`BENCH_TLS_KEY` で TLS + HTTP/3 を同じアドレスで受ける。
- h2load は p50/p99 を出さないので、`p50_ms` の列に平均（`time for request` の mean）を入れる。

## 改修前後の交互 A/B と確保の回数（F-202）

```bash
git worktree add /path/to/base <改修前のコミット>
(cd /path/to/base/tools/api-bench/axum-veil && cargo build --release && \
   CARGO_TARGET_DIR=target-epoll cargo build --release --features epoll)
BENCH_BASE_DIR=/path/to/base python3 tools/api-bench/run_bench.py --group rust --only "native handler,(mount)"
```

`BENCH_BASE_DIR` を渡すと、その作業ツリーの `axum-veil` で `mount`・`native`（io_uring・epoll）を「[base]」として足し、
同じラウンドの中で交互に測る。
`--proto h3` では、その作業ツリーの `target-h3`・`target-h3-epoll` のネイティブハンドラを「[base]」として足す
（`axum-veil` に `http3` feature が無い古いコミットは、今の `tools/api-bench/axum-veil` と `common.rs` を写してからビルドする）。

```bash
(cd tools/api-bench/axum-veil && CARGO_TARGET_DIR=target-count cargo build --release --features count-allocs)
python3 tools/api-bench/allocs.py [--mode native|mount] [--path /plaintext|/json/veil] [--bin <path>]
ALLOC_TRACE=60 tools/api-bench/axum-veil/target-count/release/api-bench-axum-veil native 127.0.0.1:3000 1
```

`count-allocs` のビルドは確保（alloc・realloc）の累計を 1 秒ごとに標準エラーへ出し、`allocs.py` は wrk の負荷の
前後の差を要求数で割る。`ALLOC_TRACE=<n>` は起動 4 秒後から確保 n 回ぶんのバックトレースを出す（どこで確保しているか。
行番号が要るときは `CARGO_PROFILE_RELEASE_DEBUG=line-tables-only` でビルドする）。

