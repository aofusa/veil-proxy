# F-115 第2段 + B-43: HTTP/3 データグラム mmsg バッチングと StreamBlocked バグ修正

計測日: 2026-07-13（`perf/http3-grpc-optimization` ブランチ、full features
`veil:glibc` / `veil:musl` を各時点のツリーから再ビルド）。`tools/perf` を
`CONFIG_GLOB` で scoped 実行し、各 (config, proto) を 3 反復・median 集計。
すべて同日・同一ホスト上で「変更前 → 変更後」を A/B 比較（nginx を環境ノイズの
基準として併走計測。nginx は全時点で http1.1 ≈2330〜2390 / http2 ≈2430〜2480 と安定）。

## 実施内容

1. **F-115 第2段（recvmmsg/sendmmsg バッチング）** — `src/udp/socket.rs` /
   `src/http3_server.rs`
   - 受信: 主ループ drain を `recvmmsg(2)`（MSG_DONTWAIT、最大 16 データグラム/1 syscall、
     異なるフローも同時）へ置換。セグメント処理は `process_datagram_segments` へ抽出。
   - 送信: `send_pending_packets` を per-connection 即時 flush（-c100 で 1 sweep 最大
     ~100 syscall）から、sweep 全体の送信エントリ蓄積 → `sendmmsg(2)` 一括送出
     （16 メッセージ / 256KB 閾値で途中送出、GSO 無効時はパケット境界へ展開）へ変更。
   - mmsghdr/iovec/sockaddr/cmsg は Box 固定アドレスのスクラッチを再利用（per-sweep の
     ヒープ確保なし）。EAGAIN は既存 POLL_ADD 待機のみで、新規 io_uring オペコードなし。
2. **seccomp 許可リスト追補** — `docker/assets/security/seccomp.json` に
   `recvmmsg` / `sendmmsg` を追加（欠けると defaultAction=ERRNO の EPERM で全送信が
   失敗し HTTP/3 が 0 req/s になる。glibc で顕在化、musl は libc 内フォールバックで動作）。
   veil 内蔵 seccomp（`src/security.rs`）は従来から許可済み。
3. **B-43 修正（本計測の最大の発見）** — `src/http3_server.rs`
   静的応答の HEADERS 送出が `StreamBlocked` になると、ヘッダ未送出のままボディだけ
   `partial_responses` に保存し、再送が `send_body` を先に呼んで `FrameUnexpected` →
   ストリーム永久スタック（クライアントは max_idle_timeout=30 秒待ち）になっていた。
   新規接続の輻輳ウィンドウ（~12KB）に対し 53KB 応答が先行ストリームを占有する並行
   リクエストで多発し、h2load `-n10 -c1 -m10` の再現で **10 本中 9 本が失敗**していた。
   `PartialResponse { head: Option<Vec<h3::Header>>, body, written }` へ拡張し、再送
   共通ヘルパー `try_flush_partial` でヘッダ→ボディ順序を一元化して修正。

## 診断の経緯（要点）

- クライアント 2 本同時（各 -c50）でも合計 433 req/s（1 本時と同じ）→ サーバ側律速を確定。
- `-n` を 10→200 に変えても所要が常に ~30 秒 → **アイドルタイムアウト待ちのテール**を発見。
- h2load 詳細出力で `9 failed / 9 errored`、サーバログで `send_body error: FrameUnexpected`
  → B-43 に到達。h2load の `failed`（ストリームエラー）は **Non-2xx に計上されない**ため、
  ハーネスの Errors 列（Non-2xx=0）では長期間検出できなかった。
- F-111/F-114 の「HTTP/3 は 1 データグラム 1 syscall の構造的制約」という結論は
  **副次要因**で、主因は B-43 だった（マージナル処理レート ~430 req/s/conn・2.3ms/req は
  当時から出ていたが、初期バーストの失敗 + 30 秒テールが平均を支配していた）。

## A/B 結果（veil_glibc、median ± stdev、-n 30000 -c100 -m10 / wrk -t4 -c100）

| Config | Proto | 変更前 | 変更後 | 変化 |
|---|---|---|---|---|
| h2_1_feat_http3 | **http3** | **421.2 ± 1.7**（lat 130.7ms） | **853.3 ± 3.8**（lat 1.12ms） | **+103%** |
| h2_1_feat_http3 | http1.1 | 2366.8 | 2739.0 | 環境回復分（nginx も同幅） |
| h2_1_feat_http3 | http2 | 2428.9 | 2703.2 | 同上 |
| h2_1_ktls_0_lb_kernel_ofc_1 | http1.1 | 2758.3 | **3298.3 ± 11.3** | 前回計測 3296.6 と一致 = 回帰なし |
| h2_1_ktls_0_lb_kernel_ofc_1 | http2 | 2363.2 | **2704.3 ± 22.7** | 前回計測 2659.1 と同水準 = 回帰なし |
| h2_1_feat_grpc | grpc | 1215.9（環境減速時） | **1475.2 ± 32.9** | 前回計測 1488.1 と同水準 = 回帰なし |
| h3_proxy | http3 | 625.1（**エラー 47**） | **671.2 ± 3.3（エラー 0）** | 安定化 |

veil_musl も同傾向（http3 file 832.6 / h3_proxy 666.0・エラー 0 / grpc 1456.5 /
http1.1 3249.8 / http2 2729.9）。全構成 Non-2xx=0・h2load failed=0。

- HTTP/3 は HTTP/2 比 **16% → 32%** へ倍増。B-43 修正単体の寄与が支配的
  （中間計測: 第2段+seccomp のみで 421→443（+5.2%）、B-43 追加で 777〜853）。
- 負荷中の veil CPU は 155% → **220〜246%**（4 コア中）へ上昇し、サーバが待機でなく
  実仕事で回るようになった。残る HTTP/2 とのギャップはユーザ空間 QUIC の
  per-request CPU コスト（暗号化 + パケット化。カーネル TLS/TCP との構造差）で、
  4 コア共有ホストでは以降の改善はレシオ的に小さい。
- h3_proxy のエラー混入（B-42）は本修正後の再計測で glibc/musl とも 0 件。
  B-42 の症状の相当部分が B-43 と同根だった可能性が高い（チケットは継続観察として保持）。

## gRPC の対照計測（veil の中継オーバーヘッド）

k6(50VU) → **grpcbin 直行（TLS :9001、veil なし）**: **1452 req/s / ~34ms**。
k6 → veil → grpcbin（h2c）: **1475 req/s / 32ms**。
→ 差は誤差範囲であり、**32ms のレイテンシは k6 クライアント + grpcbin 上流のコスト**。
veil の gRPC 中継（TLS 終端 + h2c プール F-106）のオーバーヘッドは実質ゼロで、
プロキシ側にこれ以上の改善余地はない。

## 再現手順

```bash
docker build -f docker/Dockerfile.glibc -t veil:glibc --build-arg CARGO_FEATURES='full' .
docker build -f docker/Dockerfile.musl  -t veil:musl  --build-arg CARGO_FEATURES='full' .
bash tools/perf/gen_configs.sh
CONFIG_GLOB='h2_1_feat_http3'             bash tools/perf/run_perf.sh   # H3 file
CONFIG_GLOB='h2_1_ktls_0_lb_kernel_ofc_1' bash tools/perf/run_perf.sh   # H1/H2 回帰
CONFIG_GLOB='h2_1_feat_grpc'              bash tools/perf/run_perf.sh   # gRPC
CONFIG_GLOB='h3_proxy'                    bash tools/perf/run_perf.sh   # H3 proxy

# B-43 再現（修正前のみ失敗する）
h2load --alpn-list=h3 -n 10 -c 1 -m10 https://<veil>:443/
```

## 関連

- [F-115](../backlog/features/F-115-http3-recvmmsg-sendmmsg-batching.md)（第1段+第2段で完了）
- [B-43](../backlog/bugs/B-43-http3-static-streamblocked-frameunexpected.md)（修正済み）
- [B-42](../backlog/bugs/B-42-http3-proxy-load-instability.md)（症状解消を確認、継続観察）
- 前回計測: [perf_f114_coverage_regression.md](perf_f114_coverage_regression.md)
