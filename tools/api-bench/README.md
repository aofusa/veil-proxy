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
