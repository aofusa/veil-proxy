# docs/perf — パフォーマンス計測サマリ（v0.8.0）

Veil の HTTP/1.1・HTTP/2・h2c・HTTP/3・gRPC・WebSocket・L4 のスループットを、nginx を基準に計測した
**最新の結果だけ**をまとめる（過去の計測は git 履歴で辿れる）。

| ファイル | 内容 |
|---|---|
| [`linux_suite_ab_raw.tsv`](linux_suite_ab_raw.tsv) | Linux x86_64 のフルスイート（`suite_ab.sh`、main と交互、nginx の同じ仕事の相手を含む）の生データ |
| [`linux_h2c_raw.tsv`](linux_h2c_raw.tsv) | h2c の 2 構成だけを 12 万要求・5 反復で測り直した生データ（上のスイートの h2c は 1 秒で終わり揺れが大きい） |
| [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) | FreeBSD 14.3 aarch64 の `run_perf_bsd.sh`（標準・サーバ律速 × 54KB・3B）の生データ |
| [`freebsd_cpu_per_req.tsv`](freebsd_cpu_per_req.tsv) | FreeBSD のサーバの CPU 時間 / 要求（`cpu_per_req.sh`）の生データ |
| [`platform_verification.md`](platform_verification.md) | v0.8.0 の全プラットフォームの試験（単体・統合・E2E・ライブラリ・セキュリティ） |

v0.8.0 の性能の改善の内容は [F-240](../backlog/features/F-240-v080-perf-vs-nginx.md)、計測の見直しは
[B-153](../backlog/bugs/B-153-bsd-perf-harness-bias.md) と [tools/perf/README.md](../../tools/perf/README.md)。

---

## Linux x86_64（2026-10-18、Docker コンテナ間）

- コミット: `1dc3d4aa`（feat/v080-release）。HTTP/3 の送出のコピーとタイマーの削減（`102cccea`）は下の HTTP/3 の A/B で確かめた。比較対象は main（`d0e026bb` のメッシュの後のコード、`veil:mesh-ab`）と `nginx:alpine`
- イメージ: `veil:glibc`（`full`、io_uring、`--profile dist`）
- 実行: `BASE=mesh-ab NEW=glibc bash tools/perf/suite_ab.sh`（69 構成 × 3 反復、構成ごとに順を入れ替えて交互）
- **全 798 計測で Non-2xx = 0、NA 行ゼロ**

### nginx との比較（同じ仕事の相手、Req/s 中央値）

v0.8.0 で nginx に同じ仕事の相手を足した（`base_proxy`: TLS で受けて上流へ中継、`base_compression`: 毎回 gzip、
`base_proxy_compression`: 中継 + gzip、HTTP/3 は `base`・`base_proxy` に `quic` を足した）。対応は `tools/perf/vs_nginx.py`。
v0.8.0 より前は中継・圧縮・HTTP/3 の相手が無く、veil の中継を nginx の静的配信と比べていた。

| 構成 | プロトコル | nginx | veil | main | 対 nginx | 対 main |
|---|---|---|---|---|---|---|
| `h2_1_ktls_0_lb_kernel_ofc_1`（54KB 静的） | HTTP/1.1 | 6,544（`base`） | 9,670 | 9,777 | **1.48×** | 0.989 |
| 同上 | HTTP/2 | 5,824（`base`） | 9,414 | 9,857 | **1.62×** | 0.955 |
| `h2_1_feat_proxy`（54KB 中継） | HTTP/1.1 | 4,947（`base_proxy`） | 6,523 | 6,418 | **1.32×** | 1.016 |
| 同上 | HTTP/2 | 3,799（`base_proxy`） | 6,809 | 6,860 | **1.79×** | 0.993 |
| `h2_1_feat_compression`（毎回圧縮） | HTTP/1.1 | 2,294（`base_compression`） | 8,409 | 8,331 | **3.67×** | 1.009 |
| 同上 | HTTP/2 | 2,105（`base_compression`） | 4,070 | 4,025 | **1.93×** | 1.011 |
| `h2_1_proxy_compression`（中継 + 圧縮） | HTTP/1.1 | 2,020（`base_proxy_compression`） | 2,670 | 2,596 | **1.32×** | 1.029 |
| 同上 | HTTP/2 | 1,868（`base_proxy_compression`） | 3,357 | 3,403 | **1.80×** | 0.986 |
| `h2_1_feat_compression_cached` | HTTP/2 | 5,824（`base`） | 26,276 | 26,466 | **4.51×** | 0.993 |
| `h3_file_metrics`（54KB 静的） | HTTP/3 | 1,952（`base`） | 2,368 | 2,179 | **1.21×** | **1.087** |
| `h3_proxy`（54KB 中継） | HTTP/3 | 1,775（`base_proxy`） | 2,106 | 1,891 | **1.19×** | **1.114** |
| `h2_0_feat_l4`（L4 平文） | HTTP/1.1 | 6,544（`base`） | 12,967 | 13,128 | **1.98×** | 0.988 |
| `h2c_file`（54KB 静的、12 万要求 × 5） | h2c | 9,300（`h2c_file`） | 30,879 | 30,956 | **3.32×** | 0.998 |
| `h2c_proxy`（54KB 中継、12 万要求 × 5） | h2c | 6,263（`h2c_proxy`） | 11,096 | 10,969 | **1.77×** | 1.012 |

| プロトコル | 構成数 | 対 nginx の中央値 | 最小 | nginx 未満 |
|---|---|---|---|---|
| HTTP/1.1 | 54 | **1.29×** | 1.00 | 0 |
| HTTP/2 | 45 | **1.62×** | 1.04 | 0 |
| HTTP/3 | 19 | **1.20×** | 1.15 | 0 |
| h2c | 2 | **2.55×** | 1.77 | 0 |

- 対 nginx が 1.00 に並ぶのは kTLS を有効にした構成（`*_ktls_1_*`）だけ。コンテナ（veth）では kTLS が不利で、
  nginx 側は kTLS 無しの `base`。
- gRPC（`grpc_h2_*` 約 5,000 req/s、`grpc_h3_*` 約 20,700 req/s）と WebSocket（3,334）は nginx の相手を置いていない。

### main との退行確認（交互 A/B）

| プロトコル | 組数 | 比の中央値 | 最小 | 最大 |
|---|---|---|---|---|
| HTTP/1.1 | 54 | 1.005 | 0.931 | 1.048 |
| HTTP/2 | 45 | 0.994 | 0.919 | 1.052 |
| HTTP/3 | 19 | **1.087** | 1.043 | 1.122 |
| h2c（測り直し） | 2 | 1.005 | 0.998 | 1.012 |
| gRPC | 6 | 0.978 | 0.921 | 1.004 |
| gRPC over HTTP/3 | 6 | 0.992 | 0.933 | 1.088 |
| WebSocket | 1 | 1.006 | — | — |
| **全体** | 133 | **1.004** | — | — |

0.95 を下回った 7 組は、その構成だけ 5 反復で測り直した: `h2_1_feat_cache` 1.002・`h2_1_ktls_0_lb_cbpf_ofc_1` 1.004・
`h2_1_ktls_1_lb_cbpf_ofc_0` 1.026・`h3_proxy_compression` 1.003・`grpc_h2_otel` 0.979・`grpc_h3_rate_limit` 0.993、`h2c_file` は
上の h2c の測り直し。**退行なし。** HTTP/3 の伸びは QUIC の輻輳制御の既定を cubic に（F-240）・経路 MTU の探索で
1452 バイトのペイロード（同）による。

### HTTP/3 の改善（v0.8.0）

| 変更 | 54KB の 1 要求あたりの UDP の送出 | Req/s（Docker のラボ、h2load 4 スレッド） |
|---|---|---|
| v0.7.0 相当（bbr・1350 バイト固定） | 42.2 | 約 1,930 |
| cubic + 1452 バイト + 経路 MTU の探索 | 39.2（nginx 38.9） | 2,280〜2,400（nginx 1,860〜1,960） |

計測のハーネスも直した: h2load を 1 スレッドから 4 スレッドにした（以前は h2load 自体が律速で、veil・nginx とも
4 コア中 1.5〜1.7 コアしか使っていなかった）。

---

## FreeBSD 14.3 aarch64（2026-10-18、QEMU + HVF、ネイティブ）

- コミット: CPU/要求は `1dc3d4aa`、標準・サーバ律速の rps は `021e9fc9`（違いは HTTP/3 の経路 MTU の探索だけ）。
  `102cccea`（HTTP/3 の送出のコピーとタイマーの削減）は 3B で `1dc3d4aa` と同等（CPU/要求 −0.5%）
- 実行: `tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10`（54KB と `-p /small.html` の 3B）、サーバ律速は
  `SRV_CPUS=0 GEN_CPUS=1,2,3 UP_CPUS=1,2,3 SRV_THREADS=1 LOAD_THREADS=3`、CPU/要求は `tools/perf/bsd/cpu_per_req.sh`
- **全計測でエラー 0**
- 計測の見直し（B-153）: nginx を計測対象のコアへ固定し、上流を別プロセスの nginx にし、反復ごとに順を入れ替えた。

### 対 nginx

「標準」は計測対象 2 コア・負荷 2 コア、「サーバ律速」は計測対象 1 コア・負荷 3 コアの rps の比（4 反復の中央値）。
「効率」はサーバの CPU 時間 / 要求の比（nginx / veil。1 より大きいほど veil が少ない CPU で返す）。

| シナリオ | 大きさ | 標準 | サーバ律速 | CPU/要求 veil | nginx | 効率 |
|---|---|---|---|---|---|---|
| `h1_file_tls`（HTTPS 静的） | 3B | 0.98 | 1.07 | 2.6µs | 3.3µs | **1.24** |
| `h2_file_tls`（HTTP/2 静的） | 3B | **1.25** | **1.19** | 2.2µs | 3.3µs | **1.49** |
| `h1_file_plain`（平文 HTTP/1.1 静的） | 3B | 1.05 | **1.12** | 2.8µs | 3.0µs | **1.09** |
| `h2c_file_plain`（h2c 静的） | 3B | **1.94** | **1.34** | 2.2µs | 3.9µs | **1.74** |
| `h3_file`（HTTP/3 静的） | 3B | 1.02 | 1.00 | 5.0µs | 5.2µs | 1.04 |
| `h1_proxy_tls`（HTTPS 中継） | 3B | 0.95 | 1.01 | 7.2µs | 7.5µs | 1.04 |
| `h2_proxy_tls`（HTTP/2 中継） | 3B | **1.90** | **1.40** | 8.6µs | 10.0µs | **1.16** |
| `l4_tcp`（L4 TCP） | 3B | 0.95 | 0.95 | 3.7µs | 3.7µs | 1.01 |
| `h1_file_tls` | 54KB | **1.31** | **1.24** | 41.5µs | 55.7µs | **1.34** |
| `h2_file_tls` | 54KB | **1.49** | **1.52** | 34.2µs | 53.7µs | **1.57** |
| `h1_file_plain` | 54KB | 1.00 | 0.95 | 6.3µs | 6.7µs | 1.06 |
| `h2c_file_plain` | 54KB | 0.88 | 1.02 | 7.9µs | 12.4µs | **1.57** |
| `h3_file` | 54KB | 0.92 | 1.01 | 181.6µs | 153.5µs | 0.85 |
| `h1_proxy_tls` | 54KB | **1.10** | 1.07 | 71.0µs | 80.2µs | **1.13** |
| `h2_proxy_tls` | 54KB | **1.47** | **1.49** | 71.8µs | 105.2µs | **1.46** |
| `l4_tcp` | 54KB | **1.12** | **1.11** | 32.1µs | 37.8µs | **1.18** |

読み方:

- **VM の rps はホストの負荷で大きく揺れる。** ホスト（Apple Silicon の mac）はメモリが逼迫しており（空き 255MB、
  スワップあり）、同じバイナリでもラウンドごとに 3 割下がることがあった。rps が 0.95〜1.05 の行は誤差の範囲で、効率は
  `cpu_per_req.sh` の CPU/要求で見る。
- 平文の 54KB（`h1_file_plain`・`h2c_file_plain`）は、veil・nginx とも 約 85k req/s（約 35Gbps）で頭打ちになる
  （クライアントとループバックが律速）。h2c は共有メモリ + `sendfile(2)`（F-240）でサーバの CPU が nginx の 0.64 倍。
  ホストが空いていた回のサーバ律速では h2c 54KB が 145k req/s 対 nginx 110k（1.33×）だった。
- **残る差: `h3_file` 54KB はスループットは並ぶが、サーバの CPU が 15% 多い。** プロファイルの上位は AES-GCM（両者とも）、
  quiche の内部の `Instant::now()`（1 パケットに 2 回）、データのコピー（quiche の API がストリームのバッファへコピーする）。
  送出のコピーの 1 回と、タイマーのヒープの空回りは v0.8.0 で削った。Linux の HTTP/3 は 1.15〜1.21×。

### FreeBSD で計測して直したもの（v0.8.0）

| 変更 | 計測 |
|---|---|
| `[http3] recv_drain_max` 64 → 1024 | HTTP/3 3B 205k → 280k req/s（nginx 266k） |
| reactor のタイマーを切り捨てない（B-152） | 空の kevent 80 → 0.2 回/要求、TLS 中継 3B の CPU −5% |
| QUIC の輻輳制御 bbr → cubic | 54KB の UDP 破棄 70 万 → 10 万パケット / 6 秒、1 要求 61 → 46 パケット |
| h2c の DATA を共有メモリ + `sendfile(2)` + `TCP_NOPUSH` | h2c 54KB のサーバの CPU/要求 10.2 → 7.9µs（nginx 12.4µs） |
| HTTP/1.1 の中継の応答ヘッダを 1 本のバッファへ | TLS 中継 3B の CPU/要求 −17% |
| L4 の起床の削減（短い read の後は kevent を待つ・アイドル監視 1 本・方向ごとにタスク） | read 4.0 → 3.1/req、CPU −6% |

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

## 再現手順

### Linux（Docker）

```bash
docker build -f docker/Dockerfile.glibc -t veil:glibc .
git worktree add /tmp/base main && (cd /tmp/base && docker build -f docker/Dockerfile.glibc -t veil:base .)
bash tools/perf/gen_configs.sh
BASE=base NEW=glibc bash tools/perf/suite_ab.sh
python3 tools/perf/suite_ab_summary.py tools/perf/results/suite_ab/<日時>.tsv base glibc   # 対 main
python3 tools/perf/vs_nginx.py tools/perf/results/suite_ab/<日時>.tsv veil_glibc           # 対 nginx
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
