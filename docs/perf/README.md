# docs/perf — パフォーマンス計測サマリ

Veil の HTTP/1.1・HTTP/2・HTTP/3・gRPC・L4 スループット／レイテンシ／CPU・メモリ使用量を、
`nginx:alpine` を基準に **同一 Docker ネットワーク上のコンテナ間通信** で計測した結果のサマリ。

- 計測ハーネス: [`tools/perf/`](../../tools/perf/)（`gen_configs.sh` で構成生成 /
  `run_perf.sh` で反復計測 / `analyze_results.sh` で median±stdev 集計）。
  実行すると `tools/perf/results/results_raw.tsv`（1 反復 1 行の生データ）と
  `results_summary.md`（集計）が生成される（いずれも git 管理外の作業成果物）。
- **本ディレクトリの [`results_raw.tsv`](results_raw.tsv)** は最新計測
  （2026-07-16、v0.5.0 向けフルスイート）の `tools/perf` 生データのコミット済みコピー。
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

## 再現手順

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
