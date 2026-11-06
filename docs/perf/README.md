# docs/perf — パフォーマンス計測サマリ（v0.11.0）

Veil の HTTP/1.1・HTTP/2・h2c・HTTP/3・gRPC・WebSocket・L4 のスループットを、nginx と前の版（v0.10.1）を基準に計測した
**最新の結果だけ**をまとめる（過去の計測は git 履歴で辿れる）。

| ファイル | 内容 |
|---|---|
| [`linux_suite_ab_raw.tsv`](linux_suite_ab_raw.tsv) | Linux x86_64 のフルスイート（`suite_ab.sh`、v0.10.1 と交互、nginx の同じ仕事の相手を含む）の生データ |
| [`linux_remeasure_raw.tsv`](linux_remeasure_raw.tsv) | 上で v0.10.1 との比が 0.95 を下回った 15 構成を 5 反復で測り直した生データ |
| [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) | FreeBSD 14.3 aarch64 の `run_perf_bsd.sh`（標準・サーバ律速 × 54KB・3B、nginx と）の生データ |
| [`freebsd_v0101_ab_raw.tsv`](freebsd_v0101_ab_raw.tsv) | FreeBSD の v0.10.1 と v0.11.0 の候補の交互 A/B（rps・CPU/要求）の生データ |
| [`platform_verification.md`](platform_verification.md) | 全プラットフォームの試験（v0.11.0） |

---

## Linux x86_64（2026-11-06、aofish の Docker Desktop、コンテナ間）

- 比較:
  - v0.11.0 の最終候補（`feat/v0110-tickets` の `325e92c3`。v0.11.0 前のセキュリティスキャンの修正 B-313〜B-325 を含む）、
    イメージは `veil:v0110-rc`
  - v0.10.1（タグ `e1ee3f3b`、`veil:v0101-rel`）
  - `nginx:alpine`
- イメージ: どちらも `cargo build --profile dist --features full`（io_uring）を、同じ実行イメージ
  （`cgr.dev/chainguard/glibc-dynamic`）に `/veil` だけ差し替えて入れた
- 実行: `BASE=v0101-rel NEW=v0110-rc NGINX=1 bash tools/perf/suite_ab.sh`（69 構成 × 3 反復、構成ごとに順を入れ替えて交互、約 2.2 時間）
- **全 834 計測で Non-2xx = 0、NA 行ゼロ**

### v0.10.1 との退行確認（交互 A/B）

| プロトコル | 組数 | 比の中央値 | 最小 | 最大 | 0.95 未満 |
|---|---|---|---|---|---|
| HTTP/1.1 | 54 | 0.993 | 0.905 | 1.197 | 4 |
| HTTP/2 | 45 | 0.984 | 0.898 | 1.090 | 5 |
| HTTP/3 | 19 | 0.999 | 0.852 | 1.176 | 4 |
| h2c | 2 | 1.045 | 1.014 | 1.075 | 0 |
| gRPC | 6 | 0.980 | 0.922 | 1.013 | 1 |
| gRPC over HTTP/3 | 6 | 1.002 | 0.942 | 1.079 | 1 |
| WebSocket | 1 | 3.129 | — | — | 0 |
| **全体** | 133 | **0.993** | — | — | 15 |

0.95 を下回った 15 組は、その構成だけ 5 反復で測り直した（`linux_remeasure_raw.tsv`、34 組）。

| プロトコル | 組数 | 比の中央値 | 最小 | 最大 |
|---|---|---|---|---|
| HTTP/1.1 | 13 | 1.014 | 0.994 | 1.052 |
| HTTP/2 | 12 | 0.999 | 0.953 | 1.116 |
| HTTP/3 | 7 | 1.019 | 0.994 | 1.147 |
| gRPC over HTTP/3 | 1 | 1.051 | — | — |
| gRPC | 1 | 0.940 | — | — |
| **全体** | 34 | **1.013** | 0.940 | 1.147 |

- **退行なし。** フルスイートの 0.85〜0.95 は、`suite_ab.sh` が構成ごとに片方の反復をまとめて測るので、このホストの時間で変わる
  揺れ（同じイメージでも ±15%）が片側に乗ったもの。測り直しでは 0.94 の 1 組を除いて 0.95 以上。
- 残った `h2_1_feat_grpc`（gRPC unary を h2c の上流へ中継、k6）の 0.94 は、セキュリティ修正の前の候補でも 0.90〜0.93 だった。
  `h2load -t4 -c50 -m10` で同じ経路に 13 万 req/s を流して `perf stat` で 1 要求あたりの命令数を交互に採ると、v0.10.1 が
  110,655〜111,389、v0.11.0 の候補が 111,025〜112,266（差 1% 未満）、スループットも同等（127,924〜133,342 対 119,010〜132,289）。
  k6 の 2 万 req/s はクライアントと上流（grpcbin）で決まっていて、veil の差ではない。
- WebSocket（`h2_1_feat_websocket`、k6）は v0.10.1 の約 1 万 req/s に対し約 3.2 万 req/s で、セキュリティ修正の前の候補でも同じだった。
  v0.11.0 で WebSocket の中継（F-288 の拡張 CONNECT との共通化、B-309）を変えた後で、どの変更の分かは切り分けていない。
- セキュリティ修正（B-313〜B-325）のうち、毎回の要求で通るもの（HTTP/1.1 の要求の前の空行の数え方と 431 の検査、上流の HTTP/2 の
  資格情報のヘッダの never-indexed、キャッシュの鍵のルートの名前空間）は、測り直しの範囲で差にならなかった。

### nginx との比較（同じ仕事の相手、Req/s 中央値）

対応は `tools/perf/vs_nginx.py` の表のとおり（中継は `base_proxy`、圧縮は `base_compression`、中継 + 圧縮は `base_proxy_compression`、
それ以外は `base`）。

| プロトコル | 構成数 | 対 nginx の中央値 | 最小 | 最大 | nginx 未満 |
|---|---|---|---|---|---|
| HTTP/1.1 | 54 | 0.70 | 0.41 | 2.99 | 43 |
| HTTP/2 | 45 | **1.45** | 0.90 | 4.00 | 15 |
| HTTP/3 | 19 | 0.96 | 0.78 | 1.06 | 17 |
| h2c | 2 | **1.86** | 1.81 | 1.91 | 0 |

代表的な構成:

| 構成 | プロトコル | nginx | veil | 対 nginx |
|---|---|---|---|---|
| `h2_1_ktls_0_lb_cbpf_ofc_1`（静的） | HTTP/2 | 34,520 | 49,884 | **1.45×** |
| `h2_1_feat_proxy`（中継） | HTTP/2 | 23,951 | 39,043 | **1.63×** |
| `h2_1_proxy_compression`（中継 + 圧縮） | HTTP/2 | 6,543 | 24,907 | **3.81×** |
| `h2_1_proxy_compression`（中継 + 圧縮） | HTTP/1.1 | 9,270 | 26,825 | **2.89×** |
| `h2_1_feat_compression`（毎回圧縮） | HTTP/1.1 | 11,985 | 26,747 | **2.23×** |
| `h3_proxy_cache`（中継 + キャッシュ） | HTTP/1.1 | 42,764 | 54,513 | **1.27×** |
| `h2_0_feat_l4`（L4 平文） | HTTP/1.1 | 62,143 | 84,374 | **1.36×** |
| `h2c_file` / `h2c_proxy` | h2c | 70,879 / 38,249 | 135,684 / 69,422 | **1.91× / 1.81×** |
| `h2_1_ktls_0_lb_kernel_ofc_1`（静的） | HTTP/1.1 | 62,143 | 47,574 | 0.77× |
| `h2_0_ktls_0_lb_cbpf_ofc_0`（静的、ファイルキャッシュ無し） | HTTP/1.1 | 62,143 | 25,692 | 0.41× |
| `h3_file_metrics`（静的） | HTTP/3 | 23,051 | 22,730 | 0.99× |
| `h3_proxy`（中継） | HTTP/3 | 21,131 | 19,376 | 0.92× |

nginx を下回る行の読み方（このホストは 16 vCPU で、v0.10.0 までの 4 コアのホストと違い負荷生成の側が先に詰まる）:

- **HTTP/1.1 の静的配信は負荷生成で決まっている。** サーバの CPU 時間 / 要求は、ファイルキャッシュ有り（`*_ofc_1`）で veil 94.6µs
  に対し nginx 113.7µs と veil が少ない。`wrk` のスレッドと接続を増やすと順位が変わる（同じ構成、54KB・TLS）:

  | 負荷 | nginx | veil |
  |---|---|---|
  | `wrk -t4 -c100`（スイートと同じ） | 55,521 | 43,365 |
  | `wrk -t8 -c400` | 85,770 | 68,438 |
  | `wrk -t12 -c400` | 62,517 | **111,135** |

  veil は既定（`threads = 0`＝16 スレッド）で 100 接続を薄く分けるので、少ない負荷ではスレッドの起床が 1 要求あたりに乗る
  （`threads = 4` なら CPU/要求は 44µs）。負荷を見た接続の振り分けは別の設計変更として残す。
- **`*_ofc_0`（ファイルキャッシュ無し）は比較の条件が揃っていない。** nginx の `base` は `open_file_cache` を有効にしている。
  veil の同じ条件は `*_ofc_1`（`[static_file_cache]` + `open_file_cache`）。
- **HTTP/3 は同等。** 静的配信の 19 構成は veil・nginx とも 2.2〜2.4 万 req/s に並び（負荷生成の `h2load` の HTTP/3 の上限）、
  CPU/要求は veil 67〜69µs・nginx 71µs。中継（`h3_proxy*`）の 0.78〜0.94 は、CPU/要求 veil 102µs・nginx 91µs で、
  上流への HTTP/1.1 の中継の分。

---

## FreeBSD 14.3 aarch64（2026-11-06、QEMU + HVF、ネイティブ）

- コミット: `650d6695`（v0.11.0 の候補、セキュリティ修正の前）。その後の変更（B-313〜B-325）は FreeBSD の単体・E2E・
  security-e2e（capsicum）で確かめた（[platform_verification.md](platform_verification.md)）。VM のメモリ 4GB・4 コア。
- 実行: `tools/perf/bsd/run_perf_bsd.sh -r 4 -d 10`（54KB と `-p /small.html` の 3B）。
  - サーバ律速は `SRV_CPUS=0 GEN_CPUS=1,2,3 UP_CPUS=1,2,3 SRV_THREADS=1 LOAD_THREADS=3`。
- **全計測でエラー 0**

### v0.10.1 との退行確認（交互 A/B）

同じ VM で v0.10.1 と v0.11.0 の候補（`--features full-freebsd`）を交互に 4 ラウンド（8 シナリオ × 3B・54KB）。比は v0.11.0 / v0.10.1 の
中央値（rps は大きいほど、CPU/要求は小さいほど良い）。

| シナリオ | 3B rps | 3B CPU/要求 | 54KB rps | 54KB CPU/要求 |
|---|---|---|---|---|
| `h1_file_plain` | 0.985 | 1.002 | 0.999 | 0.986 |
| `h1_file_tls` | 1.009 | 0.996 | 1.018 | 0.993 |
| `h1_proxy_tls` | 0.994 | 1.008 | 0.990 | 1.010 |
| `h2_file_tls` | 0.959 | 1.042 | 0.999 | 1.001 |
| `h2_proxy_tls` | 0.973 | 1.028 | 0.978 | 1.022 |
| `h2c_file_plain` | 0.980 | 1.019 | 0.996 | 1.003 |
| `h3_file` | 1.028 | 0.972 | 1.011 | 0.987 |
| `l4_tcp` | 1.015 | 0.986 | 0.997 | 1.008 |

HTTP/2・h2c の 2〜4% は、HTTP/2・h2c の 3 シナリオ（と比べる `h1_file_tls`）を 8 ラウンドで測り直すと 3B で rps 0.981〜0.997・CPU/要求 1.004〜1.020、
54KB で 0.998〜1.019・0.981〜1.013 だった。VM の揺れの範囲で、**退行なし**と判断した（候補の修正を 1 つ試したが差が出ず、入れていない）。

### 対 nginx

「標準」は計測対象 2 コア・負荷 2 コア、「サーバ律速」は計測対象 1 コア・負荷 3 コアの rps の比（4 反復の中央値）。

| シナリオ | 大きさ | nginx（標準） | veil（標準） | 標準 | サーバ律速 |
|---|---|---|---|---|---|
| `h1_file_tls`（HTTPS 静的） | 54KB | 21,122 | 28,686 | **1.36** | **1.46** |
| `h2_file_tls`（HTTP/2 静的） | 54KB | 20,112 | 27,701 | **1.38** | **1.52** |
| `h1_file_plain`（平文 HTTP/1.1 静的） | 54KB | 47,824 | 48,659 | 1.02 | 1.00 |
| `h2c_file_plain`（h2c 静的） | 54KB | 61,702 | 52,315 | 0.85 | **1.15** |
| `h3_file`（HTTP/3 静的） | 54KB | 3,141 | 3,037 | 0.97 | 0.97 |
| `h1_proxy_tls`（HTTPS 中継） | 54KB | 15,201 | 16,608 | **1.09** | 1.03 |
| `h2_proxy_tls`（HTTP/2 中継） | 54KB | 11,360 | 14,321 | **1.26** | **1.40** |
| `l4_tcp`（L4 TCP） | 54KB | 34,136 | 39,676 | **1.16** | **1.11** |
| `h1_file_tls` | 3B | 215,209 | 203,357 | 0.94 | 1.00 |
| `h2_file_tls` | 3B | 318,374 | 412,608 | **1.30** | 1.03 |
| `h1_file_plain` | 3B | 276,332 | 301,399 | **1.09** | **1.10** |
| `h2c_file_plain` | 3B | 137,118 | 590,464 | **4.31** | **1.17** |
| `h3_file` | 3B | 126,174 | 128,536 | 1.02 | 0.96 |
| `h1_proxy_tls` | 3B | 124,034 | 114,403 | 0.92 | 0.97 |
| `h2_proxy_tls` | 3B | 61,788 | 98,608 | **1.60** | **1.80** |
| `l4_tcp` | 3B | 166,572 | 165,033 | 0.99 | 0.95 |

- VM の rps はホストの負荷で大きく揺れる（同じバイナリでもラウンドごとに 3 割動く）。0.90〜1.05 の行は誤差の範囲。
- `h2c_file_plain` 54KB の標準 0.85 は負荷生成が律速（サーバ律速では 1.15）。
- `h3_file` 54KB は、FreeBSD の quiche（`boringssl-boring-crate`）の経路で、本文をパケットへ詰めるコピーと quiche の内部の確保が
  上位（v0.8.0 から同じ残り）。

---

## 計測条件

### Linux（`tools/perf/`）

- Docker のコンテナ間通信（aofish: 16 vCPU の Docker Desktop / WSL2。veil・nginx・上流・負荷生成で共有）。kTLS は直交表の `ktls`
  因子でだけ有効
- 負荷: HTTP/1.1 は `wrk -t4 -c100 -d10s`、HTTP/2・HTTP/3 は `h2load -t4 -c100 -m10`、gRPC・WebSocket は k6
- 配信: `/`（54,576B の index.html）。上流は nginx（平文 HTTP/1.1）
- 利用者の別の計測（`vp-bench*` のコンテナ）が動いている間は待つ（`suite_ab.sh` の外側のスクリプトで確認）

### FreeBSD（`tools/perf/bsd/`）

- ゲストの中のループバック。計測対象は 0,1 コア（サーバ律速は 0）、負荷生成と上流（別の nginx）は 2,3 コア（同 1,2,3）
- 負荷: HTTP/1.1 は `wrk -t2 -c64`、HTTP/2・h2c は `h2load -t2 -c64 -m32`、HTTP/3 は `h3load -t2 -c64 -m32`（quinn + h3）
- 静的配信は `[static_file_cache]` + `open_file_cache`（nginx も `open_file_cache`）
- 手で A/B するときは、バイナリの名前を `veil` のままにする（`pkill -x veil` で止まらない名前にすると、`SO_REUSEPORT` で
  前のプロセスが同じポートで受け続け、計測が混ざる）

## 再現手順

### Linux（Docker）

```bash
cargo build --profile dist --features full     # 新しい側（ベースは git worktree で v0.10.1 を同じようにビルド）
# どちらも cgr.dev/chainguard/glibc-dynamic に /veil だけを入れたイメージにする（veil:new・veil:base）
bash tools/perf/gen_configs.sh
BASE=base NEW=new bash tools/perf/suite_ab.sh
python3 tools/perf/suite_ab_summary.py tools/perf/results/suite_ab/<日時>.tsv base new   # 対 v0.10.1
python3 tools/perf/vs_nginx.py tools/perf/results/suite_ab/<日時>.tsv veil_new           # 対 nginx
CONFIG_GLOB=<構成> NGINX=0 ITERATIONS=5 BASE=base NEW=new bash tools/perf/suite_ab.sh     # 測り直し
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
```
