# HTTP/3・gRPC・WebSocket 拡張計測 + F-106/F-111 最適化

`tools/perf` の HTTP/3(QUIC)・gRPC・WebSocket 専用クライアント計測（F-89 第 3 弾）と、
その後の gRPC 中継最適化（F-106: H2C バックエンド接続プーリング）・HTTP/3 送信経路の
ゼロコピー化（F-111）をまとめた記録。static 配信中心の [`results_summary.md`](results_summary.md)
（HTTP/1.1・HTTP/2 の完全直交表）を補完する。

## 1. 計測構成

| プロトコル | 構成 | クライアント | 上流 |
|-----------|------|--------------|------|
| HTTP/3 (QUIC) | `h2_1_feat_http3` | QUIC 対応 h2load（`local/h2load-h3`, `--alpn-list=h3`） | なし（静的 File） |
| gRPC | `h2_1_feat_grpc` | grafana k6 gRPC（`k6/grpc.js`） | `moul/grpcbin`(h2c) |
| WebSocket | `h2_1_feat_websocket` | grafana k6 WS（`k6/websocket.js`） | `jmalloc/echo-server` |

ベースは HTTP/2 有効・kTLS 無効・kernel LB（feat 系共通。kTLS はコンテナ環境と相性が悪いため既定オフ）。
`h2_1_feat_http3` は `http2_enabled=true` も併設するため、同一構成で HTTP/1.1・HTTP/2・HTTP/3 を
直接比較できる。負荷: HTTP/1.1=wrk、HTTP/2/3=h2load(`-n30000 -c100 -m10`)、gRPC/WS=k6(VUS50, 10s)。

## 2. 計測結果の推移

### 2.1 初回計測（2026-07 上旬、F-89 第 3 弾。co-tenant 負荷下・4 コア・参考値）

| Target | Config | Proto | Req/s (median±stdev) | Lat Avg | Errors |
|--------|--------|-------|----------------------|---------|--------|
| nginx | base | http1.1 | 1215.4 ± 31.2 | 74.62ms | 0 |
| nginx | base | http2 | 1383.5 ± 3.4 | 584.51ms | 0 |
| veil_glibc | http3 構成 | http1.1 | 1585.4 ± 46.9 | 59.42ms | 0 |
| veil_glibc | http3 構成 | http2 | **1848.1 ± 5.6** | 320.57ms | 0 |
| veil_glibc | http3 構成 | **http3** | **269.8 ± 2.2** | 227.02ms | 0 |
| veil_glibc | grpc | grpc | 474.0 ± 2.4 | 97.43ms | 0 |
| veil_glibc | websocket | websocket | 578.0 ± 27.4 | 318.92ms | 0 |

同一構成での HTTP/3 は HTTP/2 比 **約 1/7（0.15×）**。gRPC は静的 HTTP/2 比 **約 1/4**
（h2c バックエンドへリクエストごとに接続 + ハンドシェイクしていたのが主因、後述 F-106 で解消）。

**GSO/GRO は Docker veth では効果なし**（A/B で off 404 / on 365 req/s）。既定 `false` が妥当。

### 2.2 F-106（H2C バックエンド接続プーリング）— gRPC 最適化

`handle_http2_proxy_h2c`（gRPC 中継の代表経路）が **リクエストごとに** TCP 3-way + h2c
ハンドシェイク（プリフェース + SETTINGS 往復）を張り捨てていたのが根本原因（B-28 の明示的残件）。
`H2C_POOL`（`src/pool.rs`）で `HTTP_POOL`/`HTTPS_POOL` と同様の checkout/return 方式による
接続再利用を実装。再利用で顕在化した送信ウィンドウ枯渇バグ（`WINDOW_UPDATE` 未反映、
約 2600 リクエストで `Send window exhausted`）も併せて修正した。

**同一セッション A/B**（静穏窓、`veil:glibc-base` 改修前 vs `veil:glibc-f106` 改修後、
k6 gRPC VUS=50×10s×3 反復）:

| 反復 | base（改修前）Req/s | Lat | f106（改修後）Req/s | Lat |
|------|--------------------:|-----|--------------------:|-----|
| 1 | 849.5 | 55.7ms | 1470.9 | 31.7ms |
| 2 | 642.4 | 74.9ms | 1500.1 | 31.6ms |
| 3 | 274.5 | 177.0ms | 1506.2 | 31.1ms |
| **median** | **642.4** | **74.9ms** | **1500.1** | **31.6ms** |

**+133%（約 2.3 倍）、レイテンシ約 2.4 倍改善、fails=0**。改修前は 274〜850 req/s と負荷に
敏感で激しくばらつくが、改修後は 1471〜1506 req/s で極めて安定（毎リクエストのハンドシェイクが
消え、1 コールの仕事がフレーミング + 中継のみに縮小するため）。

### 2.3 F-111（perf 回帰確認 + HTTP/3 GSO ゼロコピー化）

現行 HEAD で `veil:glibc`/`veil:musl`（full）を再ビルドし、静穏ホスト（loadavg<0.2）で再計測。

| Target | Config | Proto | Req/s (median) | Lat Avg | Errors |
|---|---|---|---|---|---|
| nginx | base | http1.1 | 2362.9 | 40.62ms | 0 |
| nginx | base | http2 | 2471.6 | 200.64ms | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | **3275.0** | 29.38ms | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http2 | **2696.6** | 183.43ms | 0 |
| veil_glibc | h2_1_feat_grpc | grpc | **1470.4** | 32.04ms | 0 |
| veil_glibc | h2_1_feat_http3 | http1.1 | 2767.7 | 35.53ms | 0 |
| veil_glibc | h2_1_feat_http3 | http2 | 2695.7 | 188.36ms | 0 |
| veil_glibc | h2_1_feat_http3 | http3 | **424.7** | 128.33ms | 0 |
| veil_musl | 同上3構成 | (同傾向) | (同傾向) | - | 0 |

- **HTTP/1.1・HTTP/2 は回帰なし**（nginx比 +38.6%/+9.1%、既存水準を維持）。
- **gRPC（F-106）は 1470.4 req/s** で退行なし（§2.2 の改修後値と一致）。
- **HTTP/3 は 424.7 req/s（HTTP/2 比 15.8%）**。§2.1 の 15.0% とほぼ同一比率で、構造的な
  速度差に変化なし。

#### HTTP/3 再調査（否定した仮説）

previous 結論（userspace QUIC の構造的コスト）を鵜呑みにせず、以下を個別に再検証した
（いずれも主因ではなかった）:

1. **QUIC ストリーム/フロー制御上限**: `initial_max_streams_bidi`（既定100）・
   `initial_max_data` 等は h2load `-c100 -m10` に対して十分に大きく、律速していない。
2. **quiche のパケットペーシング**: `quiche::Config` は既定 `pacing: true` だが、これは
   `SendInfo.at` という**アドバイザリヒント**に過ぎず、`SO_TXTIME` 等で明示的に遅延送信
   しない限り実送信は遅延されない。本実装は `at` を見ず即座に送出しているため無関係。
3. **ワーカー分散**: HTTP/3 も `bind_reuseport` で全ワーカーが UDP ソケットを共有し、
   カーネルが 5-tuple ハッシュで分散している（単一スレッド集中ではない）。

#### 改修: GSO バッチ送信のゼロコピー化

`send_pending_packets` は複数パケットをスレッドローカルの `batch: Vec<u8>` へ連続追記し
`offsets` で境界を記録する設計だったが、`flush_gso_batch` が毎回そこから `Vec<&[u8]>` を
`collect` し、さらに送信関数内部の `combine_packets` が**同じデータを新規 `Vec<u8>` へ
再結合**していた。`batch` は元々連結済みのため、この再結合は完全に無駄な二重アロケーション
+ 二重コピーだった（ホットパス絶対規則違反）。`send_gso_combined_async`
（`src/udp/socket.rs`）を新設し、`batch` をそのまま `sendmsg(UDP_SEGMENT)` へ渡すよう変更
（GSO 無効時は `offsets` 境界通りに個別送信、追加確保なし）。計測は誤差範囲（静的配信は
1req1packetが大半でこの経路をあまり使わない）だが、ゼロコピー原則への準拠として正当な修正。

## 3. 結論

- HTTP/1.1・HTTP/2・gRPC（F-106）は複数セッションを通じて**回帰なし**。
- gRPC は H2C 接続プーリング（F-106）で **+133%** の改善を達成し、静的 HTTP/2 比 55% 前後まで
  引き上げた。
- HTTP/3 は quiche（userspace QUIC）の per-packet syscall コストという**構造的な制約**であり、
  既存の最適化（GSO/GRO バッチング・ゼロコピーアクターモデル・EMSGSIZE 修正・F-111 の
  送信経路ゼロコピー化）を尽くしても HTTP/1.1・HTTP/2 と同等のスループットには到達できない。
  ストリーム上限・quiche パケットペーシング・ワーカー分散はいずれも主因ではないことを個別に
  確認済み。今後さらに縮めるには、カーネル側の QUIC オフロードなど環境側の前提変更が必要。

関連チケット: [F-89](../backlog/features/F-89-perf-full-features-coverage.md)、
[F-106](../backlog/features/F-106-h2c-backend-connection-pooling.md)、
[F-111](../backlog/features/F-111-perf-regression-check-http3-gso-zerocopy.md)。
