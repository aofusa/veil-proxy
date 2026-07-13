# docs/perf — パフォーマンス計測サマリ

Veil の HTTP/1.1・HTTP/2・HTTP/3・gRPC スループット／レイテンシ／CPU・メモリ使用量を、
`nginx:alpine` を基準に **同一 Docker ネットワーク上のコンテナ間通信** で計測した結果のサマリ。

- 計測ハーネス: [`tools/perf/`](../../tools/perf/)（`gen_configs.sh` で構成生成 /
  `run_perf.sh` で反復計測 / `analyze_results.sh` で median±stdev 集計）。
  実行すると `tools/perf/results/results_raw.tsv`（1 反復 1 行の生データ）と
  `results_summary.md`（集計）が生成される（いずれも git 管理外の作業成果物）。
- **本ディレクトリの [`results_raw.tsv`](results_raw.tsv)** は最新計測
  （2026-07-13、下記「最新結果」）の `tools/perf` 生データのコミット済みコピー。
  `bash tools/perf/analyze_results.sh docs/perf/results_raw.tsv` で下表を再集計できる。

## 計測条件（最新計測）

- ホスト: 4 コア Linux（co-tenant あり。1 分 loadavg < 1.2 の静穏ウィンドウで実施）
- イメージ: `veil:glibc` / `veil:musl`（full features、`--build-arg CARGO_FEATURES='full'`）、
  比較対象 `nginx:alpine`（`access_log off`）
- 負荷: HTTP/1.1 = wrk `-t4 -c100 -d10s` / HTTP/2・HTTP/3 = h2load `-n 30000 -c100 -m10`
  （HTTP/3 は QUIC 対応 h2load、ALPN=h3）/ gRPC = k6 50VU×10s → grpcbin(h2c) 中継
- 各 (config, proto) を warmup 後 3 反復、median±stdev 集計。Errors は Non-2xx
- kTLS はコンテナ（veth）と相性が悪いため計測構成では無効（直交表の ktls 因子でのみ計測）

## 最新結果（2026-07-13、median ± stdev）

| Target | Config | Proto | Req/s | Lat Avg | CPU% | Mem MB | Errors |
|---|---|---|---|---|---|---|---|
| nginx | base | http1.1 | 2348.1 ± 19.9 | 40.91ms | 223.2 | 21.9 | 0 |
| nginx | base | http2 | 2446.5 ± 13.1 | 203.90ms | 171.5 | 25.9 | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | **3298.3 ± 11.3** | 29.35ms | 199.2 | 91.2 | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http2 | **2704.3 ± 22.7** | 186.17ms | 125.4 | 137.4 | 0 |
| veil_glibc | h2_1_feat_http3 | http3 | **853.3 ± 3.8** | 1.12ms | 174.7 | 252.6 | 0 |
| veil_glibc | h2_1_feat_grpc | grpc | **1475.2 ± 32.9** | 31.51ms | 95.8 | 85.1 | 0 |
| veil_glibc | h3_proxy | http3 | 671.2 ± 3.3 | 1.43ms | 160.8 | 316.6 | 0 |
| veil_musl | h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | 3249.8 ± 4.8 | 29.95ms | 198.0 | 93.0 | 0 |
| veil_musl | h2_1_feat_http3 | http3 | 832.6 ± 2.8 | 1.14ms | 178.9 | 271.9 | 0 |
| veil_musl | h2_1_feat_grpc | grpc | 1456.5 ± 15.8 | 32.57ms | 132.0 | 85.0 | 0 |

（全行は [`results_raw.tsv`](results_raw.tsv) を参照。h2_1_feat_http3 の http1.1/http2、
h3_proxy の http1.1/http2、musl の残り行も同 tsv に含まれる）

**要点:**

- **HTTP/1.1: nginx 比 1.40 倍、HTTP/2: 1.11 倍**。ボトルネックは TLS 終端
  （平文 L4 素通しは TLS 経由の最大 2.2 倍）で、L7 機能ロジック
  （wasm/metrics/access-log/rate-limit/admin/otel）はノイズ範囲内（±5%）。
- **HTTP/3: 853 req/s（HTTP/2 比 32%）**。F-115 mmsg バッチング + B-43 修正で
  従来の 421 req/s から **+103%（2 倍化）**（下記）。残るギャップはユーザ空間 QUIC の
  per-request CPU コスト（暗号化 + パケット化。カーネル TLS/TCP との構造差）。
- **gRPC 中継: 1475 req/s / 31.5ms**。k6(50VU)→grpcbin **直行**の対照計測でも
  1452 req/s / ~34ms であり、レイテンシはクライアント + 上流のコスト。
  **veil のプロキシホップのオーバーヘッドは実質ゼロ**。
- 全構成 Non-2xx=0・h2load failed=0。

## HTTP/3 2 倍化の内訳（2026-07-13、F-115 第2段 + B-43）

| 施策 | Req/s | 変化 |
|---|---|---|
| 変更前（F-115 第1段まで） | 421.2 ± 1.7 | — |
| + F-115 第2段（recvmmsg/sendmmsg バッチング）+ seccomp 追補 | 443.2 ± 2.7 | +5.2% |
| + **B-43 修正**（StreamBlocked → FrameUnexpected スタック） | **853.3 ± 3.8** | **+103%（累計）** |

- **B-43（主因）**: 静的応答の HEADERS 送出が `StreamBlocked`（新規接続の輻輳ウィンドウ
  ~12KB に対し 53KB 応答が先行ストリームを占有）になると、ヘッダ未送出のままボディだけ
  保留し、再送が `send_body` 先行で `FrameUnexpected` → ストリーム永久スタック →
  クライアントが max_idle_timeout(30s) 待ち。h2load `-n10 -c1 -m10` で 10 本中 9 本が
  失敗する再現手順で特定。**この失敗は h2load の `failed`（ストリームエラー）であり
  Non-2xx に計上されない**ため、Errors 列だけでは長期間検出できなかった。
  → 従来の「per-packet syscall が構造的主因」（F-111）という結論は副次要因だったと判明。
- **F-115 第2段**: 受信は `recvmmsg(2)`（1 syscall 最大 16 データグラム、異なるフローも
  同時）、送信は sweep 全体を `sendmmsg(2)` 一括送出。スクラッチは Box 固定アドレスの
  再利用で per-sweep ヒープ確保なし、EAGAIN は POLL_ADD 待機のみ（新規 io_uring
  オペコードなし）。**Docker seccomp 許可リスト（`docker/assets/security/seccomp.json`）に
  `recvmmsg`/`sendmmsg` が必要**（defaultAction=ERRNO のため欠けると HTTP/3 が無音で全滅。
  glibc で顕在化、musl は libc 内フォールバックで動作してしまう）。
- h3_proxy（HTTP/3 逆プロキシ）の高並行時エラー混入（B-42）も本修正後は glibc/musl とも
  0 件に安定化（チケットは継続観察）。
- 関連チケット: [F-115](../backlog/features/F-115-http3-recvmmsg-sendmmsg-batching.md) /
  [B-43](../backlog/bugs/B-43-http3-static-streamblocked-frameunexpected.md) /
  [B-42](../backlog/bugs/B-42-http3-proxy-load-instability.md)

## 計測履歴（時系列サマリ）

1. **初期ベンチマーク**: kTLS 無効時に nginx 超えを最初に確認。glibc ≧ musl、
   mimalloc 有効が高速という傾向。
2. **B-13/B-14/B-15**（seccomp `faccessat2` 未許可・cache 無効時のファイル解決スタブ）:
   修正前計測は HTTP/1.1 Non-2xx 100% 等の異常値。修正後の再計測で全 24 計測 Non-2xx=0。
3. **kTLS はコンテナで不利**（+36〜70% 無効時が高速）、`reuseport_balancing=kernel` は
   単一クライアント IP 負荷で有利（実運用の多数クライアント IP では cbpf でも分散する）。
4. **F-73/F-74 HTTP/2 送信最適化**: per-frame の二重確保 + 二重コピー排除で HTTP/2 +11.6%、
   フレーム連結（HEADERS/DATA コアレッシング）で送信 syscall 削減。
5. **完全直交表（2⁴=16: http2×ktls×LB×open_file_cache）+ full features ショーケース**を
   整備。full features 有効化そのものによる低下なし。nginx 比最大 1.6 倍の構成を確認。
6. **F-89 機能単位オーバーヘッド計測**: TLS 終端が支配的コスト、L7 機能はノイズ範囲内。
   第 3 弾で http3/grpc/websocket の専用クライアント計測を追加。
7. **F-106 H2C プーリング / F-111 GSO 送信ゼロコピー**: gRPC 中継の接続再利用と HTTP/3
   送信経路の確保排除。F-111 時点で HTTP/3 425 req/s（当時は syscall 律速と結論）。
8. **F-114 全プロトコル×全機能マトリクス**: `h2_1_proxy_*`/`h3_file_*`/`h3_proxy*`/
   `grpc_h2_*`/`grpc_h3*` を追加（65+ 構成、`CONFIG_GLOB` で scoped 計測）。
9. **F-115 第1段（受信 drain バッチ）**: select/タイマー往復の償却で +2.6%。
10. **F-115 第2段 + B-43（2026-07-13、最新）**: HTTP/3 853 req/s へ倍増（上記）。

## 教訓（計測方針に反映済み）

- **コンテナ（veth/bridge）では kTLS が不利**。feat 系構成は kTLS 既定オフ。
- **ホスト負荷（co-tenant のビルド等）が計測ノイズの支配的要因**。静穏ウィンドウ
  （1 分 loadavg 目安 < 1.5、理想は < 0.5）を確認してから計測し、
  比較は必ず**同日・同一環境の A/B**（nginx 併走で環境ノイズを正規化）で行う。
- **h2load の `failed`（ストリームエラー）は Non-2xx に計上されない**。Errors=0 でも
  異常低スループット時は h2load の `requests:` 行とサーバ warn ログを確認する（B-43 の教訓）。
- **Docker seccomp 許可リストは使用 syscall の追加に追随させる**。defaultAction=ERRNO の
  ため欠けると機能が無音で全滅する（F-115 recvmmsg/sendmmsg の教訓）。

## 再現手順

```bash
docker build -f docker/Dockerfile.glibc -t veil:glibc --build-arg CARGO_FEATURES='full' .
docker build -f docker/Dockerfile.musl  -t veil:musl  --build-arg CARGO_FEATURES='full' .
docker build -t local/h2load-h3:latest tools/perf/h2load-http3   # HTTP/3 クライアント

bash tools/perf/gen_configs.sh
CONFIG_GLOB='h2_1_feat_http3'             bash tools/perf/run_perf.sh   # HTTP/3 file
CONFIG_GLOB='h2_1_ktls_0_lb_kernel_ofc_1' bash tools/perf/run_perf.sh   # H1/H2 best
CONFIG_GLOB='h2_1_feat_grpc'              bash tools/perf/run_perf.sh   # gRPC
CONFIG_GLOB='h3_proxy'                    bash tools/perf/run_perf.sh   # HTTP/3 proxy
# 全構成スイート: CONFIG_GLOB 省略（65+ 構成、数時間）

# B-43 リグレッション確認（修正前は 9/10 失敗・30 秒待ちになる）
h2load --alpn-list=h3 -n 10 -c 1 -m10 https://<veil>:443/
```
