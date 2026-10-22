# docs/perf — パフォーマンス計測サマリ（v0.8.3）

Veil の HTTP/1.1・HTTP/2・h2c・HTTP/3・gRPC・WebSocket・L4 のスループットを、nginx を基準に計測した
**最新の結果だけ**をまとめる（過去の計測は git 履歴で辿れる）。

| ファイル | 内容 |
|---|---|
| [`linux_suite_ab_raw.tsv`](linux_suite_ab_raw.tsv) | Linux x86_64 のフルスイート（`suite_ab.sh`、v0.8.2 と交互、nginx の同じ仕事の相手を含む）の生データ |
| [`linux_remeasure_raw.tsv`](linux_remeasure_raw.tsv) | 上で v0.8.2 との比が 0.95 を下回った 2 構成を 5 反復で測り直した生データ |
| [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) | FreeBSD 14.3 aarch64 の `run_perf_bsd.sh`（標準・サーバ律速 × 54KB・3B）の生データ |
| [`freebsd_cpu_per_req.tsv`](freebsd_cpu_per_req.tsv) | FreeBSD のサーバの CPU 時間 / 要求（`cpu_per_req.sh`）の生データ |
| [`platform_verification.md`](platform_verification.md) | v0.8.3 の全プラットフォームの試験（単体・統合・E2E・ライブラリ・セキュリティ） |

---

## Linux x86_64（2026-10-22、Docker コンテナ間）

- 比較:
  - v0.8.3 の main（`637989f4` 相当。v0.8.3 のタグ `ff15ea56` + HTTP/2 の END_STREAM の改善 `403f79bf` + 試験の修正）、イメージは `veil:v083-ab`
  - v0.8.2（`d8f9d6c0`、`veil:v082-ab`）
  - `nginx:alpine`
- イメージ: どちらも `docker/Dockerfile.glibc`（`full`、io_uring、`--profile dist`）
- 実行: `BASE=v082-ab NEW=v083-ab bash tools/perf/suite_ab.sh`（69 構成 × 3 反復、構成ごとに順を入れ替えて交互、3.7 時間）
- **全 834 計測で Non-2xx = 0、NA 行ゼロ**

### nginx との比較（同じ仕事の相手、Req/s 中央値）

対応は `tools/perf/vs_nginx.py` の表のとおり。

- 中継: `base_proxy`（TLS で受けて上流へ中継）
- 圧縮: `base_compression`（毎回 gzip）
- 中継 + 圧縮: `base_proxy_compression`
- HTTP/3: `base`・`base_proxy` に `quic` を足したもの
- それ以外: `base`（TLS 静的）

| 構成 | プロトコル | nginx | veil | 対 nginx |
|---|---|---|---|---|
| `h2_1_ktls_0_lb_kernel_ofc_1`（54KB 静的） | HTTP/1.1 | 6,284（`base`） | 9,118 | **1.45×** |
| 同上 | HTTP/2 | 5,163（`base`） | 9,006 | **1.74×** |
| `h2_1_feat_proxy`（54KB 中継） | HTTP/1.1 | 4,521（`base_proxy`） | 6,262 | **1.39×** |
| 同上 | HTTP/2 | 3,418（`base_proxy`） | 6,570 | **1.92×** |
| `h2_1_feat_compression`（毎回圧縮） | HTTP/1.1 | 2,199（`base_compression`） | 8,380 | **3.81×** |
| 同上 | HTTP/2 | 2,072（`base_compression`） | 3,926 | **1.90×** |
| `h2_1_proxy_compression`（中継 + 圧縮） | HTTP/1.1 | 1,974（`base_proxy_compression`） | 2,444 | **1.24×** |
| 同上 | HTTP/2 | 1,792（`base_proxy_compression`） | 3,336 | **1.86×** |
| `h2_1_feat_compression_cached` | HTTP/2 | 5,163（`base`） | 24,816 | **4.81×** |
| `h3_file_metrics`（54KB 静的） | HTTP/3 | 1,982（`base`） | 2,498 | **1.26×** |
| `h3_proxy`（54KB 中継） | HTTP/3 | 1,830（`base_proxy`） | 2,184 | **1.19×** |
| `h2_0_feat_l4`（L4 平文） | HTTP/1.1 | 6,284（`base`） | 12,814 | **2.04×** |
| `h2c_file`（54KB 静的） | h2c | 8,014（`h2c_file`） | 27,717 | **3.46×** |
| `h2c_proxy`（54KB 中継） | h2c | 6,302（`h2c_proxy`） | 10,370 | **1.65×** |

| プロトコル | 構成数 | 対 nginx の中央値 | 最小 | nginx 未満 |
|---|---|---|---|---|
| HTTP/1.1 | 54 | **1.34×** | 1.05 | 0 |
| HTTP/2 | 45 | **1.75×** | 1.30 | 0 |
| HTTP/3 | 19 | **1.23×** | 1.15 | 0 |
| h2c | 2 | **2.55×** | 1.65 | 0 |

- 全 120 構成で nginx 以上。
- 1.0 付近まで下がるのは、kTLS を有効にした構成（`*_ktls_1_*`）の HTTP/1.1 だけ（最小 1.05×）。コンテナ（veth）では kTLS が不利で、
  nginx 側は kTLS 無しの `base`。
- gRPC（`grpc_h2_*` 約 5,000 req/s、`grpc_h3_*` 約 21,000 req/s）と WebSocket は、nginx の相手を置いていない。

### v0.8.2 との退行確認（交互 A/B）

| プロトコル | 組数 | 比の中央値 | 最小 | 最大 |
|---|---|---|---|---|
| HTTP/1.1 | 54 | 0.998 | 0.966 | 1.026 |
| HTTP/2 | 45 | 0.999 | 0.944 | 1.046 |
| HTTP/3 | 19 | 0.994 | 0.966 | 1.035 |
| h2c | 2 | 1.003 | 0.998 | 1.008 |
| gRPC | 6 | 0.994 | 0.979 | 1.027 |
| gRPC over HTTP/3 | 6 | 0.992 | 0.938 | 1.014 |
| WebSocket | 1 | 1.004 | — | — |
| **全体** | 133 | **0.998** | — | — |

0.95 を下回った 2 組は、その構成だけ 5 反復で測り直した（`linux_remeasure_raw.tsv`）。

- `grpc_h3_wasm`: 1.016
- `h2_1_feat_compression`: HTTP/1.1 0.994、HTTP/2 0.999

**退行なし。** v0.8.3 の変更による性能の影響は、計測の揺れの範囲だった。

- quiche の FFI を aws-lc-sys のバインディング経由にした（F-243）。`#[inline(always)]` の包みで、呼び出し先は同じ関数。HTTP/3 は 0.994。
- ログの文の英語化（F-242）は、文字列の変更だけ。
- HTTP/2 の END_STREAM を最後の DATA フレームに付ける改善は、Linux では揺れの範囲（HTTP/2 0.999）。
  FreeBSD の h2c 54KB では +9%（下）。

---

## FreeBSD 14.3 aarch64（2026-10-21、QEMU + HVF、ネイティブ）

- コミット: `ff15ea56`（v0.8.3）。VM のメモリ 8GB。
- 実行: `tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10`（54KB と `-p /small.html` の 3B）。
  - サーバ律速は `SRV_CPUS=0 GEN_CPUS=1,2,3 UP_CPUS=1,2,3 SRV_THREADS=1 LOAD_THREADS=3`。
  - CPU/要求は `tools/perf/bsd/cpu_per_req.sh`。
- **全計測でエラー 0**

### 対 nginx

「標準」は計測対象 2 コア・負荷 2 コア、「サーバ律速」は計測対象 1 コア・負荷 3 コアの rps の比（4 反復の中央値）。
「効率」はサーバの CPU 時間 / 要求の比（nginx / veil。1 より大きいほど veil が少ない CPU で返す）。

| シナリオ | 大きさ | 標準 | サーバ律速 | CPU/要求 veil | nginx | 効率 |
|---|---|---|---|---|---|---|
| `h1_file_tls`（HTTPS 静的） | 3B | 0.99 | **1.09** | 3.5µs | 4.4µs | **1.26** |
| `h2_file_tls`（HTTP/2 静的） | 3B | **1.45** | **1.29** | 2.2µs | 3.4µs | **1.54** |
| `h1_file_plain`（平文 HTTP/1.1 静的） | 3B | **1.07** | **1.11** | 2.6µs | 2.9µs | **1.11** |
| `h2c_file_plain`（h2c 静的） | 3B | **2.36** | **1.45** | 2.2µs | 3.9µs | **1.76** |
| `h3_file`（HTTP/3 静的） | 3B | **1.07** | 0.94 | 4.8µs | 5.0µs | **1.06** |
| `h1_proxy_tls`（HTTPS 中継） | 3B | 0.93 | 1.02 | 7.0µs | 7.2µs | 1.03 |
| `h2_proxy_tls`（HTTP/2 中継） | 3B | **5.26** | **2.13** | 7.9µs | 9.8µs | **1.24** |
| `l4_tcp`（L4 TCP） | 3B | 0.92※ | 0.91※ | 3.5µs | 3.5µs | 1.00 |
| `h1_file_tls` | 54KB | **1.33** | **1.28** | 39.0µs | 51.6µs | **1.32** |
| `h2_file_tls` | 54KB | **1.30** | **1.52** | 32.0µs | 50.2µs | **1.57** |
| `h1_file_plain` | 54KB | 1.00 | 0.97 | 5.9µs | 6.3µs | **1.07** |
| `h2c_file_plain` | 54KB | 0.89 | **1.23** | 7.3µs | 11.8µs | **1.62** |
| `h3_file` | 54KB | 0.96 | 0.97 | 147.7µs | 136.4µs | 0.92 |
| `h1_proxy_tls` | 54KB | **1.13** | **1.11** | 65.1µs | 73.5µs | **1.13** |
| `h2_proxy_tls` | 54KB | **1.35** | **1.46** | 62.6µs | 95.8µs | **1.53** |
| `l4_tcp` | 54KB | **1.12** | **1.12** | 29.7µs | 34.7µs | **1.17** |

読み方と、1 を下回った行の調べた結果:

- **VM の rps はホストの負荷で大きく揺れる。** 同じバイナリでもラウンドごとに 3 割動く。0.90〜1.05 の行は誤差の範囲で、効率は CPU/要求で見る
  （効率は 16 項目中 15 項目で 1.00 以上）。
- ※ `l4_tcp` 3B は、反復の順で偏っていた（nginx を先に測った回だけ nginx が大きい）。サーバ律速を 8 反復で測り直すと、
  ラウンドごとの比は 0.88〜1.32、中央値 **1.07**。syscall は veil 5.1 / 要求に対し nginx 8.2。
- `h2c_file_plain` 54KB の標準（負荷生成が律速。上の表は v0.8.3 のタグ）は 0.89。
  - サーバの CPU は veil が 4 割少なく、サーバ律速では 1.23。
  - 1 接続あたりの多重度で変わる: `-m1` 1.02、`-m32` 0.94（100 万要求の交互）。
  - 原因: 応答の終わりに長さ 0 の DATA フレーム（END_STREAM）を別に送っていた（16384×3 + 5424 + 0）。END_STREAM を最後の DATA フレームに
    付けるようにした（`403f79bf`、v0.8.3 の後）。
  - 交互 3 反復で h2c 54KB は 101.9k → 111.3k req/s（**+9.2%、対 nginx 0.98**）。TLS の HTTP/2 は 5 反復で静的 +6.1%・中継 0.98（揺れの範囲）。
  - 次のものは違いにならなかった: 共有メモリ + `sendfile(2)` の有無（無効にすると 5% 下がる）、TCP のセグメント数
    （veil 3.5 / nginx 4.6 / 要求）、書き込みの閾値（128KB → 32KB）。
- `h3_file` 54KB の効率 0.92（スループットは同等）は、v0.8.0 と同じ残り。
  - 上位は `memcpy`（本文をパケットへ詰めるコピー。nginx も同じ）、`malloc`/`free`（quiche の内部と FreeBSD の libc の jemalloc）、
    quiche の送出。
  - FreeBSD の quiche（`boringssl-boring-crate`）は v0.8.3 の変更（F-243）の対象外。
  - FreeBSD arm64 の DTrace はユーザ空間のスタックを辿れない（`ustack()` が空。フレームポインタ付きのビルドでも同じ）ため、
    呼び出し元までは分けられなかった。
- 配布物（`--profile dist`、fat LTO）と release の CPU/要求の差は 1% 以内（v0.8.0 で確認）。

---

## 計測条件

### Linux（`tools/perf/`）

- Docker のコンテナ間通信（同じホストの 4 コアを veil・nginx・上流・負荷生成で共有）。kTLS は直交表の `ktls` 因子でだけ有効
- 負荷: HTTP/1.1 は `wrk -t4 -c100 -d10s`、HTTP/2・HTTP/3 は `h2load -t4 -n 30000 -c100 -m10`、h2c は `-n 120000`
- 配信: `/`（54,576B の index.html）。上流は nginx（平文 HTTP/1.1）

### FreeBSD（`tools/perf/bsd/`）

- ゲストの中のループバック。計測対象は 0,1 コア（サーバ律速は 0）、負荷生成と上流（別の nginx）は 2,3 コア（同 1,2,3）
- 負荷: HTTP/1.1 は `wrk -t2 -c64`、HTTP/2・h2c は `h2load -t2 -c64 -m32`、HTTP/3 は `h3load -t2 -c64 -m32`（quinn + h3）
- 静的配信は `[static_file_cache]` + `open_file_cache`（nginx も `open_file_cache`）
- 手で A/B するときは、バイナリの名前を `veil` のままにする（`pkill -x veil` で止まらない名前にすると、`SO_REUSEPORT` で
  前のプロセスが同じポートで受け続け、計測が混ざる。v0.8.3 の調査で実際に踏んだ）

## 再現手順

### Linux（Docker）

```bash
docker build -f docker/Dockerfile.glibc -t veil:new .
git worktree add /tmp/base v0.8.2 && (cd /tmp/base && docker build -f docker/Dockerfile.glibc -t veil:base .)
bash tools/perf/gen_configs.sh
BASE=base NEW=new bash tools/perf/suite_ab.sh
python3 tools/perf/suite_ab_summary.py tools/perf/results/suite_ab/<日時>.tsv base new   # 対 v0.8.2
python3 tools/perf/vs_nginx.py tools/perf/results/suite_ab/<日時>.tsv veil_new           # 対 nginx
```

### FreeBSD（QEMU VM）

```bash
tools/qemu/bsd-vm.sh freebsd aarch64 up
tools/qemu/bsd-vm.sh freebsd aarch64 build
# ゲスト内（pkg install -y nginx nghttp2 wrk-luajit curl、h3load をビルド）:
cargo build --release --manifest-path tools/perf/h3load/Cargo.toml
sh tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10                        # 54KB
sh tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10 -p /small.html         # 3B
SRV_CPUS=0 GEN_CPUS=1,2,3 UP_CPUS=1,2,3 SRV_THREADS=1 LOAD_THREADS=3 \
  sh tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10                      # サーバ律速
sh tools/perf/bsd/cpu_per_req.sh                                    # CPU/要求
```
