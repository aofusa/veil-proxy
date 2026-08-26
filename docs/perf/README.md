# docs/perf — パフォーマンス計測サマリ

Veil の HTTP/1.1・HTTP/2・HTTP/3・gRPC・L4 スループット／レイテンシ／CPU・メモリ使用量を、
`nginx:alpine` を基準に **同一 Docker ネットワーク上のコンテナ間通信** で計測した結果のサマリ。

- 計測ハーネス: [`tools/perf/`](../../tools/perf/)（`gen_configs.sh` で構成生成 /
  `run_perf.sh` で反復計測 / `analyze_results.sh` で median±stdev 集計）。
  実行すると `tools/perf/results/results_raw.tsv`（1 反復 1 行の生データ）と
  `results_summary.md`（集計）が生成される（いずれも git 管理外の作業成果物）。
- **FreeBSD ネイティブ計測の生データは [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv)**
  （2026-08-07〜08）。Docker が使えない FreeBSD 用の別ハーネス
  （`tools/perf/freebsd/`）の出力で、`build` 列（aio / noaio / cache_off / cache_on /
  ktls_on / ktls_off）で構成を、`body` 列（54576 / 3）でレスポンスサイズを区別する。
  詳細な分析は下記「FreeBSD ネイティブ計測」節を参照。
- **本ディレクトリの [`results_raw.tsv`](results_raw.tsv)** は `tools/perf` 生データの
  コミット済みコピーで、計測のたびに `# ==== <日付> ...` の節として**追記**していく。
  **最新のフルスイートは 2026-08-25（`full-container`（epoll reactor）と io_uring 既定ビルドの
  同一コミット比較、全 67 構成 × 2 ビルド × 3 反復 = 756 計測）**で、下記
  「2026-08-25 full-container（epoll reactor）フルスイート」節がその集計。
  その前のフルスイートは 2026-08-24（B-72 マージ後、全 67 構成 × glibc/musl × 3 反復 = 757 計測）。
  `bash tools/perf/analyze_results.sh docs/perf/results_raw.tsv` で下表を再集計できる。
  行順は **nginx ベースライン → veil_glibc 各構成 → veil_musl 各構成**（F-118 で明文化）。
  `h3_proxy_buffering` の行のみ、B-46 修正後の同日 scoped 再計測で置換している（下記）。

## 計測条件（最新計測）

- ホスト: 4 コア Linux（co-tenant あり）
- イメージ: `veil:glibc` / `veil:musl`（full features、`--build-arg CARGO_FEATURES='full'`、
  **B-44/B-45/B-46 修正込み**）、比較対象 `nginx:alpine`（`access_log off`、
  http1.1/http2 の base 構成のみ = F-118 の方針）
- 負荷: HTTP/1.1 = wrk `-t4 -c100 -d10s` / HTTP/2・HTTP/3 = h2load `-n 30000 -c100 -m10`
  （HTTP/3 は QUIC 対応 h2load、ALPN=h3）/ gRPC = k6 50VU×10s → grpcbin(h2c) 中継 /
  L4 = wrk（平文 9080 素通し）
- 各 (config, proto) を warmup 後 3 反復、median±stdev 集計。Errors は Non-2xx
- gRPC over HTTP/3 はクライアント（k6）非対応のためフェイルセーフで NA（仕様どおり）
- kTLS はコンテナ（veth）と相性が悪いため feat 系構成では無効（直交表の ktls 因子でのみ計測）

## 2026-08-26 フルスイート（F-163〜F-167 / B-74 / B-75 適用後、全 67 構成 × 2 ビルド × 3 反復）

`BUILDS='glibc container' ITERATIONS=3 bash tools/perf/run_perf.sh`。
**672 計測すべてで Non-2xx = 0、NA 行はゼロ**（`grpc_h3*` も F-167 で実計測できるようになった）。
生データは [`results_raw.tsv`](results_raw.tsv) の `# ==== 2026-08-26 ...` 節。

### 代表構成（Req/s 中央値、2026-08-25 の同一ハーネス計測との比較）

| 構成 | プロトコル | nginx | 08-25 `full` | **今回 `full`** | 08-25 `full-container` | **今回 `full-container`** |
|---|---|---|---|---|---|---|
| `h2c_file`（3B 静的・h2c） | h2c | 10,329 | 24,546 | **24,647** | 24,903 | **25,787** |
| `h2c_proxy`（54KB 中継・h2c） | h2c | 6,344 | 10,407 | **11,120（+6.8%）** | 10,565 | **11,402（+7.9%）** |
| `h2_1_ktls_0_lb_kernel_ofc_1` | HTTP/1.1 | 6,758 | 9,271 | **9,433** | 9,316 | 9,322 |
| 同上 | HTTP/2 | 6,205 | 7,367 | 7,349 | 7,450 | 7,376 |
| `h2_1_feat_proxy` | HTTP/1.1 | 6,758※ | 6,386 | **6,526** | 6,653 | **6,662** |
| `h3_file_metrics` | HTTP/3 | — | 1,851 | **1,878** | 1,959 | 1,912 |
| `h3_proxy` | HTTP/3 | — | 1,582 | **1,613** | 1,655 | — |
| `grpc_h2_metrics` | gRPC(k6) | — | 4,438 | **4,451** | 4,565 | 4,533 |
| **`grpc_h3_metrics`** | **gRPC over HTTP/3** | — | **NA（計測不能）** | **8,018** | **NA** | **8,679** |
| `h2_1_feat_websocket` | WebSocket | — | 3,129 | **3,268** | 4,861 | 4,791 |
| `h2_0_feat_l4` | L4（平文 9080） | 6,758 | 13,778 | 13,709 | 12,992 | 12,823 |

※ nginx ベースラインは TLS **静的配信**であり逆プロキシではない（F-118 の方針）。
`feat_proxy` 行の対 nginx は同条件比較ではない。プロキシ同士の同条件比較は `h2c_proxy` のみ。

### 読み方

- **`h2c_proxy`（54KB 逆プロキシ）が両バックエンドで +6.8〜7.9%。** F-165 R4
  （バックエンド応答ボディを `Bytes::copy_from_slice` からプール済み `BytesMut` の
  `split_to().freeze()` へ）の効果で、ラボの交互 A/B（CPU/req -17%）と整合する。
  対 nginx は **1.75×（`full-container`）／1.75×（`full`）**。
- **gRPC over HTTP/3 が初めて数字になった（F-167）。** 全 6 構成で 7,300〜8,800 rps、
  Non-2xx = 0。**k6 の gRPC（HTTP/2、4,400〜4,600 rps）と直接比較してはならない**
  （k6 は VU ベース、h2load は `-c/-m` 多重化ベースで負荷モデルが違う）。
  比較して意味があるのは同じ h2load 条件での構成間・ビルド間の相対値。
- **この計測が B-74（HTTP/3 → h2c 上流にコネクションプールが無く `EADDRNOTAVAIL` で
  5.9% が 5xx、726 rps）と B-75（epoll の readiness ヒントによる kTLS + HTTP/2 の
  恒久ハング）の 2 件を発見した。** どちらも単体 934・統合 54・E2E 544 をすべて通過しており、
  **フルスイートだけが検出できた**（B-74 修正後は同経路が 726 → 8,000 rps 超）。
- `full` と `full-container` は 08-25 と同じく**概ね互角**で、WebSocket（container 有利）と
  L4（uring 有利）の傾向も維持されている。

### 計測条件

- ホスト: 4 コア Linux（co-tenant あり）。**クライアント・veil・上流が同一マシンを共有する**ため、
  veil 単体の改善は rps に鈍く出る（負荷時の CPU は client 71% / veil 195% / backend 130% ＝ 約 400%）。
  改善幅を正確に見たい場合は `tools/perf/h2c_proxy_lab.sh cpuab`（veil の CPU/req 交互 A/B）を使う。
- 負荷: HTTP/1.1 = wrk `-t4 -c100 -d10s` / HTTP/2・HTTP/3 = h2load `-n 30000 -c100 -m10` /
  gRPC = k6 50VU×10s / **gRPC over HTTP/3 = QUIC 対応 h2load + gRPC unary ワイヤ形式（F-167）** /
  WebSocket = k6 / L4 = wrk（平文 9080）
- ハングでスイートが停止しないよう `CLIENT_TIMEOUT`（既定 180 秒）を導入済み（B-75 の教訓）。

## 2026-08-26 F-163〜F-166（tls_only / UDS / アロケーション最小化 / epoll 最適化）

`tools/perf/h2c_proxy_lab.sh` の交互 A/B と、新設した **`cpuab`（veil の CPU/req 交互 A/B）**、
**`tools/perf/alloc_measure.sh`（1 リクエストあたりのヒープ確保回数の実測、`alloc-stats` feature）**
による計測。ベースラインは同一ハーネス・同一日ビルドの `veil:ab-base-*`。

### なぜ rps ではなく CPU/req を主指標にしたか

ラボはクライアント（h2load）・veil・上流 nginx が**同じ 4 コアを共有**し、負荷時の CPU は
**client 71% / veil 195% / backend 130% ＝ 約 400%（＝ 4 コア飽和）**だった。
この状態では veil 単体を X% 速くしても rps は X×（veil のシェア）程度しか伸びず、
数 % の改善はノイズに埋もれる。**veil の CPU/req なら共有飽和の影響を受けずに検出できる。**

### syscall / リクエスト（`h2c_proxy`、epoll reactor、strace -c -f、20,000 リクエスト）

| 経路 | 変更前 | 変更後 | `epoll_ctl` |
|---|---|---|---|
| 54KB | 5.966 | **4.718（-20.9%）** | 1.298 → **0.127** |
| 3B | 5.472 | **3.825（-30.1%）** | 1.250 → **0.015** |

F-166 A（`EPOLLET` 常時登録 + readiness ヒント）の効果。残りは
「バックエンドへの write 1 + read 1」＋コアレッシング済みのクライアント I/O で、
**このワークロード（HTTP/1.1 上流）の理論下限に近い**。

### アロケーション / リクエスト（`alloc-stats`）

| 構成 | 変更前 | HPACK ゼロコピー後 | 削減 |
|---|---|---|---|
| `h2c_file` 3B（静的） | 34.3 | **20.3** | **-41%** |
| `h2c_proxy` 3B | 54.8 | **41.7** | **-24%** |
| `h2c_proxy` 54KB | 50.7 | **37.8** | **-25%** |

### CPU/req と rps（交互 A/B、6 ラウンド、`cpuab`）

| 段階 | 構成 | rps | CPU/req |
|---|---|---|---|
| F-166 A + F-165 R1-R3 | epoll 3B | 22,075 → 22,375（+1.4%） | 22.2 → **21.5 µs（-3.2%）** |
| 同上 | epoll 54KB | 10,599 → 10,618 | 70.7 → 71.0 µs（ノイズ） |
| 同上 | io_uring 54KB | 10,457 → 10,457 | 80.9 → 80.5 µs |
| **F-165 R4（ボディのゼロコピー切り出し）** | epoll 54KB | 10,423 → **11,169（+7.2%）** | 71.7 → **59.0 µs（-17.7%、6/6 勝）** |
| **同上** | io_uring 54KB | 10,540 → **11,025（+4.6%）** | 79.3 → **65.8 µs（-17.1%、6/6 勝）** |

**R4 では allocs/req は 37.8 → 38.8、bytes/req は 61KB → 71KB と「増えた」のに CPU は 17% 下がった。**
効いていたのは確保回数ではなく **54KB の memcpy そのもの**である（F-157 の「コピー削減は
自明に速くならない」の裏返しで、**キャッシュに載らないサイズのコピーは実際に高い**）。

### gRPC（k6 → veil → grpcbin、`grpc_h2_metrics`、3 反復の中央値）

| ビルド | 変更前 | 変更後 |
|---|---|---|
| `full-container`（epoll） | 4,506 rps / 10.61 ms | **4,631 rps（+2.8%）/ 10.34 ms** |
| `full`（io_uring） | 4,431 rps / 10.82 ms | **4,476 rps（+1.0%）/ 10.69 ms** |

## 2026-08-25 full-container（epoll reactor）フルスイート（全 67 構成 × 2 ビルド × 3 反復）

Linux の **`full-container` feature セット**（`full` + `epoll` = readiness reactor 本番経路。
Docker / Kubernetes / gVisor など io_uring が seccomp や runtime のエミュレーションで
制限されがちな環境向け、F-144）のスループットを、**同一コミット・同日ビルドの io_uring 既定
ビルドと直接比較できる形**で取得した。

- コミット `5ddb413`（F-159 / F-160 / F-139 / F-161 / F-130 C2 / F-162 適用後）
- `veil:glibc` = `--build-arg CARGO_FEATURES=full`（`veil_rt_uring`）、
  `veil:container` = `--build-arg CARGO_FEATURES=full-container`（`veil_rt_reactor`）。
  どちらも `docker/Dockerfile.glibc`・同一コミット・同日ビルド。
  reactor で動いていることは起動ログ（`enable_io_uring_restrictions ... this build uses the
  reactor (epoll) runtime backend` の警告）で確認済み。
- 実行: `BUILDS='glibc container' ITERATIONS=3 bash tools/perf/run_perf.sh`
- **756 計測すべてで Non-2xx = 0**。`NA` は `grpc_h3*` の 12 行のみ（k6 が gRPC over HTTP/3 に
  非対応のフェイルセーフ、仕様どおり）。生データは [`results_raw.tsv`](results_raw.tsv) の
  `# ==== 2026-08-25 full-container ...` 節。

### 代表構成（Req/s 中央値）

| 構成 | プロトコル | nginx | io_uring (`full`) | full-container (`epoll`) | container / uring |
|---|---|---|---|---|---|
| `h2_1_ktls_0_lb_kernel_ofc_1` | HTTP/1.1 | 6,663 | 9,271 | 9,316 | **1.005** |
| `h2_1_ktls_0_lb_kernel_ofc_1` | HTTP/2 | 6,134 | 7,367 | 7,450 | **1.011** |
| `h2c_file`（3B 静的・平文 h2c） | h2c | 10,651 | 24,546 | 24,903 | **1.015** |
| `h2c_proxy`（54KB 中継） | h2c | 6,417 | 10,407 | 10,565 | **1.015** |
| `h2_1_feat_proxy` | HTTP/1.1 | 6,663 | 6,386 | 6,653 | **1.042** |
| `h3_file_metrics` | HTTP/3 | — | 1,851 | 1,959 | **1.058** |
| `h3_proxy` | HTTP/3 | — | 1,582 | 1,655 | **1.047** |
| `grpc_h2_metrics` | gRPC | — | 4,438 | 4,565 | **1.029** |
| `h2_1_feat_websocket` | WebSocket | — | 3,129 | 4,861 | **1.554** |
| `h2_0_feat_l4` | L4（平文 9080） | 6,663 | 13,778 | 12,992 | **0.943** |
| `h2_1_proxy_compression` | HTTP/1.1 | 6,663 | 1,881 | 1,607 | **0.854** |

### プロトコル別の比（container / io_uring、中央値）

| プロトコル | ペア数 | 比の中央値 | 最小 | 最大 |
|---|---|---|---|---|
| HTTP/1.1 | 52 | 1.009 | 0.854 | 1.055 |
| HTTP/2 | 43 | 0.997 | 0.871 | 1.026 |
| HTTP/3 | 18 | **1.033** | 1.018 | 1.066 |
| h2c | 2 | 1.015 | 1.015 | 1.015 |
| gRPC | 6 | 1.024 | 1.007 | 1.029 |
| WebSocket | 1 | **1.554** | — | — |
| **全 122 ペア** | 122 | **1.009** | 0.854 | 1.554 |

### 読み方

- **全体としては互角**（全 122 ペアの比の中央値 1.009）。`full-container` は
  「io_uring が使えない環境向けの代替」であって性能を諦める選択ではない、という
  F-144 の前提が 4 コア Linux 上の実測でも成り立っている。
- **HTTP/3 は reactor のほうが一貫して速い（+1.8〜6.6%、18 ペア全部で reactor 勝ち）**。
  io_uring 側の HTTP/3 受信は `mmsg_batch_size` 本の `IORING_OP_RECVMSG` を常時 in-flight に
  保つパイプライン（F-130 C1）で、1 データグラムごとに SQE を再投入する。一方 reactor は
  `recv_drain_max`（既定 64）まで `recvmmsg` で一気に drain するため、**この負荷（-c100 -m10 の
  QUIC）では「1 回の syscall で何通拾えるか」で reactor が勝っている**。真の multishot + buffer ring
  （F-130 C2）はまさにここを埋める施策だが、検証機のカーネルが `IORING_REGISTER_PBUF_RING` を
  拒否するため実測できていない（AGENTS.md / F-130 参照）。
- **WebSocket は reactor が +55%**。長寿命コネクション上の小さなフレームを往復させる
  ワークロードで、io_uring 側は 1 フレームごとに SQE 提出 + CQE 回収の固定費を払うのに対し、
  reactor は readiness ヒント（`poll` の結果）で読み書きを直接発行できる。
  **1 リクエストあたりのバイト数が小さく往復回数が多いほど reactor 有利**という傾向は、
  HTTP/3・gRPC の結果とも整合する。
- **逆に reactor が明確に負けるのは「圧縮を伴うプロキシ」（-13〜15%）と
  「kTLS + CBPF の HTTP/2」（-12%）、L4（-5.7%）**。前者は CPU バウンド（gzip/brotli）で
  ワーカーが計算に張り付く構成、後者は kTLS の送信オフロードと `splice(2)` が絡む経路で、
  いずれも io_uring の完了通知モデルが有利に働く。
- **`veil:musl` は本計測では測っていない**（比較したい軸が libc ではなくランタイムバックエンド
  であるため、`BUILDS='glibc container'` に絞った）。musl との比較は 2026-08-24 節を参照。

## 2026-08-24 フルスイート（B-72 マージ後、全 67 構成 × glibc/musl × 3 反復）

B-72（io_uring タイマーのユーザ空間ヒープ化）と中間 64KB `Vec` 除去を main へ入れたあと、
**h2c 以外も含めた Linux 全構成**を計測し直した。**757 計測すべてで Non-2xx = 0**。
`NA` は `grpc_h3*` の 6 構成のみ（k6 が gRPC over HTTP/3 に非対応のフェイルセーフ、仕様どおり）。
`veil:glibc` / `veil:musl` は**同一コミットから同日ビルド**している。

### 代表構成（Req/s 中央値）

| 構成 | プロトコル | nginx | veil_glibc | veil_musl | 対 nginx |
|---|---|---|---|---|---|
| `h2c_file`（h2c 静的） | h2c | 10,905 | **23,849** | 23,359 | **2.19×** |
| `h2c_proxy`（h2c 逆プロキシ） | h2c | 6,409 | **10,039** | 9,943 | **1.57×** |
| `h2_1_ktls_0_lb_kernel_ofc_1`（TLS 静的・最良構成） | HTTP/1.1 | 6,511 | **9,180** | 8,981 | **1.41×** |
| 同上 | HTTP/2 | 6,129 | **7,249** | 7,302 | **1.18×** |
| `h2_0_feat_l4`（L4 平文素通し） | HTTP/1.1 | 6,511 | **13,726** | 13,736 | **2.11×** |
| `h2_1_feat_proxy`（TLS 逆プロキシ） | HTTP/1.1 | 6,511※ | 6,337 | 6,326 | 0.97×※ |
| 同上 | HTTP/2 | 6,129※ | 5,882 | 5,862 | 0.96×※ |
| `h2_1_feat_buffering` | HTTP/2 | 6,129※ | 5,938 | 5,916 | 0.97×※ |
| `h2_1_feat_http3`（HTTP/3 静的） | HTTP/3 | — | 1,849 | — | — |
| `h3_proxy`（HTTP/3 逆プロキシ） | HTTP/3 | — | 1,598 | 1,592 | — |
| `h2_1_feat_grpc` | gRPC(k6) | — | 4,308 | 4,289 | — |
| `h2_1_feat_websocket` | WebSocket(k6) | — | 3,109 | 3,292 | — |

> ※ **この行の「対 nginx」を額面どおり読んではならない。** `run_perf.sh` の nginx ベースライン
> （`base` 構成）は **TLS 静的配信**であり、逆プロキシではない（F-118 の方針。`nginx.conf` の
> 443 サーバは `root /var/www` の静的配信）。したがって `feat_proxy` / `feat_buffering` の比は
> **「veil の逆プロキシ」対「nginx の静的配信」**であって同条件比較ではなく、
> 実際には「veil はプロキシしながら nginx の静的配信とほぼ同速」と読むのが正しい。
> **プロキシ同士の同条件比較になっているのは h2c だけ**（nginx 側も `/proxy/` で中継する）で、
> そこでは **1.57×**。

### B-72 の効果（h2c_proxy）

同一ハーネスで測った `h2c_proxy` 1.57× は、改修時の交互 A/B（ラボハーネス）で得た
**対 nginx ~1.60×** と一致しており、**標準ハーネス側からも独立に裏付けられた**。
改修前は 1.19× だった（下記 B-72 節参照）。

### 機能別オーバーヘッド（HTTP/2・glibc・基準 = `h2_1_ktls_0_lb_kernel_ofc_0` の 7,085）

| 機能 | Req/s | 基準比 |
|---|---|---|
| metrics | 7,029 | 99.2% |
| http3（併設） | 6,996 | 98.7% |
| opentelemetry | 6,982 | 98.6% |
| admin | 6,965 | 98.3% |
| rate-limit | 6,954 | 98.2% |
| wasm（パススルー 1 枚） | 6,911 | 97.5% |
| access-log | 6,860 | 96.8% |
| cache | 6,826 | 96.3% |
| **buffering** | 5,938 | **83.8%** |
| **proxy（バックエンドホップ）** | 5,882 | **83.0%** |
| **compression** | 2,011 | **28.4%** |

- **観測系（metrics / otel / admin / rate-limit / access-log / wasm / cache）は 96〜99%**
  で、ほぼノイズ範囲のオーバーヘッドに収まっている。
- **proxy / buffering の −17% はバックエンドホップそのもの**のコストで、機能実装の問題ではない。
- **compression の −72% は 54,576B を毎リクエスト実圧縮している**ためで、
  CPU バウンドな処理として妥当（キャッシュ無しの最悪ケース）。

### glibc と musl

代表構成のいずれでも **両者の差は数 % 以内**でノイズ範囲。アロケータ・libc の違いが
スループットを左右する状況にはなっていない。

生データは [`results_raw.tsv`](results_raw.tsv) 末尾の
`# ==== 2026-08-24 B-72 マージ後 Linux フルスイート再計測 ...` 節。

---

## v0.6.0 io_uring 非劣化確認（2026-07-20、median ± stdev）

v0.6.0 のマルチプラットフォーム対応（macOS/Windows/FreeBSD kTLS・AIO 等）は**すべて
`cfg`/feature で分離**され、既定の Linux io_uring データプレーンのホットパス
（`src/runtime/uring/`・`proxy.rs` の splice 送出。F-120 のリネームは論理不変・E2E 検証済み、
kTLS splice の cfg は Linux で `all(veil_ktls, target_os="linux")` = 従来の `veil_ktls` と
同一評価）には**ロジック差分がありません**。同一ホストで v0.6.0 を最適静的配信構成
（`h2_1_ktls_0_lb_kernel_ofc_1` = 全 feature ビルド・kTLS off・kernel LB・open_file_cache on）で
再計測し、v0.5.0 公表値と同水準であることを確認した（全 Non-2xx = 0）:

| Target | Proto | v0.6.0 Req/s (median±stdev) | v0.5.0 参考 | 比 |
|---|---|---|---|---|
| veil_glibc | HTTP/1.1 | **3189.0 ± 19.7** | 3213 | 99.3%（誤差内） |
| veil_glibc | HTTP/2 | **2792.3 ± 47.3** | 2763 | 101.1% |
| veil_musl | HTTP/1.1 | **3139.6 ± 13.0** | — | — |
| veil_musl | HTTP/2 | **2770.5 ± 41.7** | — | — |
| nginx（同ホスト同時計測） | HTTP/1.1 | 2263.2 ± 44.7 | — | veil 比 1.41 倍 |
| nginx | HTTP/2 | 2383.2 ± 29.8 | — | veil 比 1.17 倍 |

> 補足: `open_file_cache` off 構成（`ofc_0`）では毎リクエストのファイル open が加わり
> HTTP/1.1 2700 / HTTP/2 2478 になる（構成差であり退行ではない）。絶対値はホスト状態
> （4 コア・co-tenant・稼働 610 日）に依存するため、**同一ホストで同時計測した nginx 比**と
> **v0.5.0 との相対**で退行有無を判断している。全プラットフォーム×arch（aarch64 io_uring は
> `tools/qemu` full-system QEMU、macOS/Windows）の網羅計測は別途。

**再確認（2026-07-20、現行 main = F-123 capability mode 静的配信 + http3 無効ビルド修正 +
`cap_safe_sleep` 反映後）**: 同一構成・同一ホストで再計測し退行が無いことを確認した
（`h2_1_ktls_0_lb_kernel_ofc_1`、3 反復、全 Non-2xx = 0）。本セッションの変更はすべて
`cfg`/FreeBSD 限定で Linux io_uring データプレーンのホットパスに影響しないため、値は上表と
統計的に同一:

| Target | Proto | 再計測 Req/s (median±stdev) | 上表比 | nginx 同時計測比 |
|---|---|---|---|---|
| veil_glibc | HTTP/1.1 | 3180.6 ± 41.5 | 99.7%（誤差内） | 1.45×（nginx 2188.1 ± 173.5） |
| veil_glibc | HTTP/2 | 2848.8 ± 60.1 | 102.0% | 1.19×（nginx 2393.1 ± 30.6） |
| veil_musl | HTTP/1.1 | 3131.9 ± 38.6 | 99.8% | 1.43× |
| veil_musl | HTTP/2 | 2740.6 ± 67.0 | 98.9% | 1.15× |

## F-150/F-151 後の Linux 退行確認（2026-08-11、median ± stdev）

F-150（rustls 送信の `writev(2)` 直結）・F-151（HTTP/3 メインループのイベント駆動化）は
どちらも **Linux の既定データプレーンを通る**（F-150 は kTLS 無効時の rustls 経路全般、
F-151 は HTTP/3）ため、既定の最適静的配信構成で退行が無いことを確認した
（`h2_1_ktls_0_lb_kernel_ofc_1`、3 反復、`veil:glibc` / `veil:musl` を full features で再ビルド、
全 Non-2xx = 0）。

| Target | Proto | 本計測 Req/s | v0.6.0 参考 | 同時計測 nginx 比 | v0.6.0 の nginx 比 |
|---|---|---|---|---|---|
| veil_glibc | HTTP/1.1 | **3018.5 ± 6.0** | 3189.0 / 3180.6 | **1.46×** | 1.41 / 1.45 |
| veil_glibc | HTTP/2 | **2693.8 ± 43.4** | 2792.3 / 2848.8 | **1.19×** | 1.17 / 1.19 |
| veil_musl | HTTP/1.1 | **2989.8 ± 4.7** | 3139.6 / 3131.9 | **1.44×** | 1.43 |
| veil_musl | HTTP/2 | **2707.7 ± 70.8** | 2770.5 / 2740.6 | **1.20×** | 1.15 |
| nginx（同時計測） | HTTP/1.1 | 2069.1 ± 4.4 | 2263.2 / 2188.1 | — | — |
| nginx（同時計測） | HTTP/2 | 2265.2 ± 49.6 | 2383.2 / 2393.1 | — | — |

**退行なし**。絶対値は全ターゲットで過去計測比 5〜6% 低いが、**同時計測の nginx も
同じだけ低い**（HTTP/1.1 2069 対 2188〜2263、HTTP/2 2265 対 2383〜2393）ため
ホスト状態の差であり、本節冒頭の方針どおり **nginx 比で判定**する。
nginx 比は 4 ケースすべてで過去計測と同等かわずかに良い。

生データは [`results_raw.tsv`](results_raw.tsv) の末尾（`# ==== 2026-08-11 ...` 節）。

## F-150/F-151 後の FreeBSD 計測（2026-08-11、FreeBSD 14.3 amd64 / QEMU+KVM）

**注意: 過去の FreeBSD 計測（2026-08-07〜08）は aarch64 / QEMU+HVF（Apple Silicon）で
行っており、本計測は amd64 / QEMU+KVM である。ホストもゲストアーキも異なるため
絶対値の直接比較はできない。** 有効なのは**同一実行内で併走させた nginx との比**である
（本ドキュメントの計測方針どおり）。

| シナリオ（54KB） | veil | nginx | **veil/nginx** | 参考: 改修前の同比（aarch64） |
|---|---|---|---|---|
| HTTP/1.1 TLS | 2546 | 2632 | **0.97** | 0.44 |
| HTTP/2 TLS | 2926 | 2775 | **1.05** | 0.52 |
| h2c 平文 | 4692 | 7272 | 0.65 | 0.52 |
| HTTP/1.1 proxy | 1996 | 1965 | **1.02** | 0.72 |
| HTTP/2 proxy | 2430 | 1709 | **1.42** | 0.94 |
| L4 TCP | 5514 | 5349 | **1.03** | 0.61 |
| **HTTP/3** | **595** | **477** | **1.25** | 0.50 |

- **HTTP/1.1 TLS・HTTP/2 TLS・HTTP/3 のいずれも nginx と同等以上**になった
  （改修前はそれぞれ 0.44 / 0.52 / 0.50）。F-150（TLS 送信のコピー排除）と
  F-151（HTTP/3 のループ固定費排除）が効く領域と一致する。
- h2c 平文（TLS なし）だけは 0.65 と差が残る。ここは TLS 送信経路を通らないため
  F-150 の対象外であり、次の調査対象。
- アーキ・ハイパーバイザが異なるため「何倍速くなった」とは言えない。言えるのは
  **同一環境で nginx に対する相対位置が改善した**ことである。

生データは [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) の末尾。

### 計測ハーネスのバグを 2 件修正した（どちらも「0 rps」を静かに記録していた）

FreeBSD の HTTP/3 計測は**この 2 件により長らく壊れていた**（`h3_file` が veil・nginx とも
0 rps。値が 0 でもハーネスはエラー終了しないため気付きにくい）。

1. **`h3load` が h2load 互換の密着形短オプションを受理していなかった**。
   ハーネスは `-t2 -c64` の形で渡すが `h3load` は `-t 2` しか受理せず、
   「不明なオプション: -t2」で即終了していた。`h2load` は両形式を受理するため
   `h3load` 側を両対応にした（`split_short_opt` + 単体テスト 3 件）。
2. **固定リクエスト数（`-n`）を外側の `timeout` で包んでいた**。
   `-n $((CONNECTIONS * 2000))`（既定 64 接続なら 128,000 リクエスト）は
   遅い環境では `timeout` までに捌き切れず、h3load が集計行を出す前に殺されていた。
   wrk と同じく **`-d`（時間）で区切る**方式に変更した。

**教訓: 計測ハーネスは「0 を記録して正常終了する」失敗モードを持つ。
新しい環境で計測する際は、まず 0 や NA が出ていないかを確認すること。**

## v0.6.0 リリース前の最終計測（2026-08-16）

リリース確定前に **Linux（Docker、退行確認 + 機能構成スイート）** と
**FreeBSD（ネイティブ、aarch64）** を再計測した。対象コードは F-153（`canonicalize` 排除）・
F-154（ルート単位 dirfd 封じ込め）・B-65（offload 往復の統合）を含む現行 main。
全 205 行（Linux 163 + FreeBSD 42）で **Non-2xx / errors = 0**。

### Linux 退行確認（`h2_1_ktls_0_lb_kernel_ofc_1`、3 反復、median ± stdev）

| Target | Proto | 本計測 Req/s | 2026-08-11 | 同時計測 nginx 比 | 08-11 の nginx 比 |
|---|---|---|---|---|---|
| veil_glibc | HTTP/1.1 | **2969.4 ± 10.0** | 3018.5 | **1.79×** | 1.46× |
| veil_glibc | HTTP/2 | **2636.1 ± 176.6** | 2693.8 | **1.40×** | 1.19× |
| veil_musl | HTTP/1.1 | **2968.1 ± 26.6** | 2989.8 | **1.79×** | 1.44× |
| veil_musl | HTTP/2 | **2709.9 ± 63.4** | 2707.7 | **1.44×** | 1.20× |
| nginx（同時計測） | HTTP/1.1 | 1658.2 ± 43.0 | 2069.1 | — | — |
| nginx（同時計測） | HTTP/2 | 1884.4 ± 63.6 | 2265.2 | — | — |

**退行なし。** veil の絶対値は 4 ケースすべて前回比 98〜100%（誤差内）で、
**同時計測の nginx だけが 20% 低い**（ホスト状態の差）。本ドキュメントの方針どおり
nginx 比で判定すると 4 ケースすべてで改善している（F-153/F-154 が
静的配信のリクエスト単価を下げた効果と整合する）。

### Linux 機能構成スイート（`h2_*_feat_*`、glibc/musl × 3 反復、代表値）

| Target | Config | Proto | Req/s | 参考: v0.5.0（2026-07-16） |
|---|---|---|---|---|
| veil_glibc | h2_0_feat_l4 | http1.1 | 3545.0 ± 6.7 | 5074.3 |
| veil_musl | h2_0_feat_l4 | http1.1 | 4511.6 ± 13.6 | 5080.8 |
| veil_glibc | h2_1_feat_http3 | http3 | 560.2 ± 10.3 | 835.4 |
| veil_musl | h2_1_feat_http3 | http3 | 644.3 ± 7.8 | 824.9 |
| veil_glibc | h2_1_feat_grpc | grpc | 1127.8 ± 236.6 | 1609.4 |
| veil_glibc | h2_1_feat_wasm | http2 | 2547.5 ± 225.0 | — |
| veil_glibc | h2_1_feat_proxy | http2 | 1849.5 ± 13.1 | 1933.3 |
| veil_glibc | h2_1_feat_websocket | websocket | 977.5 ± 45.3 | — |

**絶対値を v0.5.0 と直接比べてはならない**（本計測日のホストは同時計測の nginx が
2182.7 → 1647.4 = **75%** に落ちており、ホスト全体が約 25% 遅い）。ホスト差で正規化すると
L4・gRPC・HTTP/3 とも v0.5.0 と同水準（例: HTTP/3 は 835 × 0.75 ≒ 626 に対し musl 644 /
glibc 560）で、**退行を示す材料は無い**。機能別オーバーヘッドの傾向（compression のみ
CPU バウンドで大きい、他はノイズ範囲）も従来と一致する。

生データは [`results_raw.tsv`](results_raw.tsv) 末尾の
`# ==== 2026-08-16 ...` 2 節。

## F-155: FreeBSD の対 nginx パリティ（2026-08-17、FreeBSD 14.3 aarch64 / QEMU+HVF）

### 結論: 54KB の劣後は**コードではなく計測ハーネスの設定**が原因だった

`tools/perf/freebsd/run_perf_freebsd.sh` は veil の設定に `ktls_enabled = true` を
ハードコードしていた一方、比較対象の nginx は kTLS を使っていなかった
（`ssl_conf_command` 未指定）。FreeBSD の software kTLS が TLS レコードごとに
カーネルワーカースレッドへ暗号処理をディスパッチして直列化することは
2026-08-08 に本ドキュメントへ記録済みだったが、**ハーネスがその知見に追従していなかった**。

`ktls_enabled = false` に揃えた同一 VM・同一セッションの実測（3 反復の中央値、54,576B）:

| シナリオ | nginx | veil | **veil/nginx** | 2026-08-16（kTLS 有効） |
|---|---|---|---|---|
| HTTP/1.1 TLS | 23,890 | 26,024 | **1.09** | 0.51 |
| HTTP/2 TLS | 21,566 | 24,922 | **1.16** | 0.56 |
| h2c 平文 | 66,724 | 40,381 | 0.61 | 0.58 |
| HTTP/3 | 3,426 | 3,428 | 1.00 | 0.98 |
| HTTP/1.1 proxy | 16,233 | 16,681 | **1.03** | 0.78 |
| HTTP/2 proxy | 12,702 | 16,191 | **1.27** | 0.98 |
| L4 TCP | 36,209 | 22,770 | 0.63 | 0.58 |

**TLS を使う 4 シナリオすべてで veil が nginx を上回った**（HTTP/1.1・HTTP/2 の
ファイル配信とプロキシ）。残る劣後は h2c 平文（0.61）と L4 TCP（0.63）。

### p99 3810ms のスパイクは再現しない（H7 は誤診）

`docs/artifacts/freebsd_perf_bottleneck_analysis.md` は `h1_proxy_tls` の
p99 = 3810ms を「`TcpStream::connect_str` の `to_socket_addrs()` が同期
`getaddrinfo` を呼ぶため」と結論していたが、**これは誤りである**:

1. Rust 標準の `impl ToSocketAddrs for str` は**まず `SocketAddr` としてのパースを試し、
   成功したら resolver を呼ばない**。上流は `127.0.0.1:18080` の IP リテラルなので
   `getaddrinfo` は元から実行されていない。
2. 再計測すると p99 は 54KB で 5.66〜5.82ms（nginx 5.87〜6.05ms）、
   3B で 1.24〜1.30ms。スパイクは一度も観測されなかった。

F-155 の Phase 1（`ProxyTarget::socket_addr` の事前解決）は
「接続ごとの `Vec` 確保を消す」というホットパス規則上の改善として入れてあるが、
**p99 の改善を主張してはならない**。

### 3B（リクエスト単価）は依然として劣後する

| シナリオ | nginx | veil | veil/nginx |
|---|---|---|---|
| HTTP/1.1 TLS | 200,390 | 93,068 | 0.46 |
| HTTP/2 TLS | 211,104 | 177,370 | 0.84 |
| h2c 平文 | 374,569 | 168,032 | 0.45 |
| HTTP/1.1 proxy | 107,430 | 98,824 | 0.92 |
| HTTP/2 proxy | 76,948 | 82,829 | **1.08** |
| L4 TCP | 170,818 | 153,575 | 0.90 |

F-155 の Phase 2/3/4（capsicum の malloc 排除・kqueue `write_hint`・バッチ accept）は
この領域を狙ったものだが、**この VM のラウンド間変動（同一バイナリで 1.8 倍）に
対して効果が埋もれ、単独の寄与を確定できなかった**。

### `h1_file_plain` シナリオの追加

Phase 5（`sf_hdtr` によるヘッダ+本体の 1-syscall 化）が効くのは
**平文 HTTP/1.1 の静的配信だけ**（rustls 経路は平文漏洩になるので通さない、
HTTP/2・h2c は DATA フレーミングが要るので `sendfile` に載らない）。
既存シナリオにその構成が無かったため `h1_file_plain` を追加した。

**注意**: veil の平文リスナー（`h2c_listen`）は **h2c 専用**で HTTP/1.1 を
「Plain HTTP/1.1 not supported on H2C-only server」として切断する。
平文 HTTP/1.1 を veil に喋らせる唯一の方法は、**TLS ポートへ `http://` を投げて
プロトコル検出経由で `accept_plain` させる**こと。これを知らずに h2c ポートへ
投げると veil だけ 0 rps になる（一度踏んだ）。

実測（3 反復連続、54,576B）:

| iter | veil rps | veil p99 | nginx rps | veil/nginx |
|---|---|---|---|---|
| 1 | 87,749 | 0.86ms | 94,799 | 0.93 |
| 2 | 70,804 | 1.01ms | 76,889 | 0.92 |
| 3 | 52,638 | 1.36ms | 58,390 | 0.90 |

**veil と nginx が同率で落ちており対 nginx 比は 0.90〜0.93 でほぼ一定**（絶対値が
下がるのは VM 側の要因）。`sf_hdtr` 経路の正しさは実機で別途確認済み
（200 応答・`Content-Length` 一致・3B 本体一致・54,576B 本体の md5 が配信元と一致）。

### Phase 6（`TCP_NOPUSH`）の効果は確認できていない

`nopush_guard` の有無を変えた A/B を試みたが**有効な計測が取れていない**（下記の
計測不備に該当したため）。Phase 5 でヘッダと本体が既に 1 回の `sendfile(2)` に
まとまっているため `TCP_NOPUSH` に合流させる余地はほとんど無く、代わりに
1 リクエストあたり `setsockopt` が 2 回増える。**改めて A/B し、効果が確認できなければ外すこと。**

### 計測の落とし穴: `pkill -x veil` は別名バイナリを殺さない（F-156 で判明・修正済み）

ハーネスは計測ごとに `pkill -x veil` で veil を落としていたが、`-x` は
**プロセス名の完全一致**である。A/B のために `VEIL_BIN` へ `veil.baseline` /
`veil.f156` のような**別名のバイナリ**を渡すと、プロセス名が `veil.baseline` 等になり
**1 つも kill されない**。さらに veil のリスナーは `SO_REUSEPORT`(_LB) で bind するため、
**取り残された旧プロセスが同じポートを掴んだまま生き残り、カーネルが新旧プロセスへ
接続を分散する**。結果として

1. ラウンドを重ねるほど veil プロセスが積み上がり、同じ 2 コアを奪い合う
   （＝「ラウンドごとに単調劣化する」ように見える）
2. 「新バイナリ」を測っているつもりで**旧バイナリとの混合**を測る

という二重の汚染が起きる。実際に **13 プロセスの残存**を確認し、これに基づいて
起票しかけた「veil 固有の劣化バグ」（B-66）は撤回した。

**対策（実装済み）**: `stop_veil()` で `VEIL_BIN` をフルパス指定の `pkill -f` と
`pkill -x veil` の 2 段構えにし、`start_veil()` は起動前に `pgrep veil` で残存を数えて
**1 つでも残っていたら計測を中止する**（黙って混ざった値を出さない）。

**A/B で別バイナリを使うときは、`/root/ab/<変種>/veil` のように
basename を `veil` のままディレクトリで分けること。**

生データは `docs/artifacts/freebsd_perf/`（git 管理外の作業成果物）。

## F-156: L4 マルチワーカー化（2026-08-17、FreeBSD 14.3 aarch64）

### L4 TCP は 1 コアしか使っていなかった → 対 nginx 0.58 から 1.09 へ

`spawn_l4_listeners` はリスナー定義 1 個につき `thread::spawn` を 1 回しか呼んでおらず、
設定の `threads` に関わらず **L4 TCP は常に 1 スレッド（1 コア）**で動いていた
（比較対象の nginx は `worker_processes 2` + `listen ... reuseport` で 2 コア）。
`create_listener` 経由で `SO_REUSEPORT_LB` リスナーを `threads` 個複製した結果:

| round | F-155（1 コア） | F-156（2 コア） |
|---|---|---|
| 1 | 0.563 | **1.052** |
| 2 | 0.519 | **0.902** |
| 3 | 0.645 | **1.183** |
| 4 | 0.599 | **1.135** |
| 中央値 | 0.58 | **1.09** |

（54,576B、交互 A/B・ラウンドごとに実行順も入替。別セッションでも 0.54 → 0.96 / 1.13 で再現）

**1 コアあたりで見ると元から veil のほうが速かった**（veil 22.7k rps/コア vs
nginx 18.1k rps/コア）ので、劣後の原因はコード効率ではなく使用コア数だった。

### 計測の落とし穴 2: この VM のノイズは「数ラウンド方向が揃う」程度では超えられない

`h2c_file_plain` で **同じ 2 バイナリの交互 A/B を 2 回**取ったところ:

- 1 回目: 「`accept_batch` 有り」が **4 ラウンドすべてで劣る**（0.428 → 0.394）
- 2 回目: 「有り」が **勝つ**（0.427 → 0.477）
- 8 サンプルずつの平均: **0.434 対 0.431（ほぼ同一）**

1 回目だけを見て回帰と判断し実装を撤回しかけたが、**差は最初から無かった**。
**4 ラウンド一致は、この計測系では有意ではない。**

さらに根本的な問題として、**その計測系が対象の機構を動かしているかを先に確認すること**:
`h2c_file_plain` は h2load が keep-alive で 64 接続を張りっぱなしにするため、
`accept` は計測開始時の 64 回しか実行されない。`accept_batch` の効果を
このシナリオで測ろうとすること自体が誤りだった（測るには接続確立レートの負荷が要る）。



### FreeBSD 14.3 aarch64（QEMU/HVF・Apple Silicon、54,576B、capsicum + capability mode 有効）

**同一 VM・同一ボディサイズの 2026-08-07 計測との直接比較**（アーキ・ハイパーバイザが
同じなので比較可能。ホストは `ssh llm` の macOS）:

| シナリオ | veil | nginx | **veil/nginx** | 2026-08-07 の同比 |
|---|---|---|---|---|
| HTTP/1.1 TLS | 21,441 | 42,439 | **0.51** | 0.44 |
| HTTP/2 TLS | 23,002 | 41,269 | **0.56** | 0.52 |
| h2c 平文 | 60,688 | 103,959 | **0.58** | 0.52 |
| HTTP/1.1 proxy | 22,516 | 29,017 | **0.78** | 0.72 |
| HTTP/2 proxy | 20,589 | 20,946 | **0.98** | 0.94 |
| L4 TCP | 37,478 | 64,293 | 0.58 | 0.61 |
| HTTP/3 | 5,941 | 6,072 | **0.98**（下限、下記参照） | 0.50 |

- **L4 以外の全シナリオで対 nginx 比が改善**した（HTTP/3 は 0.50 → 0.98）。
  F-153/F-154/B-65 が静的配信の固定費を下げた効果と整合する。
- **aarch64 の対 nginx 比（0.5 前後）は amd64（2026-08-11・16 の 0.77〜1.05）より低い。**
  ホストもゲストアーキもハイパーバイザも異なるため**絶対値も比も arch をまたいで
  比較してはならない**（本ドキュメントの既定方針）。判定は必ず**同一環境の時系列**で行う。
- **HTTP/3 の 0.98 は「veil ≧ nginx」の下限値**であり、パリティの証明ではない。
  接続数を 64 → 16 に減らしても両サーバとも rps がほぼ変わらず（veil 6,473 / nginx 5,918）
  レイテンシだけが 1/4 になった（p50 178ms → 21ms）。**並行度に依存しないスループット上限
  ＝ 2 コアに固定した `h3load`（quinn ベースのクライアント）側の律速**である。
  FreeBSD の HTTP/3 の真の上限を測るにはクライアント側の増強が必要。

生データは [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) 末尾の
`# ==== 2026-08-16 ...` 節。

> **計測時の落とし穴（今回踏んだもの）**: `tools/qemu/bsd-vm.sh` の同期対象は
> `src`/`tests`/`third_party` 等**のみで `tools/` を含まない**。VM 内の
> `tools/perf` は前回セッションで手動コピーしたものが残っており、**2026-08-11 に
> 修正したはずの h3load 短オプションのバグが VM 内では未修正のまま**で、
> 再び `h3_file` が veil・nginx とも 0 rps になった。
> **VM で perf を回す前に `tools/perf` を明示的に同期すること**（tar over ssh）。

## 最新結果（2026-07-16、v0.5.0、median ± stdev）

全 105 (target, config, proto) × 3 反復（739 行）で **Non-2xx = 0**（NA は grpc_h3 の
フェイルセーフのみ）。代表値:

| Target | Config | Proto | Req/s | Lat Avg | CPU% | Mem MB | Errors |
|---|---|---|---|---|---|---|---|
| nginx | base | http1.1 | 2182.7 ± 81.6 | 44.43ms | 227.5 | 22.4 | 0 |
| nginx | base | http2 | 2334.6 ± 19.5 | 218.65ms | 177.7 | 27.6 | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | **3212.9 ± 24.8** | 30.12ms | 218.8 | 93.4 | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http2 | **2762.8 ± 26.2** | 177.20ms | 129.4 | 207.6 | 0 |
| veil_glibc | h2_1_feat_http3 | http3 | **835.4 ± 12.2** | 1.14ms | 162.1 | 276.5 | 0 |
| veil_glibc | h2_1_feat_grpc | grpc | **1609.4 ± 5.9** | 29.42ms | 134.4 | 87.6 | 0 |
| veil_glibc | h2_0_feat_l4 | http1.1 | **5074.3 ± 56.1** | 19.05ms | 124.6 | 94.1 | 0 |
| veil_glibc | h2_1_feat_proxy | http2 | 1933.3 ± 39.9 | 287.04ms | 128.4 | 289.9 | 0 |
| veil_glibc | h3_proxy | http3 | 653.0 ± 1.4 | 1.46ms | 170.8 | 469.8 | 0 |
| veil_musl | h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | 3126.4 ± 22.0 | 30.96ms | 219.7 | 94.9 | 0 |
| veil_musl | h2_1_feat_http3 | http3 | 824.9 ± 3.2 | 1.15ms | 175.9 | 337.2 | 0 |
| veil_musl | h2_1_feat_grpc | grpc | 1575.4 ± 17.7 | 30.00ms | 138.4 | 97.8 | 0 |
| veil_musl | h2_0_feat_l4 | http1.1 | 5080.8 ± 25.0 | 19.01ms | 140.9 | 102.0 | 0 |

（全プロトコル × 全機能マトリクスの全行は [`results_raw.tsv`](results_raw.tsv) を参照。
`h2_1_proxy_*` / `h3_file_*` / `h3_proxy_*` / `grpc_h2_*` の各機能構成も同 tsv に含まれる）

**要点:**

- **HTTP/1.1: nginx 比 1.47 倍、HTTP/2: 1.18 倍**（h2load 既定 1 スレッドはクライアント
  律速気味の点に注意。F-116 の A/B では `-t4` で HTTP/2 3646 req/s = HTTP/1.1 超えを確認済み）。
- **F-121（HPACK Huffman 4-bit LUT）**: デコード単体の release マイクロベンチで旧線形探索比
  **約 11.9 倍**（代表ヘッダ文字列 + 全 256 バイト符号、200 ラウンド）。e2e `tools/perf`
  の HTTP/2 エンドツーエンドはホスト co-tenant 負荷で絶対 req/s が変動するため、
  同一実行内の相対指標で確認（例: 2026-07-17 scoped `h2_1_ktls_0_lb_kernel_ofc_1`、
  h2load `-t4`: veil_glibc http2 **1858 req/s** / http1.1 1603 / nginx http2 1148 →
  **HTTP/2 が HTTP/1.1 超え・nginx 比 1.62 倍**。Errors=0。生ログは
  `tools/perf/results/f121_h2_*`、設計は `docs/artifacts/hpack_huffman_lut_design.md`）。
- **L4 平文素通し 5074 req/s** = TLS 経由 HTTP/1.1 の 1.6 倍・nginx 比 2.3 倍。
  B-45 修正（半クローズ伝搬）により反復劣化（旧: 3 回目に 0 req/s）が解消し安定。
- **HTTP/3: 835 req/s（File）/ 653 req/s（Proxy）**。ボトルネックはユーザ空間 QUIC の
  per-request CPU コスト（F-115 で 2 倍化済み）。
- **gRPC 中継: 1609 req/s / 29.4ms**（前回 1475 から +9%。B-44 のプール上限拡大が
  H2C プールの再利用にも寄与）。プロキシホップのオーバーヘッドは実質ゼロ（F-106 対照計測）。
- **HTTP/2 プロキシ系（`*proxy*` の http2）は ~1930 req/s・Non-2xx=0 に回復**。
  F-116 多重化直後は接続チャーン/fd 枯渇で ~590 req/s + 502 混入だった（下記 B-44）。
- L7 機能（wasm/metrics/access-log/rate-limit/admin/otel/cache）のオーバーヘッドは
  引き続きノイズ範囲内（±5%）。compression のみ CPU バウンドで大きい（仕様どおり）。

## 参考値: HTTP/3 を `--net=host` + GSO/GRO 有効で計測（2026-07-17）

コンテナブリッジ（veth）を外した場合の HTTP/3 上限の参考として、`h2_1_feat_http3` 相当の
構成（listen 8443）を `docker run --net=host` + `[http3] gso_gro_enabled = true` で起動し、
QUIC 対応 h2load（同じく `--net=host`、127.0.0.1 宛）で計測した（3 反復、Non-2xx=0）:

| ネットワーク | gso_gro_enabled | Req/s (median ± stdev) |
|---|---|---|
| bridge（上表） | false | 835.4 ± 12.2 |
| **host** | **true** | **905.9 ± 3.0** |
| host | false | 907.7 ± 3.4 |

- **host ネットワーク化で +8〜9%**（veth/bridge のオーバーヘッド分）。
- **GSO/GRO はループバック計測では中立**（有効/無効の差は誤差内）。GSO/GRO は
  実 NIC でのセグメンテーションオフロードを前提とした機能であり、この参考値は
  「コンテナ経由でない場合の上限目安」として読むこと（examples/config.toml の注記どおり
  Docker/仮想環境では効果が出ない場合がある）。
- 生ログ: `docs/artifacts/perf_reports/f118/hostnet_*.log`（git 管理外）

## v0.5.0 計測で検出・修正したバグ（B-44 / B-45 / B-46）

本フルスイート（F-118）の初回実行で 3 件の潜在バグを検出し、修正後に再計測した。

| ID | 事象 | 真因 | 修正 |
|---|---|---|---|
| [B-44](../backlog/bugs/B-44-h2-proxy-backend-conn-churn-port-exhaustion.md) | HTTP/2 プロキシが ~590 req/s（−73%）+ 502 混入 | F-116 多重化で同時 fd 需要 ~1100 が **起動時に引き上げていなかった RLIMIT_NOFILE soft 1024** を超過（EMFILE）。プール上限 8 も接続チャーンを増幅 | 起動時 rlimit 自動引き上げ（nginx `worker_rlimit_nofile` 相当）+ プール上限 8→256 + connect 並行数ゲート + EADDRNOTAVAIL リトライ |
| [B-45](../backlog/bugs/B-45-l4-half-close-fd-exhaustion.md) | L4 が反復ごとに劣化し 3 回目に 0 req/s | 片方向 EOF 時に `shutdown(SHUT_WR)` を対向へ伝搬せず、クローズ済み接続が 4 fd をアイドルタイムアウトまで滞留 → EMFILE | 転送ループ離脱時の半クローズ伝搬 + FIN 即時伝搬の回帰テスト |
| [B-46](../backlog/bugs/B-46-http3-buffered-proxy-body-stall.md) | HTTP/3 + buffering full の Proxy が 2xx ヘッダのみ・ボディ 0B・全ストリームエラー | `send_response` の無条件 content-length 付与がバックエンド由来の content-length と**重複**し、nghttp3 が H3_MESSAGE_ERROR (0x10E) で拒否 | 重複時は付与しない + ボディ内容一致を検証する E2E 追加。修正後 http3 601.6 ± 18.8 req/s（同日 scoped 再計測、tsv へ反映済み） |

- 修正の実測効果: HTTP/2 プロキシ 590 → **1933 req/s**（3.3 倍）+ Non-2xx 0 化、
  L4 反復安定 **~5080 req/s**、h3_proxy_buffering http3 0 → **602 req/s**。
- 教訓: **h2load の `failed`（ストリームエラー）は Non-2xx に計上されない**（B-43 に続き
  B-46 でも同様）。Errors=0 でも 0 req/s 近傍の行は h2load の `requests:` 行を必ず確認する。

## HTTP/2 多重化の A/B（2026-07-15、F-116）

`docs/artifacts/h2_performance_analysis.md` の調査（HTTP/2 フレームループがリクエスト成立
ごとにバックエンド往復を `await` する直列処理 = アプリ層 Head-of-Line Blocking）を受け、
HTTP/3 と同型のアクターモデル（per-stream タスク + 有界チャネル + Notify + `POLL_ADD`
readiness 待ち）へ移行した F-116 の同日・同一環境 A/B（`h2_1_ktls_0_lb_kernel_ofc_1`、
main / feat/h2-multiplexing を各イメージ再ビルドの上で連続計測、ITERATIONS=3）。

**クライアント律速を解消した負荷（`-n 60000 -c100 -m10 -t4`）:**

| Target | Proto | main | F-116 | 変化 |
|---|---|---|---|---|
| veil_glibc | http2 | 3140.6 ± 77.8 | **3646.2 ± 27.8** | **+16.1%** |
| veil_musl | http2 | 3145.1 ± 50.9 | 3446.5 ± 77.9 | +9.6% |
| veil_glibc | http1.1 | 3217.2 ± 4.0 | 3214.5 ± 9.6 | ±0（非劣化） |
| nginx | http2 | 2501.9 ± 44.0 | 2481.1 ± 127.6 | （環境正規化用） |

- **HTTP/2 が HTTP/1.1 を初めて上回った**（3646 vs 3214、+13%）。nginx http2 比 **1.47 倍**。
- 標準負荷（h2load 既定 1 スレッド）では +6.5%（クライアント律速。教訓の節を参照）。
- 多重化 E2E（`test_http2_multiplexing_slow_stream_does_not_block_fast`）で機能面も担保。
- **F-117 追補**: HTTP/2 File 配信のパス解決を open_file_cache へ統一（同期 `is_dir` 排除）。
  スループット非劣化・HTTP/2 CPU 低下（168→159%）。
- **プロキシ経路の同時ストリーム増**（1 コネクション直列 → -m10 で同時 1000）が
  B-44 を顕在化させた（上記）。

## 計測履歴（時系列サマリ）

1. **初期ベンチマーク**: kTLS 無効時に nginx 超えを最初に確認。glibc ≧ musl、
   mimalloc 有効が高速という傾向。
2. **B-13/B-14/B-15**（seccomp `faccessat2` 未許可・cache 無効時のファイル解決スタブ）:
   修正後の再計測で全 24 計測 Non-2xx=0。
3. **kTLS はコンテナで不利**（+36〜70% 無効時が高速）、`reuseport_balancing=kernel` は
   単一クライアント IP 負荷で有利。
4. **F-73/F-74 HTTP/2 送信最適化**: per-frame の二重確保 + 二重コピー排除で HTTP/2 +11.6%。
5. **完全直交表（2⁴=16）+ full features ショーケース**を整備。nginx 比最大 1.6 倍。
6. **F-89 機能単位オーバーヘッド計測**: TLS 終端が支配的コスト、L7 機能はノイズ範囲内。
7. **F-106 H2C プーリング / F-111 GSO 送信ゼロコピー**: gRPC 中継の接続再利用と HTTP/3
   送信経路の確保排除。
8. **F-114 全プロトコル×全機能マトリクス**: 65+ 構成、`CONFIG_GLOB` で scoped 計測。
9. **F-115 第2段 + B-43（2026-07-13）**: HTTP/3 recvmmsg/sendmmsg バッチング +
   StreamBlocked 修正で 421 → 853 req/s へ倍増。
10. **F-116 HTTP/2 ストリーム多重化（2026-07-15）**: アクターモデル化で HTTP/2 +16.1%
    （3646 req/s、HTTP/1.1 超え・nginx 比 1.47 倍）。F-117 で open_file_cache 統一。
11. **F-118 v0.5.0 フルスイート（2026-07-16、最新）**: L4 readiness 確認・TSV 行順明文化の
    ハーネス改善後に全 105 構成×プロトコルを計測。計測起点で **B-44（RLIMIT_NOFILE /
    接続チャーン）・B-45（L4 半クローズ未伝搬）・B-46（H3 content-length 重複）を検出・
    修正**し、修正後の再計測で全行 Non-2xx=0。`--net=host` + GSO/GRO の HTTP/3 参考値も追加。

12. **FreeBSD ネイティブ計測（2026-08-07、F-145 / B-63）**: DTrace で 1 リクエスト約 20 syscall を実測し、`aio`（POSIX AIO）が 1 I/O あたり 3 syscall で readiness 経路より遅く、かつ HTTP/2 の小レスポンス高並行でサーバを完全停止させることを発見（`full-freebsd*` の既定から除外）。小レスポンスで HTTP/1.1 TLS +77.6%・L4 TCP 約 5.5 倍。HTTP/3 は自作 `tools/perf/h3load`（本体ワークスペース外の独立クレート）で初めて計測可能になった。
13. **F-146 静的コンテンツキャッシュ（2026-08-08）**: HTTP/2・HTTP/3 の静的配信がリクエストごとにファイル全体を offload スレッドプール経由で読み直していた問題を解消（`[static_file_cache]`、既定オフ）。小レスポンスの HTTP/2 で **+32%**（168.7k → 223.5k rps）、54KB の大レスポンスは帯域律速のため変化なし。レビューで DashMap の自己デッドロック（キャッシュ有効時に必ず踏む）を発見・修正。
14. **54KB 応答のボトルネック特定（2026-08-08）**: CPU 律速ではなく**コンテキストスイッチ律速**（約 400,000 回/秒 = 1 リクエスト約 17 回、CPU は 51% idle）であり、接続数を 64→512 に増やしてもスループットが動かない直列化点があることを突き止めた。原因は FreeBSD の software kTLS が TLS レコードごとにカーネルスレッドへ暗号処理をディスパッチすること。kTLS 無効で中央値 +26%（最良ラウンドは 2.24 GB/s で nginx の 2.55 GB/s に肉薄）。

## FreeBSD ネイティブ計測（2026-08-07〜08、B-63 / F-145 / F-146）

`tools/perf/` 本体は Docker 前提のため FreeBSD では動かない。専用ハーネス
[`tools/perf/freebsd/run_perf_freebsd.sh`](../../tools/perf/freebsd/run_perf_freebsd.sh) を
追加し、**ゲスト内 loopback で veil と nginx を同条件**（同じ 2 コアへ cpuset 固定・
負荷生成は別 2 コア・アクセスログ双方オフ・proxy/L4 の上流は共通 nginx）で計測した。

**生データ: [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv)**（本節・
「F-146 静的コンテンツキャッシュ」節・「FreeBSD の kTLS は大きな応答で不利」節の
全計測を含む）。

- 環境: FreeBSD 14.3-RELEASE aarch64（QEMU/HVF on Apple Silicon）、4 vCPU / 4GB、
  サーバ 2 コア・負荷生成 2 コア、loopback
- 比較対象: nginx 1.29（FreeBSD pkg、`--with-http_v2_module` / `--with-http_v3_module` /
  `--with-stream`）
- 負荷: HTTP/1.1 = wrk / HTTP/2・h2c = h2load / **HTTP/3 = 自作 `tools/perf/h3load`**
  （FreeBSD の nghttp2 pkg の h2load は ngtcp2 非同梱、curl pkg も HTTP/3 非対応のため）
- 配信ファイルは 54KB（バイト単価）と 3B（リクエスト単価）の 2 種

### 改善サマリ（本セッションの累積、veil / FreeBSD aarch64）

| シナリオ | 改善前 | 改善後 | 倍率 | 効いた変更 |
|---|---|---|---|---|
| HTTP/1.1 TLS・3B | 77,504 | 105,344 | **1.36x** | B-63（aio 除外） |
| HTTP/2 TLS・3B | 計測不能（停止） | 223,500 | **—** | B-63 + F-146 |
| h2c 平文・3B | 1,927（エラー多発） | 119,860 | **62x** | B-63 |
| L4 TCP・3B | 13,491 | 202,108 | **15x** | B-63 |
| HTTP/2 TLS・54KB | 21,446 | 27,860（kTLS 無効時） | **1.30x** | B-63 + kTLS 無効化 |
| HTTP/3・54KB | 計測手段なし | 5,869 | **—** | h3load 追加 |

改善前 = `full-freebsd`（当時の既定、`aio` 有効）。改善後 = `aio` 除外 +
`[static_file_cache]` 有効 + `ktls_enabled = false`。
小さなレスポンスでの劇的な改善（62x / 15x）は、`aio` 有効時にサーバが
停止・大量エラーを起こしていた（B-63）状態からの回復を含む。

### 環境の理論値（実測プリミティブから算出）

| 項目 | 実測 |
|---|---|
| AES-128-GCM（OpenSSL、16KB ブロック、1 コア） | **7.80 GB/s** |
| nginx 平文 h2c 54KB（2 コア） | 5.82 GB/s（106,700 rps） |
| nginx TLS HTTP/1.1 54KB（2 コア） | 2.55 GB/s（46,800 rps） |
| nginx TLS HTTP/1.1 3B（2 コア） | 384,000 rps |

暗号処理は 2 コアで 15.6 GB/s 相当あり**ボトルネックにならない**。小さな応答では
nginx が約 38〜42 万 rps に達しており、これが「1 リクエスト = read + write の 2 syscall」
から見積もられる本環境の実務上の上限とほぼ一致する。

### B-63: `aio`（F-127 の POSIX AIO 経路）が大幅な性能低下と停止を起こす

DTrace（54KB 静的ファイル / HTTP/1.1 TLS / 約 174k リクエスト）で **1 リクエストあたり
約 20 syscall**（nginx は 5〜6）を消費していることが判明した。内訳の一部:

```
openat 1, close 1, sendfile 1, write 2, __realpathat 2, fstatat 3,
poll 1.7, kevent 0.74, _umtx_op 3.4,
aio_read 1, aio_write 1, aio_error 2, aio_return 2   ← AIO だけで 6 syscall
```

POSIX AIO は 1 I/O あたり submit + `aio_error` + `aio_return` の **3 syscall** を要し、
readiness 経路（`read`/`write` 1 発）より遅い。実測:

| 計測（サーバ 2 コア固定） | `aio` あり | `aio` なし |
|---|---|---|
| HTTP/1.1 TLS・3 バイト | 105,520 rps | **187,374 rps（+77.6%）** |
| HTTP/1.1 TLS・54KB | 21,540 rps | 21,291 rps（有意差なし） |
| L4 TCP・3 バイト | 約 37,000 rps | **約 202,000 rps（約 5.5 倍）** |
| HTTP/2 TLS・3 バイト（`-c32 -m16`） | **サーバが完全停止** | 148,679 rps・エラー 0 |

大きな転送でも改善が無く、かつ HTTP/2 で停止するため、`full-freebsd` /
`full-freebsd-aarch64` の既定から `aio` を除外した（B-63）。**これが本計測で得られた
最大の改善**である。

### 対 nginx 比（`aio` 除外後、同一反復内の比で評価）

| シナリオ | 54KB | 3B |
|---|---|---|
| HTTP/1.1 TLS | 0.44 | 0.38 |
| HTTP/2 TLS | 0.52 | 0.32 |
| h2c 平文 | 0.52 | 0.30 |
| HTTP/1.1 proxy | 0.72 | 0.68 |
| HTTP/2 proxy | 0.94 | **1.77（veil が上回る）** |
| L4 TCP | 0.61 | 0.83 |
| HTTP/3 | 0.50（veil 5,869 / nginx 11,665 rps） | — |

**HTTP/3 は syscall 律速ではない**: FreeBSD には `sendmmsg`/`recvmmsg`・UDP GSO が無く
54KB あたり約 41 データグラムを個別に送るため上限は約 37,000 rps と見積もられるが、
実測は veil・nginx とも**その 1/3〜1/6** に留まる。したがって FreeBSD の HTTP/3 の
支配要因はデータグラム syscall ではなく **QUIC の暗号処理と輻輳制御**である
（当初の想定を計測が否定した）。

### F-146: 静的コンテンツキャッシュ（HTTP/2・HTTP/3）

HTTP/2・HTTP/3 の静的配信がリクエストごとにファイル全体を `offload`
（専用スレッドプール）経由で読み直していた問題を、本体を `bytes::Bytes` で保持し
参照カウントクローンで配信する `[static_file_cache]`（既定オフ）で解消した。
HTTP/1.1 は `sendfile(2)`/kTLS のゼロコピー経路なので対象外。

**同一バイナリで設定だけを切り替え、各ラウンドで off → on を交互に実行**した
（VM のスループット単調劣化による順序バイアスを避けるため。
`tools/perf/freebsd/ab_content_cache.sh`）:

| ラウンド | 54KB off | 54KB on | 3B off | 3B on |
|---|---|---|---|---|
| 1 | 22,198 | 22,346 | 172,479 | **224,608** |
| 2 | 22,313 | 22,318 | 172,972 | **224,590** |
| 3 | 22,234 | 22,145 | 164,557 | **220,529** |
| 4 | 22,082 | 22,250 | 159,286 | **222,490** |

- **小さな応答（3B）: 中央値 168,700 → 223,500 rps（+32%）**。4 ラウンドすべてで改善。
- **大きな応答（54KB）: 有意差なし**。54KB の TLS 応答は約 1.2 GB/s の帯域律速であり、
  **リクエスト単価**を削る本改修では改善しないため（理論どおりの結果）。
- キャッシュ有効時の値は極めて安定（220〜225k）だが、無効時は計測を重ねるにつれ
  劣化する（172k → 159k）。offload スレッドプール往復が環境の状態に敏感なことを示す。

なお本改修のレビューで、`try_insert` が DashMap の `entry()`（シャードの write ロック）
保持中に `len()`（全シャードの read ロック）を呼ぶ**自己デッドロック**を発見・修正した。
キャッシュ有効時は最初のミス挿入で必ず通る経路のため、**本番でもハングする**バグだった
（既定オフのため E2E では顕在化しない）。

### FreeBSD の kTLS は大きな応答で**不利**（2026-08-08、実測）

54KB の応答が 1.2 GB/s で頭打ちになる原因を調査した結果、**CPU 律速ではなく
コンテキストスイッチ律速**であることが判明した。

計測（`vmstat -w 2` / `top -b -n 2`。FreeBSD の `top -b -n 1` は CPU% の
初回サンプルが無意味なので必ず 2 サンプル以上取ること）:

| 指標 | 値 |
|---|---|
| CPU | user 10% / system 38% / **idle 51%** |
| コンテキストスイッチ | **約 400,000 回/秒**（= 1 リクエストあたり約 17 回） |
| veil プロセス | 1 コアの約 20% |

CPU に余裕があるのにスループットが伸びず、しかも**接続数を 64 → 512 に増やしても
1.2 GB/s から動かない**（23,482 → 22,359 rps）。これは CPU 容量ではなく
**直列化点**がボトルネックであることを示す。

原因は FreeBSD の **software kTLS が TLS レコードごとにカーネルワーカースレッドへ
暗号処理をディスパッチする**こと。54KB = 16KB レコード 4 個ぶんのディスパッチが
発生し、そのスケジューリング往復が支配的になる。

kTLS 有効 / 無効の交互 A/B（4 ラウンド、HTTP/2・54KB）:

| ラウンド | kTLS 有効 | kTLS 無効 |
|---|---|---|
| 1 | 22,174 | 23,407 |
| 2 | 21,859 | 29,685 |
| 3 | 22,102 | **41,083** |
| 4 | 22,351 | 26,034 |

- **中央値で約 +26%**（22,138 → 27,860 rps）。全ラウンドで kTLS 無効が上回る。
- kTLS 有効時は条件によらず 21.9〜22.4k で**張り付く**（直列化ceiling）のに対し、
  無効時は 23〜41k とホスト状況に応じて変動する（＝直列化から解放されている）。
  ラウンド 3 の 41,083 rps は **2.24 GB/s** で、nginx の 2.55 GB/s に迫る。
- **小さな応答では有意差なし**（1 応答 = 1 TLS レコードなのでディスパッチ回数が少ない）。

**運用指針**: FreeBSD では大きなレスポンスを扱う用途で `[tls] ktls_enabled` を
**有効にしないこと**（既定は false なので、明示的に true にしない限り影響は無い）。
小さなレスポンス主体なら差は無い。Linux の kTLS はこの制約と無関係
（実装が異なる）。

### 残存ボトルネックへの対応（2026-08-10〜11、F-150 / F-151）

上記の計測で「未対応」として残していた 2 件に着手し、**どちらも原因を特定して改修した**。
経緯と実験結果は [`docs/artifacts/f151_h3_loop_scaling_experiment.md`](../artifacts/f151_h3_loop_scaling_experiment.md)、
設計は [F-150](../backlog/features/F-150-rustls-writev-zero-copy-send.md) /
[F-151](../backlog/features/F-151-http3-event-driven-connection-sweep.md) を参照。

#### 1. 54KB TLS が 1.2 GB/s で頭打ち → **TLS 送信経路のコピーだった**（F-150）

当時の見立て（「TLS 送信経路のコピー回数が効いている」）は**正しかった**。
コードリーディングで具体的な箇所を特定した:

- rustls の `ConnectionCommon::write_tls` は内部の暗号文チャンク列（`ChunkVecBuffer`）を
  `write_vectored(&[IoSlice; <=64])` **1 回**で吐き出すが、`src/simple_tls.rs` /
  `src/ktls_rustls.rs` は書き込み先を毎回 `Vec::new()` にしていた。
  結果、**1 レスポンスあたり暗号文の全 memcpy（54KB 応答なら 54KB）+ malloc/free 1 組**が
  発生していた（AGENTS.md ホットパス絶対規則違反）。
- ヘッダとボディでフラッシュが 2 回走り、`write(2)` が 2 回発行されていた。
- kTLS 無効（＝ FreeBSD の推奨構成）の HTTP/1.1 静的配信は `sendfile(2)` に乗らず、
  **リクエストごとに `pread(2)`** でファイルを読み直していた（F-146 は
  「HTTP/1.1 は sendfile 経路だから対象外」としていたが、その前提が崩れていた）。

改修: fd 直結の `writev(2)` ライタ（新規 `src/tls_writev.rs`）で中間コピーと malloc を排除、
ヘッダ+ボディを 1 フラッシュへ合流（`write(2)` 2 回 → `writev(2)` 1 回）、
kTLS 無効 HTTP/1.1 の静的配信を `[static_file_cache]` の対象に追加（既定オフのため既定挙動は不変）。

#### 2. HTTP/3 が nginx 比 0.5 倍 → **QUIC の暗号処理でも輻輳制御でもなかった**（F-151）

当時の見立て（「QUIC の暗号処理・輻輳制御側の調査が必要」）は **計測により否定された**。
実装前に Linux 実機で 4 つの実験を行い原因を確定させた:

| 実験 | 結果 |
|---|---|
| 接続数スケーリング（1〜128） | 1〜32 は約 3,200 rps で頭打ち（接続数非依存）、**64 超で急落し 128 で 1/3** |
| `cc_algorithm`/`pacing` の A/B（`bbr`+pacing vs `cubic`+pacing 無効、3 ラウンド交互） | **有意差なし**＝当初仮説の否定 |
| `h3load` 2 プロセス並走 | 合計 3,390 rps（単一時と同水準）＝ **クライアント律速ではない** |
| **`mmsg_batch_size` 8 vs 128（c=128、交互 2 巡）** | **577/680 → 2,049/2,006 rps（3.2 倍）** |

最後の実験が決定的で、**トラフィック量・暗号処理量・輻輳制御はまったく同じまま
「ループイテレーション回数」だけを変えるとスループットが 3.2 倍動いた**。
真因は HTTP/3 メインループが **1 イテレーションあたり接続マップ全体を最大 6 回スイープ**
していたこと（タイムアウト最小値算出 / 全接続 `on_timeout` / 受信後 send / H3 poll /
ストリーム駆動 / 末尾 send。`send_pending_packets` はパケット受信時に 1 イテレーションで 2 回）
であり、計測条件（64〜100 接続）はまさにこの固定費が支配する領域だった。

改修: ダーティ接続集合 + タイマー最小ヒープ + per-connection なバックエンド起床通知で
イベント駆動化し、**実際に仕事のある接続だけ**を処理する。

**A/B（同一ホスト・交互実行、64 接続 = FreeBSD ハーネスの既定値、6 ラウンド）**:

| ラウンド | 改修前 | 改修後 |
|---|---|---|
| 1 | 2,495.77 | **3,977.92** |
| 2 | 1,639.98 | **4,240.33** |
| 3 | 3,355.23 | **4,109.07** |
| 4 | 2,448.33 | **4,199.47** |
| 5 | 3,631.53 | **4,047.35** |
| 6 | 2,968.03 | **4,417.82** |
| **中央値** | **2,732** | **4,154（+52%）** |

全 6 ラウンドで改善し、**ばらつきが ±40% → ±5% に縮小**した（旧実装のスループットが
「たまたま何接続が忙しかったか」に左右されていたことを示す）。
128 接続（1,280 リクエストが常時 in-flight）では +7%・誤差内で、これはこの領域では
どの接続にも毎パス実際に仕事があり **ダーティ集合 = 全接続が正しい状態**＝
削れる無駄がそもそも無いためである。

## F-153: 静的配信のパス解決（2026-08-15）

「h2c 平文が対 nginx 0.65」という残件を調査した結果、**h2c 固有の問題ではなく
静的配信のリクエスト単価**が原因だった（h2c は TLS の暗号コストが無いぶん固定費が
そのまま露出する。同じ経路を HTTP/1.1・HTTP/2・HTTP/3 も通る）。

`strace` で **1 リクエストあたり `readlink` 7 回・全件エラー**を検出。
`canonicalize()` がパスの全コンポーネントに `readlink` していた。
解決を「canonicalize してから含有チェック」から
「静的ルート dirfd 相対 open + カーネルの `RESOLVE_BENEATH` 封じ込め」へ置き換えた
（Linux は `openat2(2)`、FreeBSD は `O_RESOLVE_BENEATH`）。
検査と open が原子的になるため **TOCTOU の窓も消えている**（セキュリティも向上）。

| プラットフォーム | パス解決 syscall（1 リクエストあたり） | h2c スループット |
|---|---|---|
| Linux（glibc） | `readlink` **7 回 → 0 回** | 中央値 7,820 → **9,132（+17%）**、対 nginx 0.69 → 0.81 |
| FreeBSD | `__realpathat` **1 回 → 0 回** | ほぼ変化なし（対 nginx 0.65 → 0.68） |

### 教訓: `canonicalize()` のコストは libc 実装依存で桁が違う

**Linux（glibc）は `realpath` をユーザ空間で実装しパスの全コンポーネントに `readlink` する**
のに対し、**FreeBSD は `__realpathat` という単一 syscall** で済ませる。
同じ Rust の `Path::canonicalize()` でも**コストが 7 倍違った**。
「同じ API だから同じコスト」と考えず、**プラットフォームごとに syscall を数えること**。

## B-65: 静的配信の offload 往復を 2 回 → 1 回（2026-08-16）

F-153 の後も h2c が nginx に届かなかったため追加調査した。`strace` で書き込みの**中身**を
見たところ、20 リクエストに対する `write` 40 回の**全てが 8 バイトの eventfd 書き込み**
（= `runtime::offload` の完了通知）で、**実データの `write` は 1 回も無かった**
（レスポンス本体は io_uring 経由のため `write(2)` として現れない）。

つまり当初疑った「F-74 のフレーム連結が効いていない」は**誤り**で、真因は
**静的配信が 1 リクエストあたり offload を 2 回呼んでいた**ことだった
（パス解決+stat / 本体読み込み）。F-153 の `open_beneath` は既に open+fstat 済みの fd を
持っているので、同じ offload クロージャ内で本体まで読んで往復を 1 回に統合した。

| | 20 リクエストあたりの offload 往復 |
|---|---|
| 改修前 | 40（2 回/req） |
| 改修後 | **20（1 回/req）** |

### FreeBSD でも改善（しかも capsicum を有効にした状態で）

FreeBSD 14.3 amd64（QEMU/KVM）、3 バイト応答、**capsicum + capability mode を有効化**して計測:

| iter | veil | nginx | 比 |
|---|---|---|---|
| 1 | 10,702 | 13,550 | 0.79 |
| 2 | 12,588 | 16,605 | 0.76 |
| 3 | 9,102 | 11,796 | 0.77 |

**対 nginx 比は 0.68 → 0.77 へ改善**（前回計測は F-153 のみ・capsicum 無効）。
**より厳しいセキュリティ設定にしたうえで**改善している点に注意。
絶対値はホスト状況で 2 倍近く振れる（nginx が 11,796〜16,605）が、
**比は 0.76〜0.79 と安定**しており、比で判定する本ドキュメントの方針が有効に働いている。

capability mode 下で正常に配信できていること自体が **F-123（cap-mode の dirfd 相対化）と
F-153/B-65 の回帰テスト**になっている（errors = 0）。

### スループット（**セキュリティ機能を有効にした状態**、交互 A/B 5 ラウンド）

| | rps（中央値） | 対 nginx |
|---|---|---|
| F-153/B-65 前 | 6,599 | 0.58 |
| **F-153 + B-65 後** | **8,318（+26%）** | **0.73** |

**5 ラウンドすべてで改善**し、ばらつきも小さい（±1.3〜2%）。

> **計測方針の変更**: 本計測から **`seccomp(filter)` + `Landlock` を有効にした状態**を
> 標準とする（本番に近い条件で測らなければ意味がないため）。セキュリティ機能のコストは
> 実測で約 15%。FreeBSD ハーネスも `capsicum` + capability mode を有効にした。
> この構成は **F-153 の `openat2` が seccomp 許可リストに入っているかの回帰テストも兼ねる**
> （入っていなければ静的配信が全て 404 になる）。

### 運用指針: 静的配信が主用途なら `open_file_cache` を有効にする

F-153 の改修後でも、`open_file_cache` を有効にすると Linux の h2c 小レスポンスで
さらに約 +5%（10,610 → 11,097 rps）となり、**同時計測の nginx（11,272）とほぼ同等**になる。
改修前は同じ設定変更で **2 倍**（7,005 → 14,028）の差がついていたので、
**本改修がキャッシュの仕事の大半を先に済ませた**形になる。

既定をオンにはしていない。`valid_duration_secs` の間ファイル更新が反映されなくなる
という**観測可能な挙動変更**であり、性能だけを理由に既定を変えるべきではないため。
静的配信が主用途で、コンテンツ更新の反映遅延を許容できる場合は明示的に有効化すること。

```toml
[open_file_cache]        # または [route.open_file_cache]
enabled = true
valid_duration_secs = 60
max_entries = 10000
```

### FreeBSD の h2c ギャップは別原因（未解決）

FreeBSD では本改修が正しく効いている（`__realpathat` が 0 回になった）にもかかわらず
対 nginx 0.68 のままであり、**ギャップの主因はパス解決ではない**ことが確定した。
残る候補は 1 リクエストあたり `write` 2 回・`read` 約 4.7 回・offload 往復。
→ B-65 として別途調査する。

## 教訓（計測方針に反映済み）

- **コンテナ（veth/bridge）では kTLS が不利**。feat 系構成は kTLS 既定オフ。
- **ホスト負荷（co-tenant のビルド等）が計測ノイズの支配的要因**。静穏ウィンドウ
  （1 分 loadavg 目安 < 1.5）を確認してから計測し、比較は必ず**同日・同一環境の A/B**
  （nginx 併走で環境ノイズを正規化）で行う。
- **h2load の `failed`（ストリームエラー）は Non-2xx に計上されない**。Errors=0 でも
  異常低スループット時は h2load の `requests:` 行とサーバ warn ログを確認する
  （B-43・B-46 の教訓）。
- **Docker seccomp 許可リストは使用 syscall の追加に追随させる**（F-115 の教訓）。
- **h2load は既定 1 スレッドでクライアント律速になり得る**。HTTP/2 で 2800 req/s 級以上を
  計測する際は `H2_ARGS='-n 60000 -c100 -m10 -t4'` を併用する（F-116 の教訓）。
- **高並行の多重化計測は fd 上限・接続チャーンの検出器になる**。0 req/s 近傍や反復劣化を
  見たら、サーバの `Too many open files` / `Backend connect error` ログと
  `/proc/net/tcp` の状態分布を確認する（B-44/B-45 の教訓）。
- **git worktree から tools/perf を実行する場合、git 管理外の生成物
  （`docker/assets/ssl/*.pem` 等）を本体ツリーからコピーする**（F-116 A/B の教訓）。
- **FreeBSD の kTLS は大きな応答で不利**（`sysctl kern.ipc.tls.enable=1` でカーネル側を
  有効にしたうえで veil の `ktls_enabled = true` にした場合）。software kTLS が TLS
  レコードごとにカーネルワーカースレッドへディスパッチするため、54KB 応答で
  **中央値 26% 低下**し 1.2 GB/s で直列化により張り付く（上記「FreeBSD の kTLS は
  大きな応答で不利」節）。計測前に `kern.ipc.tls.stats.sw.gcm` が増えているかで実際に kTLS
  セッションが張られたかを確認する（F-145 の教訓）。
- **QEMU/HVF 上の VM は連続計測でスループットが単調に劣化する**（実測: 同一バイナリで
  18,217 → 15,153 → 12,845 rps）。A/B で「先に走った方が有利」という順序バイアスが
  効果量と同オーダーになるため、**交互かつ短時間サンプルを多数取る**こと。本環境では
  5〜10% の差は判定できない（F-145 の教訓）。
- **reactor バックエンド（BSD/macOS/`full-container`）は Linux 既定ビルドで 1 行も
  コンパイルされない**。`src/runtime/reactor/` を変更したら
  `VEIL_E2E_FEATURES="full,epoll" ./tests/e2e_setup.sh test` を必ず回す。F-145 では
  「最初の 1 リクエストでサーバがハングする」変更が単体 816 / 統合 53 / E2E 541 件の
  **全テストを通過**した（AGENTS.md に明記済み）。
- **「未対応ボトルネック」の原因仮説は、実装前に必ず計測で確定させる**（F-151 の教訓）。
  `docs/perf` に書いていた「HTTP/3 は QUIC の暗号処理・輻輳制御が原因と見られる」という
  仮説は、`cc_algorithm`/`pacing` の A/B で**有意差なし**として否定された。真因は
  メインループの O(N_conn) 固定費で、**`mmsg_batch_size` を変えて「ループ回数だけ」を
  動かす実験**（トラフィック量・暗号処理量・輻輳制御は不変）で 3.2 倍の差が出たことで
  確定した。**「何を変えたら何が動くか」を 1 変数に絞った実験を先に作ること。**
- **接続数を振って測ると律速の性質が分かる**（F-151 の教訓）。接続数に依存しない頭打ちは
  「1 リクエストあたりのコスト」、接続数に比例して悪化するなら「接続数あたりの固定費」。
  さらに**負荷生成側を 2 プロセスに割って合計が変わらないこと**を確認すると、
  クライアント律速を確実に排除できる。
- **計測ハーネスは「0 rps を記録して正常終了する」失敗モードを持つ**（2026-08-11 の教訓）。
  FreeBSD の HTTP/3 計測は、(1) `h3load` が密着形短オプション（`-t2`）を受理せず即終了、
  (2) 固定リクエスト数を外側 `timeout` が撃ち殺して集計行が出ない、の 2 件により
  **veil・nginx とも 0 rps** を記録し続けていた。どちらもハーネスはエラー終了しない。
  **新しい環境で計測したら、まず 0 / NA の行が無いかを確認すること。**
  負荷生成は可能なかぎり**固定リクエスト数ではなく時間で区切る**（マシン速度に依存しない）。
- **計測は必ず静穏ホストで**。並行ビルド中の E2E は 415 passed / 127 failed になり、
  静穏時は 541 passed / 1 failed だった（所要 822 秒 → 88 秒）。大量失敗を見たら
  まず loadavg を疑う。

## 再現手順

### FreeBSD ネイティブ計測

```bash
# 1) FreeBSD ゲストを起動してビルド（Apple Silicon macOS では HVF で実用速度）
tools/qemu/bsd-vm.sh freebsd aarch64 up
CARGO_FEATURES=full-freebsd-aarch64 tools/qemu/bsd-vm.sh freebsd aarch64 build

# 2) HTTP/3 クライアント（FreeBSD には QUIC 対応 h2load が無いため必須）
#    ゲスト内で:
cargo build --release --manifest-path tools/perf/h3load/Cargo.toml-aarch64

# 3) 計測（ゲスト内。nginx / wrk / nghttp2 が必要）
pkg install -y nginx nghttp2 wrk-luajit
sh tools/perf/freebsd/run_perf_freebsd.sh -r 3 -d 10                 # 54KB
sh tools/perf/freebsd/run_perf_freebsd.sh -r 3 -d 10 -p /small.html  # 3B

# ホスト（macOS）からは薄いラッパ経由でも実行できる
bash tools/perf/freebsd/vmrun.sh -r 3 -d 10
```

### Docker ベース計測（Linux）

```bash
docker build -f docker/Dockerfile.glibc -t veil:glibc --build-arg CARGO_FEATURES='full' .
docker build -f docker/Dockerfile.musl  -t veil:musl  --build-arg CARGO_FEATURES='full' .
docker build -t local/h2load-h3:latest tools/perf/h2load-http3   # HTTP/3 クライアント

bash tools/perf/gen_configs.sh
bash tools/perf/run_perf.sh                                      # 全構成スイート（~5 時間）
# scoped 計測の例:
CONFIG_GLOB='h2_1_feat_http3'             bash tools/perf/run_perf.sh   # HTTP/3 file
CONFIG_GLOB='h2_1_ktls_0_lb_kernel_ofc_1' bash tools/perf/run_perf.sh   # H1/H2 best
CONFIG_GLOB='grpc_*'                      bash tools/perf/run_perf.sh   # gRPC
CONFIG_GLOB='h2_0_feat_l4'                bash tools/perf/run_perf.sh   # L4

# B-46 リグレッション確認（修正前は 2xx ヘッダのみ・ボディ 0B で全 failed になる）
# h3_proxy_buffering 構成の veil に対して:
h2load --alpn-list=h3 -n 100 -c 10 -m10 https://<veil>:443/
```

## HTTP/3 A/B: F-129（RECVMSG+CC/pacing）vs F-115（POLL+recvmmsg）（2026-07-20、host-net h2load）

同一ホスト・同一構成（release full features、GSO on、静的配信、h2load `--alpn-list=h3 -n30000 -c100 -m10` ×3、全 2xx）で
**以前実装（F-115: `POLL_ADD`+libc `recvmmsg`・quiche 既定 CUBIC）** と
**新実装（F-129: 先頭 `IORING_OP_RECVMSG`+`POLL_FIRST` 単発 + recvmmsg drain・BBR+pacing+hystart・mmsg batch 64）** を比較:

| 実装 | HTTP/3 Req/s（iter1/2/3, median） | 備考 |
|---|---|---|
| OLD（F-115） | 7364 / 6666 / 6931（median **6931**） | POLL+recvmmsg・CUBIC |
| NEW（F-129） | 8525 / 7588 / 8393（median **8393**） | RECVMSG+POLL_FIRST・BBR/pacing/hystart |

**F-129 は F-115 比 +21%（8393/6931 = 1.21×）** の HTTP/3 スループット改善。主因は
quiche CC の CUBIC→BBR + pacing/hystart と、先頭データグラム受信の io_uring 化（POLL 二重往復排除）。
これを基準に F-130（極限 io_uring 化: 受信 drain / 送信の io_uring 化・真 multishot）で更に詰める。

## HTTP/2 kTLS ホストベンチ（2026-07-20、HTTP/3 と同条件で比較）

HTTP/3 が host-net h2load で ~7500 req/s だったため、HTTP/2 も同一ホスト・同一構成
（release full、静的配信、h2load `-n30000 -c100 -m10` ×3、全 2xx、GSO 環境）で kTLS 有効/無効を測定:

| 構成 | HTTP/2 Req/s（iter1/2/3, median） | 備考 |
|---|---|---|
| HTTP/2 + kTLS（`ktls_enabled=true`） | 4153 / 4225 / 4125（median **4153**） | kTLS 有効化はログ確認（AES-GCM offload available） |
| HTTP/2 rustls（kTLS 無効） | 4162 / 4175 / 4357（median **4175**） | ユーザ空間 rustls |
| （参考）HTTP/3 QUIC | 7489 / 7868 / 7547（median **~7547**） | 同ホスト・同 h2load params |

**発見**:
1. **ループバック（127.0.0.1）では kTLS の効果はほぼ無い**（4153 ≈ 4175）。kTLS は実 NIC の
   ハードウェア暗号オフロードで効くもので、loopback では in-kernel 暗号のままユーザ空間 rustls と
   スループット差が出ない（`docs/perf` の「kTLS は veth/コンテナと相性が悪い」と整合。ベアメタル
   実 NIC 環境向け）。
2. **host-net + GSO 有効では HTTP/3(~7547) > HTTP/2(~4150)**。HTTP/3 は QUIC の GSO バッチ送出
   （1 syscall で複数データグラム）と io_uring 受信の効果で、TCP/HTTP2 を上回る。docker bridge
   （GSO/GRO 無効）では逆に HTTP/3 が不利（~835）になるため、計測は host net と bridge を分ける。

## F-130 極限 io_uring 化 A/B: F-130（C1+C3）vs F-129（2026-07-20、host-net、back-to-back 交互 4 反復）

同一ホストで F-129 と F-130 を **交互 4 反復**（ホスト変動キャンセル）で計測（release full、GSO on、h2load h3 `-n30000 -c100 -m10`、全 2xx）:

| iter | F-129 (RECVMSG単発+libc recvmmsg/sendmmsg) | F-130 (パイプライン RECVMSG×N + SENDMSG io_uring) |
|---|---|---|
| 1 | 7180 | 7553 |
| 2 | 6411 | 7315 |
| 3 | 6925 | 7111 |
| 4 | 6986 | 7322 |
| **median** | **~6955** | **~7318（+5.2%）** |

**F-130 が全 4 反復で F-129 を上回り +5.2%**。ホットパスから libc `recvmmsg`/`sendmmsg` を排除し、
受信は N 本の独立 `IORING_OP_RECVMSG` を常時 in-flight（per-slot 固定 msghdr で peer 安全）、
送信は `IORING_OP_SENDMSG`（GSO cmsg）を複数 SQE で 1 submit した効果。真 multishot（C2）は
kernel 6.0+ 依存・multi-peer 安全性の実装コストと F-129 での不安定化実績から次段送り（フォールバック維持）。

---

## F-157: h2c 平文の対 nginx 劣後を解消（2026-08-17、FreeBSD 14.3 aarch64）

### 結果（54,576B、3 反復中央値、`static_file_cache` + `open_file_cache` 有効）

| シナリオ | veil | nginx | 対 nginx | 開始時 |
|---|---|---|---|---|
| `h2_file_tls` | 34,804 | 24,248 | **1.44** | 1.05 |
| `h1_file_tls` | 33,864 | 26,111 | **1.30** | 1.08 |
| `h2_proxy_tls` | 16,115 | 12,907 | **1.25** | 1.17 |
| `l4_tcp` | 43,822 | 39,801 | **1.10** | 1.13 |
| `h1_file_plain` | 57,213 | 54,754 | **1.04** | 1.03 |
| `h1_proxy_tls` | 17,613 | 17,712 | 0.99 | 1.02 |
| `h3_file` | 3,571 | 3,689 | 0.97 | 0.98 |
| `h2c_file_plain` | 58,938 | 73,380 | 0.80 | **0.51** |

> 比較対象の nginx にも `open_file_cache` を入れて条件を揃えている（片側だけ
> チューニングして測らない。F-155 の kTLS と同じ失敗を繰り返さないため）。
> VM の絶対値はラウンド間で 1.8 倍変動するので、判定は必ず**同一ラウンド内の
> 対 nginx 比**で行うこと。

### 何がボトルネックだったか（DTrace、N=128,000 で正規化）

h2c だけが劣後していた理由は「HTTP/2 のフレーミングが遅い」ではなく、
**リクエストごとのファイル読み込みと offload スレッドプール往復**だった。

| syscall | 改修前 | 最終 |
|---|---|---|
| `openat` / `fstat` / `lseek` / `close` | 1.0 / 2.0 / 1.0 / 1.0 | 各 0.016〜0.032 |
| `read` | 3.00 | 0.078 |
| 1 バイト `write`（offload 完了通知パイプ） | 1.00 | 0.016 |
| `_umtx_op`（offload のスレッド間同期） | 1.58 | 0.016 |
| 合計 | **約 12** | **約 1.1** |

nginx は h2c でも `sendfile(2)` + `sf_hdtr` でカーネル内完結できるが、veil は
HTTP/2 の DATA 再フレーミングが要るため構造的に `sendfile` に載せられない。
TLS 版 HTTP/2 が元から勝っていたのは、暗号処理コストが両者で支配的になり
この固定費が相対的に隠れていたためで、平文でマスキングが外れて露呈していた。

### 実施した改修

1. **ホットパスのディープコピー・確保を排除** — `build_h2_compressed_file_response` の
   `to_vec()`（F-146 のゼロコピー設計を無効化していた）、`fill_read_buf` の `split_off`、
   `drive_h2_streams` の毎ループ `collect()`。
2. **静的コンテンツキャッシュ経路の是正（効果最大）** — `static_file_cache` と
   `open_file_cache` は**セットで有効にしないと効かない**（offload ゼロ経路は
   メタデータキャッシュのヒットを前提に本体キャッシュを参照する構造）。

### 棄却した案: DATA フレームの `writev` ゼロコピー化

`sendmsg` の N 本 iovec で 54KB の memcpy を消す案を実装・検証まで行ったが、
**交互 A/B 4 ラウンドすべてで回帰**したため revert した（対 nginx 0.886 → 0.807）。

| round | base | writev |
|---|---|---|
| 1 | 0.896 | 0.873 |
| 2 | 0.894 | 0.841 |
| 3 | 0.864 | 0.784 |
| 4 | 0.878 | 0.775 |

事前見積もりが memcpy コストを DRAM 帯域（5 GB/s → 1 リクエスト 5.4µs）で計算していたのが
誤りで、**同じファイルを毎回配信するベンチではコピー元が L2/L3 に residence し続け、
memcpy は見積もりよりはるかに安い**。一方 `sendmsg` の per-iovec コストは実在する。

### 残件

`h2c_file_plain` は 0.80〜0.89 でまだ nginx を超えていない。残差は HTTP/2 の
per-request タスク spawn + チャネル + `Notify` 起床の固定費と見られる
（`poll` が 0.53/req 残っているのもここ）。解消には
`docs/artifacts/freebsd_h2c_perf_investigation.md` の Phase 3
「静的キャッシュヒット時の per-stream タスク spawn バイパス」が要る。

---

## F-157: Linux 退行確認と h2c 新規計測（2026-08-17、Linux x86_64 / io_uring）

### 退行確認（`h2_1_ktls_0_lb_kernel_ofc_1`、3 反復中央値）

| Target | Proto | 2026-08-17 | 2026-08-16 | 差 |
|---|---|---|---|---|
| veil_glibc | HTTP/1.1 | **3008.1** | 2969.5 | +1.3% |
| veil_musl | HTTP/1.1 | **2967.8** | 2968.1 | ±0% |
| veil_glibc | HTTP/2 | **2746.7** | 2636.1 | +4.2% |
| veil_musl | HTTP/2 | **2702.4** | 2709.9 | −0.3% |
| nginx（同時計測） | HTTP/1.1 | 2177.6 | 1658.2 | — |
| nginx（同時計測） | HTTP/2 | 2287.2 | 1884.4 | — |

**退行なし**（veil 側の絶対値は同等以上）。nginx の絶対値が上がっているのは
ホストが静かだったためで、同時計測での対 nginx 比は HTTP/1.1 **1.38 倍**・
HTTP/2 **1.20 倍**と veil 優位を維持している。

### h2c（平文 HTTP/2 prior knowledge）新規計測

本リリースで `tools/perf` に h2c 計測を追加した（`h2c_file` / `h2c_proxy`）。
veil の平文リスナー（`h2c_listen`）は **h2c 専用**で HTTP/1.1 を受け付けないため、
比較対象の nginx も `listen 8080; http2 on;` で h2c を有効化し、
双方に静的配信の推奨キャッシュ設定を入れて条件を揃えている。

| 構成 | veil_glibc | veil_musl | nginx | 対 nginx |
|---|---|---|---|---|
| `h2c_file`（静的配信） | **9051.9** | 8845.7 | 3687.5 | **2.45 / 2.40 倍** |
| `h2c_proxy`（逆プロキシ） | **2367.2** | 2231.8 | 2112.5 | 1.12 / 1.06 倍 |

> **Linux（io_uring）では h2c が nginx の 2.45 倍**であり、FreeBSD 側で観測された
> h2c の劣後（0.80〜0.89）は **reactor/kqueue バックエンド固有**であることがわかる。
> FreeBSD 側の残件は F-157 チケットの「残件」節を参照。

---

## F-158: h2c per-stream タスクのインライン初回 poll（2026-08-18、FreeBSD 14.3 aarch64 / QEMU+HVF）

### 背景: F-157 の残件だった「タスク spawn の固定費」を消した

F-157 は `h2c_file_plain` が対 nginx 0.80〜0.89 に留まる残件を
「HTTP/2 の per-request タスク spawn + チャネル + `Notify` 起床の固定費」と診断し、
`docs/artifacts/freebsd_h2c_perf_investigation.md` の Phase 3
「静的キャッシュヒット時の per-stream タスク spawn バイパス」を要ると結論していた。

**採用した設計は「バイパス」ではなく「インライン初回 poll」**である
（詳細と却下した代替案は `docs/artifacts/h2c_optimize_design_review.md`）。
ルーティング・セキュリティ検査・WASM・圧縮・アクセスログを**一切複製せず**、
`TaskPool::spawn_inline` が「タスクをスラブへ登録し、そのタスク自身の実 Waker で
1 回だけその場で poll する」。静的キャッシュヒット時のタスクは実際に pend する
await を 1 つも持たないため `Poll::Ready` まで進み、**エグゼキュータへ一度も
登録されない**（＝ spawn → イベントループ 1 周 → 実行、の強制往復と、
その待機に伴う `poll(2)`・`Notify` 起床が消える）。

### 実測（交互 A/B・10 ラウンド・`/root/ab/{base,new}/veil`・md5 で別バイナリを確認済み）

**3B（リクエスト単価）**

| | 中央値 rps | 最小 | 最大 |
|---|---|---|---|
| base | 555,119 | 441,871 | **581,694** |
| new | **706,779** | **652,805** | 727,641 |

**中央値 +27.3%。10 ラウンド全勝、かつ 2 つの分布は完全に分離している**
（base の最大 581,694 < new の最小 652,805）。F-156 が警告する
「4 ラウンド方向が揃っても有意ではない」水準を明確に超えている。

**54,576B（バイト単価）**

| | 中央値 rps |
|---|---|
| base | 96,981 |
| new | **97,794** |

**+0.8%（8/10 勝）＝ 退行なし。**

**この非対称性は設計どおり**である。消したのは「1 リクエストあたりの固定費」なので、
3B のようにボディが小さいリクエストでは支配的に効き、54KB では
バイト転送コストに埋もれる。**逆に言えば、54KB でも大きく改善したなら
それは説明のつかない結果であり、計測を疑うべきだった。**

### 注意: 絶対値を過去セッションと比較してはならない

本計測の絶対値（veil 3B 約 70 万 rps、nginx 約 19 万 rps）は
2026-08-17 の記録（veil 168,032 / nginx 374,569）と大きく異なる。
VM 状態・ハーネス条件が異なるためであり、**本ドキュメントの既定方針どおり
判定は「同一セッション内の交互 A/B」だけで行っている**。
なお本セッションでは **nginx 側の 3B が 11 万〜54 万 rps と 5 倍近くばらついた**
（veil 側は同一変種内で ±9% に収まる）。3B での対 nginx 比は信頼できないため、
上表は veil の絶対値のみで比較している。

生データは [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) 末尾の
`# ==== 2026-08-22 F-158 交互 A/B ...` の 2 節（3B / 54,576B、各 10 ラウンド）。
`server` 列の `veil_base` / `veil_new` で変種を区別する。

### F-158 の Linux 計測: io_uring では退行したため reactor 限定にした（2026-08-18）

FreeBSD で +27.3% を確認したあと、**既定バックエンドである Linux io_uring でも
交互 A/B を取ったところ逆に退行していた**。同一イメージ構成（`veil:glibc` = F-158 /
`veil:glibc-base` = 直前コミット、イメージ digest が別物であることを確認済み）で、
ラウンドごとに実行順を入れ替えて計測した。

| 構成 | base 中央値 | new 中央値 | 差 | base の勝ち |
|---|---|---|---|---|
| `h2c_file`（12 ラウンド） | 9,678.9 | 9,231.8 | **−4.6%** | **11/12** |
| `h2c_file`（別途 6 ラウンド） | 9,563.6 | 9,043.5 | −5.4% | 5/6 |
| `h2c_proxy`（6 ラウンド） | 2,744.1 | 2,639.1 | −3.8% | 5/6 |

**計 24 ラウンド中 21 でベースラインが勝った。**

原因は「インライン初回 poll が消せる往復コスト」がバックエンドで違うこと:
reactor の待機は `Readable::poll` の**同期 `poll(2)`** を伴う（F-141/F-155 により
この `poll(2)` フォールバック自体は削除禁止）のに対し、io_uring の同じ待機は
`IORING_OP_POLL_ADD` の SQE 1 本で他の SQE とまとめて submit されるため元から安い。
残るのはインライン poll 側の固定費と SQE バッチングの乱れだけになり、差し引きで退行する。

**対処**: `h2_task_spawner` を `veil_rt_reactor` 限定に cfg 分岐し、
`src/runtime/uring/executor.rs` は F-158 以前と 1 バイトも変わらない状態へ戻した。

> **教訓: 同一の変更が kqueue で +27.3%、io_uring で −4.6% と正反対になった。**
> 仕様書は FreeBSD の h2c と Linux の h2c proxy を 1 つの問題として共通の改修案を
> 並べていたが、**待機プリミティブが違う以上この 2 つは別の問題である**。
> ホットパス最適化は片方の結果をもう一方へ一般化せず、**必ず両バックエンドで
> 交互 A/B を取ること**。FreeBSD の結果だけで採用していれば、
> 既定プラットフォームに 4.6% の退行を入れていた。

生データは [`results_raw.tsv`](results_raw.tsv) 末尾の
`# ==== 2026-08-22 F-158 交互 A/B ...` の 2 節（12 ラウンド / 6 ラウンド）。
`target` 列の `veil_glibc_base` / `veil_glibc_new` で変種を区別する。

### h2 プロキシ応答の中間 64KB Vec 除去（2026-08-24、B-72 の続き）

B-72 でカーネル側のボトルネックを消したあと、`perf record --call-graph dwarf` で
残りの内訳を取り直した結果に基づく第 2 弾。

| veil CPU に占める割合 | 箇所 |
|---|---|
| 5.78% | `h2_relay_backend_response` → `response_buf.extend_from_slice`（中間 64KB `Vec`） |
| 3.84% | `Bytes::copy_from_slice`（先頭ボディ） |
| 5.82% | `queue_data_frames` → `encode_data_into`（DATA フレーミング） |

このうち **中間 `Vec` への確保 + コピーだけ**を消した。ヘッダーが 1 回目の read で
完結する大半のケースでは、プール済み受信バッファのスライスをそのまま
`parse_http_response` に渡し、`PooledBuf`（Drop で必ず `buf_put` する RAII）で
ボディ参照元として持ち回る。**AGENTS.md「ホットパスでリクエストごとに確保しない」
に対する違反の解消でもある。**

**`queue_data_frames` のコピーには手を付けていない**（F-157 が
「コピー元が L2/L3 に residence していると memcpy は見積もりよりはるかに安く、
iovec 化はむしろ退行する」を実測済みのため）。

#### 結果（交互 A/B 13 ラウンド、各ラウンド 40 秒ウォームアップ後）

| variant | n | 中央値 | 最小 | 最大 |
|---|---|---|---|---|
| base（B-72 のみ） | 13 | 10,135.5 | 9,934.3 | 10,298.5 |
| **new（+ Vec 除去）** | 13 | **10,412.7** | 10,209.1 | 10,553.8 |

**中央値 +2.74%、11/13 ラウンド勝ち。**
**分布は完全分離しない**（base 最大 10,298.5 > new 最小 10,209.1）が、
全ペア 169 通りのうち **165 通り（97.6%）** で new が上回る。
**効果量が ~3% と小さいため、B-72（+30.8%・完全分離）より弱い証拠である点は
明記しておく。**

#### 2 改修の累積（改修前 vs HEAD の直接 A/B）

**改修前（`1f1fcf3`）と HEAD を同一セッションで直接交互 A/B した結果:**

| variant | n | 中央値 | 最小 | 最大 |
|---|---|---|---|---|
| 改修前 | 6 | 7,384.3 | 7,235 | 7,536 |
| **HEAD** | 6 | **10,473.4** | 10,335 | 10,616 |

**累積 +41.8%、6/6 ラウンド勝ち、分布完全分離。**

> **注意**: 2 改修を別々に測った中央値を掛け合わせると +34.2% になるが、
> 各 A/B は**別時刻に取られており掛け算は妥当でない**（計測系に時間ドリフトがある）。
> **同一セッションで改修前後を直接比較した +41.8% を累積効果の正とする。**

#### 静的経路の非退行確認

タイマー改修は `timeout()` を使う**全経路**（HTTP/1.1 プロキシ・HTTP/3・L4・gRPC）に効くが、
静的配信は `timeout()` を 1 度も通らないため恩恵が無く、**新設の per-loop `has_timers()`
チェックぶんの退行が出るならここに出る**。同じ 2 イメージで `h2c_file` を A/B した:

| variant | n | 中央値 | 最小 | 最大 |
|---|---|---|---|---|
| 改修前 | 6 | 24,725.2 | 24,502 | 25,058 |
| HEAD | 6 | 25,027.7 | 24,156 | 25,878 |

**+1.2%・3/6 ラウンド勝ち・値は完全に交互**（全ペアで P(new>base) = 0.72）。
ノイズと区別できず、**退行なし**と判定する。

### B-72: io_uring のタイマー蓄積で ASYNC_CANCEL が O(n) 走査になっていた（2026-08-24）

**Linux h2c 逆プロキシ（`h2c_proxy`）の交互 A/B で中央値 +30.8%**
（7,757.5 → 10,145.4 req/s、9/9 ラウンド勝ち、**分布完全分離**: base 最大 8,120.5 < new 最小 9,951.2）。

#### 何が起きていたか

`perf record` で **veil の CPU の 40.6% がカーネルの io_uring タイムアウトリスト走査**だった。

| self% | シンボル |
|---|---|
| **36.75%** | `[k] io_cancel_req_match` |
| **3.89%** | `[k] io_timeout_extract` |

```
io_uring_enter → io_submit_sqes → io_issue_sqe → io_async_cancel
  → io_try_cancel → io_timeout_cancel → io_timeout_extract → io_cancel_req_match
```

2 つの実装が噛み合って O(n) になっていた:

1. `runtime/uring/timer.rs` の `Sleep::drop` が `detach_op_no_cancel` で
   **キャンセルを投げない**ため、`timeout(READ_TIMEOUT, read)` で内側の read が
   勝つたびに `IORING_OP_TIMEOUT` が **30 秒間** `ctx->timeout_list` に居座る
   （数千 rps で 10 万オーダー）。
2. `h2_select_readable_or_notify` が負け arm の POLL_ADD を in-flight のまま drop し、
   **1 プロキシリクエストにつき 2〜3 回** `IORING_OP_ASYNC_CANCEL` を投げる。
3. カーネルの `io_try_cancel` は poll ハッシュで対象が見つからないと
   `io_timeout_cancel` へフォールバックし、**そのリストを線形走査する**。

**静的配信（`h2c_file`）は `timeout()` を 1 度も通らない**ためリストが空で、
同じ ASYNC_CANCEL を払っていても O(1) で済んでいた。
これが「静的は nginx の 2.2 倍なのにプロキシでは 1.2 倍しか出ない」の正体だった。

#### 修正

`runtime::uring::timer` を `runtime::reactor::timer` と同一の
「スロット + 世代 + `BinaryHeap`」実装へ置き換え、カーネルへの `IORING_OP_TIMEOUT` は
park 直前に**最近接の live デッドラインへ 1 本だけ**アームする方式にした
（`ctx->timeout_list` の長さは常に 0 か 1）。`src/runtime/reactor/` は無変更。

#### 結果（54,576B、quiet host、40 秒ウォームアップ後の定常状態）

| | 静的 | プロキシ | プロキシ/静的 |
|---|---|---|---|
| veil 修正前 | 24,631 | 7,757 | 0.315 |
| **veil 修正後** | 24,631 | **10,145** | **0.412** |
| nginx | 10,795 | 6,507 | 0.603 |
| **veil 修正後 / nginx** | **2.28×** | **1.56×**（修正前 1.19×） | |

- veil の CPU: 222% → **196%**、1 リクエストあたり 253 → **193 µs·コア（-24%）**。
- `perf` の `io_cancel_req_match`: 36.75% → **0.00%**（機序として消えたことを確認済み）。

#### 「h2c proxy を h2c ファイル配信と同等にする」ことについて

**本トポロジ（4 コア 1 台にクライアント・veil・上流を同居）では原理的に達成できない。**
プロキシ構成では上流 nginx が 122%（1 req あたり ~120 µs）を消費するが、静的配信には
上流が存在しない。**veil の CPU を 0 にしても**上限は
`4.00 コア / (クライアント 66 + 上流 120 µs)` ≒ **21,500 rps** で、静的の 24,631 には届かない。
到達可能な目標は「veil 自身の CPU/req を削る」ことであり、B-72 はその 24% を削った。

生データは [`results_raw.tsv`](results_raw.tsv) 末尾の
`# ==== 2026-08-24 B-72 交互 A/B ...` 節。調査の全経緯は
`docs/artifacts/h2c_proxy_bottleneck_investigation.md`。

### h2c_proxy の「静的配信の 1/4」は veil 固有ではない（2026-08-18）

仕様書は Linux `h2c_proxy`（2,367 req/s）を「静的配信 `h2c_file`（9,051 req/s）の
1/4 に激減」と表現し、レスポンスボディの三重コピーを支配的要因としていた。
**本ホストで nginx をプロキシとして同じ計測をすると、nginx も同率で落ちる。**

| ペイロード | nginx 静的 | nginx プロキシ | 比 |
|---|---|---|---|
| 54,576B | 3,168 | 1,587 | **0.50** |
| 3B | 7,679 | 3,882 | **0.51** |

- **比が 3B と 54KB で同一**（0.50 / 0.51）なので、**帯域・ボディコピーが原因ではない**
  （3B にはコピーすべきボディがほぼ無い）。逆プロキシで約半減するのは
  HTTP トランザクションが 1 回 → 2 回になることの当然の帰結である。
- ホスト全体の CPU 飽和でもない（計測時 2.6 / 4.0 コア。内訳はクライアント 50% /
  プロキシ 128% / バックエンド 82%）。

**したがって「三重メモリコピーが支配的要因」という仕様書の主張は成立しない。**
残る真の差は「veil の静的:プロキシ比 0.26 に対し nginx は 0.57」という点であり、
対象はボディコピーではなく**バックエンド脚のリクエスト単価**である
（プール取得/返却、リクエストシリアライズ、レスポンスパース、タスク往復回数）。
なお `h2_proxy_http` の `addr.to_string()`・`h2_proxy_https` の `format!` プールキー・
`compute_upstream_path` の所有 `String` は**ホットパス規則違反として実在する**が、
malloc は ~50ns 程度であり **110µs 規模の差を説明しない**（直す価値はあるが
スループット改善を期待して直してはならない）。

生データは [`results_raw.tsv`](results_raw.tsv) 末尾の
`# ==== 2026-08-18 F-158 調査: nginx をプロキシにした静的:プロキシ比の実測 ...` 節。
