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

## Linux x86_64（2026-10-19、Docker コンテナ間）

- コミット: `3736732d`（feat/v080-release。以降はソースの変更なし）。比較対象は main（`60999877`、`veil:main-ab`）と `nginx:alpine`
- イメージ: `veil:glibc`（`full`、io_uring、`--profile dist`）
- 実行: `BASE=main-ab NEW=glibc bash tools/perf/suite_ab.sh`（69 構成 × 3 反復、構成ごとに順を入れ替えて交互）。
  h2c の 2 構成は 12 万要求・5 反復で測り直した（`linux_h2c_raw.tsv`）
- **全 834 計測で Non-2xx = 0、NA 行ゼロ**

### nginx との比較（同じ仕事の相手、Req/s 中央値）

v0.8.0 で nginx に同じ仕事の相手を足した（`base_proxy`: TLS で受けて上流へ中継、`base_compression`: 毎回 gzip、
`base_proxy_compression`: 中継 + gzip、HTTP/3 は `base`・`base_proxy` に `quic` を足した）。対応は `tools/perf/vs_nginx.py`。
v0.8.0 より前は中継・圧縮・HTTP/3 の相手が無く、veil の中継を nginx の静的配信と比べていた。

| 構成 | プロトコル | nginx | veil | main | 対 nginx | 対 main |
|---|---|---|---|---|---|---|
| `h2_1_ktls_0_lb_kernel_ofc_1`（54KB 静的） | HTTP/1.1 | 6,548（`base`） | 9,288 | 9,261 | **1.42×** | 1.003 |
| 同上 | HTTP/2 | 5,548（`base`） | 9,765 | 9,461 | **1.76×** | 1.032 |
| `h2_1_feat_proxy`（54KB 中継） | HTTP/1.1 | 5,053（`base_proxy`） | 6,470 | 6,520 | **1.28×** | 0.992 |
| 同上 | HTTP/2 | 3,530（`base_proxy`） | 6,777 | 6,877 | **1.92×** | 0.985 |
| `h2_1_feat_compression`（毎回圧縮） | HTTP/1.1 | 2,290（`base_compression`） | 8,448 | 8,459 | **3.69×** | 0.999 |
| 同上 | HTTP/2 | 1,999（`base_compression`） | 4,262 | 4,041 | **2.13×** | 1.055 |
| `h2_1_proxy_compression`（中継 + 圧縮） | HTTP/1.1 | 2,038（`base_proxy_compression`） | 2,627 | 2,810 | **1.29×** | 0.935※ |
| 同上 | HTTP/2 | 1,489（`base_proxy_compression`） | 3,539 | 3,459 | **2.38×** | 1.023 |
| `h2_1_feat_compression_cached` | HTTP/2 | 5,548（`base`） | 24,627 | 24,751 | **4.44×** | 0.995 |
| `h3_file_metrics`（54KB 静的） | HTTP/3 | 2,028（`base`） | 2,513 | 2,233 | **1.24×** | **1.125** |
| `h3_proxy`（54KB 中継） | HTTP/3 | 1,881（`base_proxy`） | 2,174 | 1,988 | **1.16×** | **1.094** |
| `h2_0_feat_l4`（L4 平文） | HTTP/1.1 | 6,548（`base`） | 13,140 | 13,162 | **2.01×** | 0.998 |
| `h2c_file`（54KB 静的、12 万要求 × 5） | h2c | 9,277（`h2c_file`） | 30,085 | 30,482 | **3.24×** | 0.987 |
| `h2c_proxy`（54KB 中継、12 万要求 × 5） | h2c | 6,361（`h2c_proxy`） | 10,868 | 10,855 | **1.71×** | 1.001 |

※ 5 反復で測り直して 0.967（下の退行確認）。

| プロトコル | 構成数 | 対 nginx の中央値 | 最小 | nginx 未満 |
|---|---|---|---|---|
| HTTP/1.1 | 54 | **1.29×** | 1.02 | 0 |
| HTTP/2 | 45 | **1.76×** | 1.21 | 0 |
| HTTP/3 | 19 | **1.21×** | 1.14 | 0 |
| h2c | 2 | **2.48×** | 1.71 | 0 |

- 対 nginx が 1.0 付近まで下がるのは kTLS を有効にした構成（`*_ktls_1_*`）だけ。コンテナ（veth）では kTLS が不利で、
  nginx 側は kTLS 無しの `base`。
- gRPC（`grpc_h2_*` 約 5,100 req/s、`grpc_h3_*` 約 21,000 req/s）と WebSocket は nginx の相手を置いていない。

### main との退行確認（交互 A/B）

| プロトコル | 組数 | 比の中央値 | 最小 | 最大 |
|---|---|---|---|---|
| HTTP/1.1 | 54 | 0.999 | 0.915 | 1.024 |
| HTTP/2 | 45 | 1.004 | 0.945 | 1.055 |
| HTTP/3 | 19 | **1.138** | 1.094 | 1.171 |
| h2c（測り直し） | 2 | 0.994 | 0.987 | 1.001 |
| gRPC | 6 | 0.978 | 0.969 | 0.987 |
| gRPC over HTTP/3 | 6 | 0.992 | 0.929 | 1.075 |
| WebSocket | 1 | 1.090 | — | — |
| **全体** | 133 | **1.002** | — | — |

0.95 を下回った 4 組は、その構成だけ 5 反復で測り直した: `grpc_h3_access_log` 1.051・`h2_1_proxy_compression` 0.967（HTTP/1.1）/
1.032（HTTP/2）・`h3_file_wasm` 0.989〜1.123・`h3_proxy_compression` 1.023〜1.109。gRPC（HTTP/2）は 6 構成とも 0.97〜0.99 だったが、
`grpc_h2_metrics` を 6 反復で測り直すと 1.000、Docker の外（同じ k6 → veil → grpcbin、seccomp・Landlock・アクセスログ有効、
release と dist の両方）でも CPU/要求・Req/s とも main と ±1% 以内。**退行なし。**

### HTTP/3 の改善（v0.8.0）

| 変更 | 計測 |
|---|---|
| 輻輳制御の既定を cubic・1452 バイト + 経路 MTU の探索（54KB の 1 要求あたりの UDP の送出 42.2 → 39.2、nginx 38.9） | Docker のラボで 約 1,930 → 2,280〜2,400 req/s（nginx 1,860〜1,960） |
| 応答の本文を quiche へコピーせず `Bytes` のまま渡す・時刻の読み取りを束ごとに 1 回（F-240） | 交互 A/B で h3_proxy +2.7%・h3_file_cache +1.6% |

計測のハーネスも直した: h2load を 1 スレッドから 4 スレッドにした（以前は h2load 自体が律速で、veil・nginx とも
4 コア中 1.5〜1.7 コアしか使っていなかった）。本文のゼロコピーで見つかった quiche の不具合（偽の STREAM_DATA_BLOCKED で
ngtcp2 のクライアントが FINAL_SIZE_ERROR で閉じる）は h2load の計測でしか出ない（E2E の HTTP/3 クライアントは quinn）。

---

## FreeBSD 14.3 aarch64（2026-10-19、QEMU + HVF、ネイティブ）

- コミット: `3736732d`（Linux と同じソース）。VM のメモリ 8GB
- 実行: `tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10`（54KB と `-p /small.html` の 3B）、サーバ律速は
  `SRV_CPUS=0 GEN_CPUS=1,2,3 UP_CPUS=1,2,3 SRV_THREADS=1 LOAD_THREADS=3`、CPU/要求は `tools/perf/bsd/cpu_per_req.sh`
- **全計測でエラー 0**
- 計測の見直し（B-153）: nginx を計測対象のコアへ固定し、上流を別プロセスの nginx にし、反復ごとに順を入れ替えた。

### 対 nginx

「標準」は計測対象 2 コア・負荷 2 コア、「サーバ律速」は計測対象 1 コア・負荷 3 コアの rps の比（4 反復の中央値）。
「効率」はサーバの CPU 時間 / 要求の比（nginx / veil。1 より大きいほど veil が少ない CPU で返す）。

| シナリオ | 大きさ | 標準 | サーバ律速 | CPU/要求 veil | nginx | 効率 |
|---|---|---|---|---|---|---|
| `h1_file_tls`（HTTPS 静的） | 3B | 0.96 | 1.05 | 3.4µs | 4.3µs | **1.27** |
| `h2_file_tls`（HTTP/2 静的） | 3B | **1.38** | **1.30** | 2.2µs | 3.3µs | **1.52** |
| `h1_file_plain`（平文 HTTP/1.1 静的） | 3B | 1.07 | **1.13** | 2.6µs | 2.9µs | **1.12** |
| `h2c_file_plain`（h2c 静的） | 3B | **2.15** | **1.46** | 2.1µs | 3.9µs | **1.79** |
| `h3_file`（HTTP/3 静的） | 3B | 1.06 | 0.92 | 4.7µs | 4.8µs | 1.03 |
| `h1_proxy_tls`（HTTPS 中継） | 3B | 0.99 | 1.03 | 6.8µs | 7.1µs | 1.04 |
| `h2_proxy_tls`（HTTP/2 中継） | 3B | **1.77** | **2.17** | 7.6µs | 9.6µs | **1.26** |
| `l4_tcp`（L4 TCP） | 3B | 0.95 | 0.92 | 3.4µs | 3.4µs | 1.00 |
| `h1_file_tls` | 54KB | **1.32** | **1.31** | 38.3µs | 50.4µs | **1.32** |
| `h2_file_tls` | 54KB | **1.35** | **1.53** | 31.4µs | 49.3µs | **1.57** |
| `h1_file_plain` | 54KB | 0.99 | 0.96 | 5.5µs | 6.2µs | **1.13** |
| `h2c_file_plain` | 54KB | 0.84 | **1.10** | 7.2µs | 11.6µs | **1.60** |
| `h3_file` | 54KB | 0.95 | 0.98 | 142.6µs | 133.3µs | 0.94 |
| `h1_proxy_tls` | 54KB | **1.14** | **1.11** | 62.9µs | 71.2µs | **1.13** |
| `h2_proxy_tls` | 54KB | **1.18** | **1.47** | 60.2µs | 93.4µs | **1.55** |
| `l4_tcp` | 54KB | **1.11** | **1.13** | 28.8µs | 33.7µs | **1.17** |

読み方:

- **VM の rps はホストの負荷で大きく揺れる。** ホスト（Apple Silicon の mac）はメモリが逼迫しており、同じバイナリでも
  ラウンドごとに 3 割下がることがあった。rps が 0.90〜1.05 の行は誤差の範囲で、効率は `cpu_per_req.sh` の CPU/要求で見る
  （効率は 1 行を除いて 1.00 以上）。
- 平文の 54KB（`h1_file_plain`・`h2c_file_plain`）は、veil・nginx ともクライアントとループバックが律速になる。h2c は
  共有メモリ + `sendfile(2)`（F-240）でサーバの CPU が nginx の 0.62 倍。
- **残る差: `h3_file` 54KB はスループットは並ぶ（1.00）が、サーバの CPU が 6% 多い**（v0.8.0 の前は 15〜18%）。本文の
  コピー（quiche の送信バッファへ）と時刻の読み取り（1 パケットに 2 回）を v0.8.0 で削った。残りはプロファイルで
  quiche の送出（`send_single`）と損失回復（`on_ack_received`・`on_packet_sent`）で、nginx の QUIC より 1 パケットあたりの
  仕事が多い。Linux の HTTP/3 は 1.14〜1.24×。
- 配布物（`--profile dist`、fat LTO）と release の CPU/要求の差は 1% 以内（HTTPS 中継 3B で確かめた）。

### FreeBSD で計測して直したもの（v0.8.0）

| 変更 | 計測 |
|---|---|
| `[http3] recv_drain_max` 64 → 1024 | HTTP/3 3B 205k → 280k req/s（nginx 266k） |
| reactor のタイマーを切り捨てない（B-152） | 空の kevent 80 → 0.2 回/要求、TLS 中継 3B の CPU −5% |
| QUIC の輻輳制御 bbr → cubic | 54KB の UDP 破棄 70 万 → 10 万パケット / 6 秒、1 要求 61 → 46 パケット |
| HTTP/3 の本文のゼロコピー・時刻の読み取りを束ごとに 1 回 | h3_file 54KB の CPU/要求 113 → 103µs（同じ回の nginx 98µs）、3B −2〜3% |
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
