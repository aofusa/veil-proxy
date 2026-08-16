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
