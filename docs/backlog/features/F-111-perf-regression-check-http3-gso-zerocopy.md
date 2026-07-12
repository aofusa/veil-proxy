# F-111: perf 回帰確認 + HTTP/3 GSO 送信のゼロコピー化

## 機能説明

`perf/http3-grpc-optimization` ブランチの総仕上げとして、現行 HEAD で
`veil:glibc`/`veil:musl`（full features）を再ビルドし、静穏ホストで
`tools/perf` を実行して **HTTP/1.1・HTTP/2 に回帰がないこと**（F-73/F-74/F-59 の
最適化が維持されているか）と **gRPC（F-106）・HTTP/3 の現状値**を確認した。
併せて HTTP/3 送信経路（`flush_gso_batch`）に残っていた無駄なアロケーション/
再コピーを修正した。

## 計測結果（静穏ホスト、loadavg<0.2、2026-07-12）

| Target | Config | Proto | Req/s (median) | Lat Avg | Errors |
|---|---|---|---|---|---|
| nginx | base | http1.1 | 2362.9 | 40.62ms | 0 |
| nginx | base | http2 | 2471.6 | 200.64ms | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | 3275.0 | 29.38ms | 0 |
| veil_glibc | h2_1_ktls_0_lb_kernel_ofc_1 | http2 | 2696.6 | 183.43ms | 0 |
| veil_glibc | h2_1_feat_grpc | grpc | 1470.4 | 32.04ms | 0 |
| veil_glibc | h2_1_feat_http3 | http1.1 | 2767.7 | 35.53ms | 0 |
| veil_glibc | h2_1_feat_http3 | http2 | 2695.7 | 188.36ms | 0 |
| veil_glibc | h2_1_feat_http3 | http3 | 424.7 | 128.33ms | 0 |
| veil_musl | 同上 3 構成 | (同傾向) | (同傾向) | - | 0 |

- **HTTP/1.1・HTTP/2 は回帰なし**（best 構成で nginx 比 +38%/+9%、[[project_perf_beat_nginx]] の
  既存水準を維持）。
- **gRPC（F-106）は 1470 req/s** で、静穏ウィンドウでの前回 A/B 実測（642→1500 req/s、
  [[project_f106_h2c_pooling]]）とほぼ一致し、退行なし。HTTP/2 静的配信比 55%。
- **HTTP/3 は 424.7 req/s（HTTP/2 比 15.8%）**。過去計測（`perf_http3_grpc_websocket_report.md`
  の 270/1848=15.0%）とほぼ同一比率で、構造的な速度差に変化なし。

## HTTP/3 の再調査結果

以下を確認したが、いずれもボトルネックの主因ではなかった:

- **QUIC ストリーム/フロー制御上限**: `initial_max_streams_bidi`（既定 100）・
  `initial_max_data`/`initial_max_stream_data_*`（既定 1MB/10MB 級）は
  h2load の `-c100 -m10`（コネクション毎 10 ストリーム）に対して十分に大きく、
  制限には掛かっていない。
- **quiche のパケットペーシング**: `quiche::Config` は既定で `pacing: true`
  だが、これは `SendInfo.at` という**アドバイザリのヒント**に過ぎず、
  アプリケーションが `SO_TXTIME` 等で明示的に遅延送信しない限り実際の送信は
  スロットルされない。本実装は `at` を参照せず `conn.send()` が返した
  パケットを即座に送出しているため、ペーシングによる人為的な律速は無い。
- **SO_REUSEPORT によるワーカー分散**: HTTP/3 も HTTP/2 と同様に
  `bind_reuseport` で全ワーカースレッドが UDP ソケットを共有し、カーネルが
  5-tuple ハッシュでフローを分散している（単一スレッド集中ではない）。

## 改修: GSO バッチ送信の二重確保・二重コピー排除

`send_pending_packets`（メインループの送信処理）は複数パケットを
スレッドローカルの `batch: Vec<u8>` へ連続追記し `offsets` で境界を記録して
いたが、`flush_gso_batch` はそこから **`Vec<&[u8]>` を毎回 `collect`** し、
さらに `send_gso_async` 内部の `combine_packets` が **同じデータを新規
`Vec<u8>` へ再結合**していた。`batch` は元々連結済みであるため、この
再結合は完全に無駄な二重アロケーション + 二重コピーだった。

`send_gso_combined_async`（`src/udp/socket.rs`）を新設し、`batch` を
そのまま `sendmsg(UDP_SEGMENT)` へ渡すよう変更（GSO 無効時のみ `offsets`
境界通りに個別送信、追加確保なし）。使われなくなった
`send_gso_async`/`send_packets_individually`/`combine_packets` は削除した。

- 静的配信中心の h2load 計測では 1 リクエスト 1 パケットで完結するケースが
  大半のため、このバッチ送信は主に大きめレスポンス/輻輳ウィンドウ増大後の
  複数パケットバースト時にのみ使われる。計測上は誤差範囲（424.7 → 424.7 で
  横ばい）だが、ホットパス絶対規則（ゼロコピー徹底・不要アロケーション禁止）
  への準拠として明確な改善であり、退行確認と併せて維持する。

## 結論

- HTTP/1.1・HTTP/2・gRPC（F-106）は**回帰なし**。
- HTTP/3 は quiche（userspace QUIC）の per-packet syscall コストという
  **構造的な制約**であり、既存の GSO/GRO・ゼロコピー・アクターモデル最適化
  （F-33/F-45/F-60/F-32/B-18）に加え本チケットで送信経路の残存ゼロコピー
  違反も解消したが、**HTTP/1.1・HTTP/2 と同等のスループットは環境（Docker
  veth・カーネル QUIC オフロード非対応）とプロトコル実装の性質上、現実的な
  改修コストでは達成できない**（[[project_perf_http3_grpc_ws]]・
  [[project_f106_h2c_pooling]] の既存結論を追認）。今後の抜本改善には
  カーネル QUIC オフロード（`io_uring` の QUIC 対応や `MSG_ZEROCOPY`
  相当の恩恵を UDP で得る）など環境側の前提変更が必要。
