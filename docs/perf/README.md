# docs/perf — パフォーマンス計測サマリ

Veil の HTTP/1.1・HTTP/2・HTTP/3・gRPC・WebSocket・L4 のスループットを、nginx を基準に計測した
**最新の結果だけ**をまとめる（過去の計測は git 履歴で辿れる）。

| ファイル | 内容 |
|---|---|
| [`results_raw.tsv`](results_raw.tsv) | Linux x86_64 のフルスイート生データ（1 反復 1 行）+ B-99 の再計測 |
| [`results_summary.md`](results_summary.md) | 上記の median±stdev 集計（`tools/perf/analyze_results.sh` の出力） |
| [`freebsd_results_raw.tsv`](freebsd_results_raw.tsv) | FreeBSD 14.3 aarch64 ネイティブ計測の生データ |
| [`platform_verification.md`](platform_verification.md) | v0.7.0 の全プラットフォーム検証（単体・統合・E2E） |

---

## Linux x86_64（2026-10-07、Docker コンテナ間）

- コミット: `3746594`（v0.7.0 リリース候補）。B-99（HTTP/1.1 プロキシ圧縮）だけ `d8126b6` で再計測
- イメージ: `veil:glibc`（`full`、io_uring）/ `veil:musl`（`full`、io_uring）。比較対象は `nginx:alpine`
- **810 計測すべてで Non-2xx = 0、NA 行ゼロ**
- 実行: `bash tools/perf/gen_configs.sh && bash tools/perf/run_perf.sh`（69 構成 × 3 反復）

### 代表構成（Req/s 中央値）

| 構成 | プロトコル | nginx | veil glibc | veil musl | 対 nginx（glibc） |
|---|---|---|---|---|---|
| `h2c_file`（3B 静的） | h2c | 10,080 | **24,135** | 23,811 | **2.39×** |
| `h2c_proxy`（54KB 中継） | h2c | 6,188 | **10,858** | 10,519 | **1.75×** |
| `h2_1_ktls_0_lb_kernel_ofc_1` | HTTP/1.1 | 6,437 | **9,667** | 9,527 | **1.50×** |
| 同上 | HTTP/2 | 5,932 | **7,243** | 7,166 | **1.22×** |
| `h2_1_ktls_0_lb_cbpf_ofc_1` | HTTP/1.1 | 6,437 | **9,721** | 9,682 | **1.51×** |
| `h2_0_ktls_0_lb_cbpf_ofc_0` | HTTP/1.1 | 6,437 | **8,095** | 8,285 | **1.26×** |
| `h2_0_feat_l4`（L4 平文） | HTTP/1.1 | 6,437 | **13,120** | — | **2.04×** |
| `h2_1_feat_compression_cached` | HTTP/2 | 5,932 | **17,914** | 17,340 | **3.02×** |
| 同上 | HTTP/1.1 | 6,437 | **10,930** | 10,883 | **1.70×** |
| `h2_1_feat_proxy`（54KB 中継） | HTTP/1.1 | 6,437 | 6,279 | 6,342 | 0.98※ |
| 同上 | HTTP/2 | 5,932 | 5,955 | 6,002 | 1.00※ |
| `h3_file_metrics` | HTTP/3 | — | 1,824 | 1,817 | — |
| `h3_proxy` | HTTP/3 | — | 1,583 | 1,578 | — |
| `grpc_h2_metrics` | gRPC | — | 4,356 | 4,320 | — |
| `grpc_h3_metrics` | gRPC over HTTP/3 | — | 7,895 | 8,034 | — |

※ nginx の列は `base` 構成で、**TLS 静的配信**（逆プロキシではない）。したがって `*_proxy*` との比は
「veil の逆プロキシ」対「nginx の静的配信」で、同条件の比較ではない。**プロキシ同士の同条件比較は h2c だけ**
（nginx 側も `/proxy/` で中継する）で、`h2c_proxy` は 1.75×。54KB の中継構成は約 6,400 req/s
（約 2.8 Gbps）で、静的配信の nginx と同じくコンテナ間ネットワークの上限に張り付いている。

### 対 nginx の分布

| プロトコル | veil 構成数 | nginx 以上 | 中央値 |
|---|---|---|---|
| HTTP/1.1 | 54 | 40 | **1.26×** |
| HTTP/2 | 45 | 40 | **1.05×** |

nginx を下回る構成は 2 種類だけ:

1. **54KB 中継の各機能構成**（`h2_1_proxy_*` / `h3_proxy_*` の HTTP/1.1・HTTP/2）: 0.95〜0.99×。
   比較相手は nginx の静的配信（上の ※）で、両者ともネットワーク上限に張り付いた並び。
   veil の中継は上流へのホップ 1 段ぶん仕事が多い。
2. **キャッシュ無効の圧縮構成**（`*_compression`）: 0.3〜0.7×。veil は毎リクエスト zstd で
   圧縮しているのに対し、比較対象の nginx は圧縮していない（ハーネスの nginx に `gzip` 設定は
   無い）ので、同じ仕事の比較ではない。推奨構成（`static_file_cache` + `open_file_cache` で
   圧縮結果をキャッシュする `*_compression_cached`）は **HTTP/2 で 3.02×** である。

### B-99: HTTP/1.1 のプロキシ圧縮（`d8126b6` で再計測）

HTTP/1.1 のプロキシ圧縮 2 経路だけ `zstd::encode_all`（呼び出しごとにコンテキストを確保する
ワンショット API）のままだった。HTTP/2・HTTP/3 と同じスレッドローカルのコンテキスト使い回しに
揃えた。

| 構成 | プロトコル | 修正前 | 修正後 | 比 |
|---|---|---|---|---|
| `h2_1_proxy_compression` | HTTP/1.1 | 1,939 | **2,751** | **1.42×** |
| `h3_proxy_compression` | HTTP/1.1 | 2,015 | **2,739** | **1.36×** |

ほかの圧縮構成（HTTP/2・HTTP/3・静的）は ±3% で変化なし。

### 退行確認（2026-08-27 のフルスイートとの比較）

比較可能な 137 組（`veil_glibc` の構成 × プロトコル）の比の中央値は **0.992**。0.95 を下回った
2 組を単独で再計測した:

| 構成 | プロトコル | 前回 | 今回（フル） | 再計測 | 再計測 / 前回 |
|---|---|---|---|---|---|
| `h3_file_rate_limit` | HTTP/2 | 7,137 | 6,670 | 6,928 | 0.971 |
| `h2_1_ktls_1_lb_cbpf_ofc_0` | HTTP/2 | 6,148 | 5,810 | 5,867 | 0.954 |

どちらもラン間のばらつき（同一イメージで最大 15%、下記）の範囲内。後者はコンテナと相性の悪い
kTLS 有効の構成で、同じ構成の HTTP/1.1 は 6,656（nginx 6,437 の 1.03×）だった。

---

## FreeBSD 14.3 aarch64（QEMU + HVF、ネイティブ）

- コミット: `d8126b6`。`tools/perf/freebsd/run_perf_freebsd.sh -r 3 -d 10`（3B と 54,576B）
- **全 96 計測でエラー 0**

### 対 nginx 比（同一ラウンド内の比、3 ラウンドの中央値）

| シナリオ | 3B | 54,576B |
|---|---|---|
| `h1_file_tls`（HTTPS 静的） | **1.07** | **1.21** |
| `h2_file_tls`（HTTP/2 静的） | **1.29** | **1.39** |
| `h1_file_plain`（平文 HTTP/1.1 静的、sendfile） | **1.09** | **1.01** |
| `h2c_file_plain`（h2c 静的） | **1.40** | 0.72 |
| `h3_file`（HTTP/3 静的） | 0.89 | 0.89 |
| `h1_proxy_tls`（HTTPS 中継） | **1.00** | **1.00** |
| `h2_proxy_tls`（HTTP/2 中継） | **1.59** | **1.47** |
| `l4_tcp`（L4 TCP 中継） | 0.98 | **1.15** |

16 項目中 12 項目で nginx 以上（互角を含む）。下回る 4 項目:

- **`h3_file` 0.89**: 同じバイナリでも対 nginx 比は ±10〜20% 動き、交互 A/B（base / new を 5 ラウンド）では
  0.94〜1.05 だった。2026-10 に HTTP/3 の固定費を順に削った結果（下表）、0.75〜0.86 から並ぶ水準になった
- **`h2c_file_plain` 54KB 0.72**: F-157 からの構造的な差。nginx は h2c でも `sendfile(2)` + `sf_hdtr` で
  カーネル内で完結するが、veil は HTTP/2 の DATA フレームへ再フレーミングするため `sendfile` に載らず、
  本体を書き込みバッファへコピーする。ただしこの構成は両サーバとも CPU を約 50% しか使っておらず
  （DTrace で確認）、スループットはクライアント（h2load 2 スレッド）とループバック側で決まっている。
  コピーを消す iovec 化は F-157 で FreeBSD でも退行を確認済み
- **`l4_tcp` 3B 0.98**: 互角（3 ラウンド 0.85 / 0.98 / 0.98）

### 2026-10 の改善（FreeBSD で計測して直したもの）

| 変更 | syscall / プロファイル | 対 nginx |
|---|---|---|
| F-172: quiche の STREAM フレーム結合 | h3 の `sendto` 1.012 → 0.143/req（nginx 0.219） | h3 3B 0.86 → 0.98 |
| B-94 / B-95: Initial の再送処理・UDP バッファ | ハンドシェイクのタイムアウト 6〜20 本 → 0、クライアント送信の損失 14% → ほぼ 0 | 30 秒ストールの解消 |
| B-96: `path = "/"` のルートキャッシュ | 毎リクエストのフル探索（lossy 変換・LRU put）が消える | h3 0.89 → 1.05、h1 TLS 0.94 → 1.02（交互 A/B） |
| F-173: L4 の空打ち read | `read` 6.0 → 4.0/req、合計 8.23 → 6.26/req（nginx 8.12） | — |
| HTTP/1.1 の syscall 削減（writev・sf_hdtr・TCP_NOPUSH・fd 共有） | `h1_file_plain` 2.04/req（nginx 4.22） | — |


---

## 計測条件

### Linux（`tools/perf/`）

- 4 コアのホストでクライアント・veil・上流・nginx を同時に動かす（コンテナ間通信）。veil 単体の
  改善は rps に鈍く出る。CPU/req を見たいときは `tools/perf/h2c_proxy_lab.sh cpuab`
- 負荷: HTTP/1.1 = wrk `-t4 -c100 -d10s` / HTTP/2・HTTP/3 = h2load `-n 30000 -c100 -m10` /
  gRPC = k6 50VU × 10s / gRPC over HTTP/3 = QUIC 対応 h2load / WebSocket = k6 / L4 = wrk
- 圧縮構成のみ `Accept-Encoding: gzip, br, zstd` を付ける
- 各 (構成, プロトコル) をウォームアップ後 3 反復し中央値をとる。Errors は Non-2xx
- kTLS はコンテナ（veth）と相性が悪いため、`feat_*` 構成では無効（直交表の ktls 因子でだけ有効）

### FreeBSD（`tools/perf/freebsd/`）

- FreeBSD 14.3 aarch64 ゲスト（4 vCPU、Apple Silicon の QEMU + HVF）。veil・nginx を CPU 0-1、
  負荷生成を CPU 2-3 に `cpuset` で固定
- HTTP/1.1 = wrk `-t2 -c64` / HTTP/2・h2c = h2load `-t2 -c64 -m32` / HTTP/3 = `tools/perf/h3load`
  `-t2 -c64 -m32`（quinn ベース） / L4 = wrk
- 3B（`/small.html`）と 54,576B（`/index.html`）の 2 サイズ、各 3 ラウンド。**判定は同一ラウンド内の
  対 nginx 比**（絶対値はホスト状態で 3 倍振れる）
- veil の software kTLS は無効（F-155）。双方 `open_file_cache` 有効、veil は `static_file_cache` も有効

---

## 計測上の注意（実測で踏んだもの）

- **同一イメージでもラン間で 15% 振れる構成がある。** ラン内の 3 反復が 1% 未満でも、独立ランの
  中央値は振れる（`h2_1_proxy_compression` HTTP/1.1 で 1,684〜1,930）。退行判定は「再計測して
  戻るか」「ビルド間で方向が揃うか」で行う
- **FreeBSD の計測 VM はホスト（macOS の常駐プロセス）の状態で絶対値が 3 倍振れる。** 同じバイナリの
  交互 A/B でも対 nginx 比が ±10〜20% 動く。数ラウンドの差を根拠に結論を出さない
- **負荷ツール側のソケットバッファにも注意。** BSD の UDP 既定（42KB）のままだとクライアントが
  応答バーストを取りこぼし、サーバの損失回復（PTO の指数バックオフ）として計測に乗る
  （1 接続が 30 秒止まる）。`h3load` は送受信バッファを広げている（B-95）
- **計測構成がその機能の有効化条件を満たしているか先に確認する。** 圧縮結果キャッシュは
  `static_file_cache` が、静的配信の offload ゼロ経路は `open_file_cache` が要る（F-157 / F-169）
- **「0 rps・NA を記録して正常終了する」失敗モードがある。** 新しい環境ではまず 0 / NA / errors の
  行が無いかを見る（B-80 では平文 HTTP/1.1 が 0 rps のまま気づかれなかった）
- **計測中に同じホストでビルドしない。** 並行ビルドで E2E の失敗数や rps が大きく変わる
- **手元の `docker run` で再現するときはハーネスと同じマウント先（`/etc/veil/conf.d/config.toml`）を
  使う。** snap 版 docker はリポジトリ外のパスを黙って空ディレクトリとしてマウントする

---

## 再現手順

### Linux（Docker）

```bash
docker build -f docker/Dockerfile.glibc -t veil:glibc .
docker build -f docker/Dockerfile.musl  -t veil:musl  .
docker build -t local/h2load-h3:latest tools/perf/h2load-http3   # HTTP/3 クライアント

bash tools/perf/gen_configs.sh
bash tools/perf/run_perf.sh                                       # 全構成（約 9 時間）
BUILDS=glibc CONFIG_GLOB='*compression' bash tools/perf/run_perf.sh   # 一部だけ
bash tools/perf/analyze_results.sh tools/perf/results/results_raw.tsv
```

### FreeBSD（QEMU VM）

```bash
tools/qemu/bsd-vm.sh freebsd aarch64 up
tools/qemu/bsd-vm.sh freebsd aarch64 build            # full-freebsd で release ビルド
# ゲスト内:
pkg install -y nginx nghttp2 wrk-luajit
cargo build --release --manifest-path tools/perf/h3load/Cargo.toml
sh tools/perf/freebsd/run_perf_freebsd.sh -r 3 -d 10                  # 54KB
sh tools/perf/freebsd/run_perf_freebsd.sh -r 3 -d 10 -p /small.html   # 3B
sh tools/perf/freebsd/syscalls_per_req.sh h3_file /small.html         # syscall/req（DTrace）
sh tools/perf/freebsd/profile_cpu.sh h3_file /small.html              # 関数別 CPU（DTrace）
```
