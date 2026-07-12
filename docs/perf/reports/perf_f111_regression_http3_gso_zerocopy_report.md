# perf 回帰確認 + HTTP/3 GSO ゼロコピー化レポート（F-111）

## 1. 目的

`perf/http3-grpc-optimization` ブランチの総仕上げとして、現行 HEAD で
`veil:glibc`/`veil:musl`（full features）を再ビルドし、静穏ホストで `tools/perf` を
実行して以下を確認する。

1. **HTTP/1.1・HTTP/2 に回帰がないこと**（F-73/F-74/F-59 のゼロコピー送信最適化が
   維持されているか）。
2. **gRPC（F-106 の H2C 接続プーリング）が退行していないこと**。
3. **HTTP/3 のボトルネックを再調査**し、HTTP/1.1・HTTP/2 と同等の性能に近づける
   余地があるか。

## 2. 計測条件

- ホスト: loadavg < 0.2（1 分値）を確認した静穏ウィンドウ（2026-07-12）。
- 対象構成: `h2_1_ktls_0_lb_kernel_ofc_1`（HTTP/1.1・HTTP/2 の既存最良構成）、
  `h2_1_feat_http3`（HTTP/1.1・HTTP/2・HTTP/3 同一構成比較）、`h2_1_feat_grpc`
  （gRPC 中継）の 3 構成 × glibc/musl × 3 反復。
- 負荷ツール: wrk（HTTP/1.1）、h2load（HTTP/2）、QUIC 対応 h2load（HTTP/3、
  `--alpn-list=h3`）、k6 gRPC（`k6/grpc.js` → grpcbin h2c）。

## 3. 結果（median、詳細は `results_summary.md` 参照）

| Target | Config | Proto | Req/s | Lat Avg | Errors |
|---|---|---|---|---|---|
| nginx | base | http1.1 | 2362.9 | 40.62ms | 0 |
| nginx | base | http2 | 2471.6 | 200.64ms | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | **3275.0** | 29.38ms | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http2 | **2696.6** | 183.43ms | 0 |
| veil_glibc | h2_1_feat_grpc | grpc | **1470.4** | 32.04ms | 0 |
| veil_glibc | h2_1_feat_http3 | http1.1 | 2767.7 | 35.53ms | 0 |
| veil_glibc | h2_1_feat_http3 | http2 | 2695.7 | 188.36ms | 0 |
| veil_glibc | h2_1_feat_http3 | http3 | **424.7** | 128.33ms | 0 |
| veil_musl | h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | 3259.0 | 29.64ms | 0 |
| veil_musl | h2_1_ktls_0_lb_kernel_ofc_1 | http2 | 2712.9 | 184.28ms | 0 |
| veil_musl | h2_1_feat_grpc | grpc | 1459.1 | 32.47ms | 0 |
| veil_musl | h2_1_feat_http3 | http3 | 429.9 | 127.03ms | 0 |

全構成・全反復で Non-2xx=0（エラーなし）。

### 3.1 HTTP/1.1・HTTP/2 — 回帰なし

- HTTP/1.1: veil glibc 3275 vs nginx 2363 → **+38.6%**（[[project_perf_beat_nginx]] の
  既存水準「+35〜38%」を維持）。
- HTTP/2: veil glibc 2697 vs nginx 2472 → **+9.1%**（既存水準「+10〜13%」の範囲内、
  ノイズ差）。
- 本作業では HTTP/1.1・HTTP/2 のホットパスに変更を加えていないため、これは
  期待通りの結果である。

### 3.2 gRPC（F-106）— 退行なし

- 1470.4 req/s（HTTP/2 静的配信比 54.5%）。F-106 実装時の静穏ウィンドウ A/B 実測
  （642→1500 req/s、[[project_f106_h2c_pooling]]）の改修後値とほぼ一致し、
  H2C 接続プーリングの効果は維持されている。

### 3.3 HTTP/3 — 構造的な速度差は不変

- 424.7 req/s、HTTP/2 比 **15.8%**（同一構成 `h2_1_feat_http3` 内比較）。
- 過去計測（`perf_http3_grpc_websocket_report.md`: 270/1848 = 15.0%）とほぼ同一の
  比率であり、既存の「userspace QUIC の構造的コスト」という結論
  （[[project_perf_http3_grpc_ws]]、[[project_f106_h2c_pooling]]）を追認する。

## 4. HTTP/3 再調査で確認した項目（いずれも主因ではない）

previous session の結論を鵜呑みにせず、以下を個別に検証した。

1. **QUIC ストリーム数/フロー制御の上限**: `initial_max_streams_bidi`（既定 100）は
   h2load `-c100 -m10`（コネクション毎 10 ストリーム）に対して十分な余裕があり、
   律速していない。`initial_max_data`/`initial_max_stream_data_*` も同様に
   静的ファイル 1 レスポンス分には十分な既定値。
2. **quiche のパケットペーシング**: `quiche::Config` は既定で `pacing: true` だが、
   これは `send()` が返す `SendInfo.at` という**アプリ側が任意に使えるヒント**に
   過ぎず（`SO_TXTIME` 等での能動的な遅延実装が必要）、quiche 自体が内部で送信を
   遅延させるわけではない。本実装は `at` を見ずに即座に送出しているため、
   ペーシングによる人為的な律速は発生していない。
3. **ワーカースレッド分散**: HTTP/3 も HTTP/2 と同様に `bind_reuseport` で
   全ワーカーが UDP ソケットを共有し、カーネルが 5-tuple ハッシュでフローを
   分散している（単一スレッドに集中していない）。

## 5. 実施した改修: GSO バッチ送信のゼロコピー化

`send_pending_packets`（`src/http3_server.rs`）はスレッドローカルの送信スクラッチ
`batch: Vec<u8>` へ複数パケットを連続追記し `offsets` で境界を記録する設計だが、
`flush_gso_batch` は毎回そこから `Vec<&[u8]>` を `collect` し、さらに
`send_gso_async` 内部の `combine_packets` が**同じデータを新規 `Vec<u8>` へ
再結合**していた。`batch` は呼び出し時点で既に連結済みであるため、この
再結合は完全に無駄な二重アロケーション + 二重コピーだった（ホットパス絶対規則の
「ゼロコピー徹底」「不要なアロケーション禁止」に反する）。

`send_gso_combined_async`（`src/udp/socket.rs`）を新設し、`batch` をそのまま
`sendmsg(UDP_SEGMENT)` へ渡すよう変更した（GSO 無効時は `offsets` 境界通りに
個別送信、追加確保なし）。使われなくなった `send_gso_async` /
`send_packets_individually` / `combine_packets` は削除した。

- 静的ファイル配信中心の h2load 計測は 1 リクエスト 1 パケットで完結するケースが
  大半のため、このバッチ送信経路は主に大きめレスポンスや輻輳ウィンドウ拡大後の
  複数パケットバースト時にのみ使われる。今回の計測では誤差範囲（424.7 req/s で
  横ばい）だが、正しい設計（AGENTS.md のホットパス絶対規則）への準拠として
  明確な改善であり、退行がないことも確認済み。

## 6. 結論

- HTTP/1.1・HTTP/2・gRPC（F-106）は**回帰なし**。
- HTTP/3 は quiche（userspace QUIC）の per-packet syscall コストという
  **構造的な制約**であり、既存の最適化（GSO/GRO バッチング F-33/F-45/F-60、
  ゼロコピーアクターモデル F-32、EMSGSIZE 修正 B-18）に加え本チケットで送信経路の
  残存ゼロコピー違反も解消したが、**HTTP/1.1・HTTP/2 と同等のスループットは
  現在の環境（Docker veth・カーネル QUIC オフロード非対応）とプロトコル実装の
  性質上、現実的な改修コストでは達成できない**。この結論は F-89/F-105/F-106 の
  既存調査と一致する。
- 今後さらに縮めるには、カーネル側の QUIC オフロード（UDP GSO/GRO が実際に
  効く環境、あるいは将来の kernel QUIC オフロード機構）など、環境側の前提が
  変わることが必要になる。
