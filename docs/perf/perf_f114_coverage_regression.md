# F-114/F-115: 網羅計測拡張後の回帰確認 + HTTP/3・gRPC ボトルネック再計測

計測日: 2026-07-12（`perf/http3-grpc-optimization` ブランチ HEAD、full features
`veil:glibc` を現行ツリーから再ビルド）。ホスト静穏ウィンドウ（計測開始時 1 分 loadavg 1.1）。
`tools/perf`（F-114 で拡張したハーネス）を `CONFIG_GLOB` で scoped 実行し、各 (config, proto)
を 3 反復・median 集計。

## 目的

1. F-114 の網羅マトリクス拡張後も **HTTP/1.1・HTTP/2 に回帰がない**ことを実測で確認。
2. **HTTP/3・gRPC** の現状ボトルネックを再計測し、[[F-111]] の構造的結論を再検証。
3. 新規 `h3_proxy`（HTTP/3 逆プロキシ）構成の挙動を確認。

## 計測結果（veil_glibc, median±stdev）

| Config | Proto | Req/s | Lat Avg | CPU% | Mem MB | Errors |
|---|---|---|---|---|---|---|
| nginx base | http1.1 | 2183.4 ± 132.2 | 43.80ms | 207.5 | 21.9 | 0 |
| nginx base | http2 | 2464.1 ± 52.7 | 200.59ms | 170.1 | 26.1 | 0 |
| **h2_1_ktls_0_lb_kernel_ofc_1** | http1.1 | **3296.6 ± 29.6** | 29.55ms | 207.8 | 62.2 | 0 |
| **h2_1_ktls_0_lb_kernel_ofc_1** | http2 | **2659.1 ± 28.8** | 188.30ms | 127.3 | 105.5 | 0 |
| h2_1_feat_http3 | http1.1 | 2781.1 ± 7.8 | 35.01ms | 222.7 | 94.8 | 0 |
| h2_1_feat_http3 | http2 | 2686.8 ± 16.1 | 189.07ms | 129.3 | 141.1 | 0 |
| **h2_1_feat_http3** | **http3** | **429.7 ± 0.9** | 125.77ms | 168.1 | 174.0 | 0 |
| **h2_1_feat_grpc** | **grpc** | **1488.1 ± 31.6** | 31.88ms | 127.6 | 81.1 | 0 |
| h3_proxy（新） | http1.1 | 2033.0 ± 14.3 | 48.03ms | 173.9 | 127.8 | 0 |
| h3_proxy（新） | http2 | 2170.9 ± 25.6 | 241.32ms | 115.9 | 160.2 | 0 |
| h3_proxy（新） | http3 | 585.5 ± 132.2 | 1.48s | 181.7 | 286.3 | **444** |

veil_musl も同傾向（best: http1.1 3250 / http2 2680、http3 file 431、grpc 1450、
h3_proxy http3 ~630 でエラーは概ね 0）。

## 結論

### 1. HTTP/1.1・HTTP/2 は回帰なし（✓）

best 構成 `h2_1_ktls_0_lb_kernel_ofc_1` で HTTP/1.1 **3296.6**（[[F-111]] 3275.0）・
HTTP/2 **2659.1**（F-111 2696.6）。いずれも F-111 と誤差範囲で一致し、nginx 比
HTTP/1.1 +51%・HTTP/2 +8%。全構成 Non-2xx=0。F-73/F-74/F-59/F-106 の最適化は維持されている。

### 2. gRPC は退行なし（✓）

`h2_1_feat_grpc` で **1488.1 req/s**（F-111 1470.4）。HTTP/2 静的配信比 55%。F-106（H2C プーリング）
の水準を維持。CPU は 128%（4 コア中 32% 相当）で **CPU バウンドではなく**、上流 h2c ホップの
往復レイテンシ（32ms）が律速。既に接続再利用済みで、これ以上は上流(grpcbin)側 or ネットワーク
ホップの性質による。

### 3. HTTP/3 は構造的制約を再確認（F-111 追認）＋新知見

`h2_1_feat_http3` で **429.7 req/s**（F-111 424.7）。HTTP/2 比 16.0%。3 反復の stdev 0.9 と
極めて安定、Non-2xx=0。**新知見**として:

- **CPU 使用率は 168%（4 コア中 42%）に留まる**。HTTP/3 ワーカーは **CPU バウンドではなく**、
  1 データグラム 1 syscall（`recvmsg`/`sendto`）の readiness 往復で待機している。Docker veth は
  カーネル GSO/GRO オフロード非対応のため（[[project_perf_http3_grpc_ws]]）、同一フロー集約も
  効かない。→ **アイドル CPU の余地があり**、syscall バッチング（`recvmmsg`/`sendmmsg`,
  [[F-115]]）でスループットを引き上げる余地が理論上ある。
- 静的ローカルファイルで **平均 125ms のレイテンシ**は RTT（veth はサブ ms）由来ではなく、
  サーバ主ループのデータグラム逐次処理 or h2load(ngtcp2) クライアント側の直列化に起因する。
  正確な内訳の切り分けには計装が要る（[[F-115]] の調査項目へ反映）。

pacing（`send_info.at`）は本実装が参照せず即時送出しているため、F-111 の通り人為的スロットルは
無い（`send_pending_packets`）。ストリーム/フロー制御上限にも達していない。

### 4. 新規 `h3_proxy`（HTTP/3 逆プロキシ）の不安定性（要調査）

HTTP/3 で受けて HTTP/1.1 バックエンド（perf-backend）へ中継する `h3_proxy` 構成は、
glibc で **反復間のばらつきが大きく（396/585/651 req/s）エラーが混入**（17/2/425）、
レイテンシ 1.48s と劣化。musl は概ね安定（0〜6 エラー）。QUIC 高並行 × バックエンドホップ下での
接続プール/フロー制御の相互作用が疑われる。**新規バグチケット [B-42](../backlog/bugs/B-42-http3-proxy-load-instability.md) を起票**して継続調査とする
（HTTP/3 File 経路は 0 エラーで安定のため、本件は Proxy ホップ特有）。

## HTTP/3 改善（F-115 第1段: 受信 drain バッチ）

診断計測で HTTP/3 は **並行度・クライアントスレッドを変えてもスループット横ばい**
（h2load `-c25〜-c200` / `-t1/-t2` で ~340〜465 req/s）で、**ワーカーあたりの固定処理レートに
律速**（CPU 余地あり）と判明。主ループ `select_biased!` が毎イテレーション（= 毎データグラム）
負け arm の `sleep(timeout)` で io_uring タイマー SQE を arm→cancel していた点に着目し、
**1 回の readiness あたり複数データグラムを非ブロッキングで drain**（`recv_with_gro_sync` で
EAGAIN まで、上限 `H3_RECV_DRAIN_MAX=64`）して select/タイマー往復を償却する第1段を実装
（`src/http3_server.rs`、追加確保なし・既存ゼロコピー経路流用）。

| | Req/s (glibc, -c100 -m10) | Lat | Errors |
|---|---|---|---|
| 変更前（baseline） | 429.7 ± 0.9 | 125.8ms | 0 |
| **F-115 drain 適用後** | **441.5**（440.8/442.5/441.2） | 121.8ms | 0 |

→ **+2.6%・レイテンシ −3%・エラー 0・3 反復とも安定**。小幅だが一貫した改善で、
per-datagram の select/タイマー往復が律速の一部だったことを実証。ただし主因は依然
**1 データグラム 1 recvmsg/sendto の syscall コスト**であり、これを削減するのは真の
`recvmmsg`/`sendmmsg`（1 syscall で複数データグラム）＝ [[F-115]] 第2段。第1段は
HTTP/3 E2E（`test_*http3*` 全通過）で回帰なしを確認済み。

## 改善方針の判断

- **HTTP/1.1・HTTP/2・gRPC**: 既に最適化水準を維持しており、本計測でボトルネックは
  「TLS 終端 + プロトコル本来のコスト」で、退行はない。追加の即効改善余地は小さい。
- **HTTP/3**: 唯一の未着手レバーは `recvmmsg`/`sendmmsg` によるクロスコネクション syscall
  バッチング（[[F-115]]）。本計測で **CPU 余地の存在（42% 利用）を確認**したことで着手根拠は
  強まったが、QUIC 最ホットパス（cancel 安全性・RefCell 借用・接続振り分け・ハンドシェイク
  送信タイミング）へ及ぶ高リスク改修であり、専有静穏環境での rebuild + 再計測 + HTTP/3 E2E
  全通過による検証が前提。**投機的な未検証改修を本データプレーンへ入れるのは避け**、[[F-115]]
  に本計測の実測知見（CPU 42%・lat 125ms・h3_proxy 不安定）を反映し、独立タスクとして
  段階検証しながら実装する方針とする。

## 再現手順

```bash
docker build -f docker/Dockerfile.glibc -t veil:glibc --build-arg CARGO_FEATURES='full' .
bash tools/perf/gen_configs.sh
CONFIG_GLOB='h2_1_ktls_0_lb_kernel_ofc_1' bash tools/perf/run_perf.sh   # 回帰
CONFIG_GLOB='h2_1_feat_http3'             bash tools/perf/run_perf.sh   # H3 file
CONFIG_GLOB='h2_1_feat_grpc'              bash tools/perf/run_perf.sh   # gRPC
CONFIG_GLOB='h3_proxy'                    bash tools/perf/run_perf.sh   # H3 proxy（新）
```
</content>
