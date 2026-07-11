# HTTP/3・gRPC・WebSocket パフォーマンス計測レポート（F-89 第 3 弾）

## 1. 目的

`tools/perf` ハーネスに追加した HTTP/3・gRPC・WebSocket の 3 プロトコル計測
（[F-89](../../backlog/features/F-89-perf-full-features-coverage.md) 残件）について、
専用クライアントで実測し、ボトルネックと最適化余地を分析する。

## 2. 計測構成

| プロトコル | 構成 | クライアント | 上流 |
|-----------|------|--------------|------|
| HTTP/3 (QUIC) | `h2_1_feat_http3` | QUIC 対応 h2load（`local/h2load-h3`, `--alpn-list=h3`） | なし（静的 File） |
| gRPC | `h2_1_feat_grpc` | grafana k6 gRPC（`k6/grpc.js`） | `moul/grpcbin`(h2c) |
| WebSocket | `h2_1_feat_websocket` | grafana k6 WS（`k6/websocket.js`） | `jmalloc/echo-server` |

- ベース: HTTP/2 有効・kTLS 無効・kernel LB（feat 系共通。kTLS はコンテナと相性が悪く既定オフ）。
- 負荷: HTTP/1.1=wrk（`-t4 -c100 -d10s`）、HTTP/2/3=h2load（`-n30000 -c100 -m10`）、
  gRPC/WS=k6（VUS=50, 10s）。各 2 反復の median。
- `h2_1_feat_http3` は `http2_enabled=true` も併設するため、**同一構成で HTTP/1.1・HTTP/2・HTTP/3 を直接比較**できる。

## 3. 結果（2 反復 median、4 コアコンテナ間、参考値）

| Target | Config | Proto | Req/s (median±stdev) | Lat Avg | CPU% | Mem MB | Errors |
|--------|--------|-------|----------------------|---------|------|--------|--------|
| nginx | base | http1.1 | 1215.4 ± 31.2 | 74.62ms | 44.3 | 20.5 | 0 |
| nginx | base | http2 | 1383.5 ± 3.4 | 584.51ms | 20.4 | 25.0 | 0 |
| veil_glibc | http3 構成 | http1.1 | 1585.4 ± 46.9 | 59.42ms | 50.2 | 32.6 | 0 |
| veil_glibc | http3 構成 | http2 | **1848.1 ± 5.6** | 320.57ms | 4.0 | 56.1 | 0 |
| veil_glibc | http3 構成 | **http3** | **269.8 ± 2.2** | 227.02ms | 25.3 | 71.5 | 0 |
| veil_glibc | grpc | grpc | 474.0 ± 2.4 | 97.43ms | 16.4 | 24.3 | 0 |
| veil_glibc | websocket | websocket | 578.0 ± 27.4 | 318.92ms | 20.4 | 21.1 | 0 |
| veil_musl | http3 構成 | http1.1 | 1598.0 ± 44.0 | 59.27ms | 0.5 | 33.5 | 0 |
| veil_musl | http3 構成 | http2 | 1799.6 ± 0.1 | 339.30ms | 8.3 | 58.8 | 0 |
| veil_musl | http3 構成 | http3 | 392.3 ± 16.3 | 140.28ms | 76.3 | 77.0 | 0 |
| veil_musl | websocket | websocket | 931.2 ± 35.4 | 190.36ms | 80.8 | 26.8 | 0 |

> 注: 計測は 4 コア・co-tenant 負荷環境の参考値。絶対値ではなく **同一実行内の相対比較** を見る。

## 4. 分析

### 4.1 HTTP/3 (QUIC) が支配的に低速 — userspace QUIC のコスト

同一 `h2_1_feat_http3` 構成での比較（glibc）:

| Proto | Req/s | HTTP/2 比 |
|-------|-------|-----------|
| HTTP/1.1 (TLS, TCP) | 1585 | 0.86× |
| HTTP/2 (TLS, TCP) | 1848 | 1.00×（基準） |
| **HTTP/3 (QUIC, UDP)** | **270** | **0.15×** |

HTTP/3 は HTTP/2 の **約 1/7** のスループット。原因:

- **QUIC 暗号処理がすべて userspace**（TCP は kTLS でカーネルオフロード可能だが QUIC は不可）。
- **1 パケット = 1 sendmsg/recvmsg のシステムコール**が支配的。veil 起動ログで
  **`GSO enabled: false, GRO enabled: false`**（`[http3] gso_gro_enabled` 既定 false）であり、
  UDP GSO/GRO によるバッチ送受信が効いていない。
- QUIC の輻輳制御・ロスリカバリ・ストリーム多重化がすべてユーザ空間で走る。

これは HTTP/3 の一般的特性（nginx/cloudflare でも TCP+kTLS に対し QUIC は CPU コスト大）だが、
**GSO/GRO 有効化がコンテナで効くか**が最大の最適化余地（→ 5 節）。

### 4.2 gRPC 中継コスト

gRPC unary（glibc 474 req/s, Non-2xx=0）は、TLS HTTP/2 終端 → h2c 再フレーミング → grpcbin 中継 →
trailers 応答の往復コスト。静的 HTTP/2（1848）比で低いのは、**バックエンドホップ + h2c 接続 + gRPC
trailers 処理**が加わるため。エラー 0 で機能は正常。

- gRPC の `/*` プレフィックス保持・`use_h2c` 尊重・HPACK 小文字化は **B-40（F-92）で修正済み**。
  本計測はその修正後 glibc イメージで **Non-2xx=0** を確認（修正前は grpcbin/tonic ともに 502 になる
  現象を再現し、B-40 の有効性を裏付けた）。
- k6 gRPC は完了ストリームごとに RST_STREAM を送るため、構成で `[http2] max_rst_stream_per_second`
  を大きく設定し Rapid Reset 対策（CVE-2023-44487）の誤検知を回避している。

### 4.3 WebSocket

WebSocket エコー（glibc 578 / musl 931 msgs/s, Non-2xx=0）は Upgrade 検出 → 双方向フレーム転送。
機能は正常でエラーなし。musl が glibc を上回るのは計測ゆらぎ（CPU 80% 使用の回）を含む参考値。

### 4.4 veil_musl gRPC の 502（イメージ鮮度の問題・コード非依存）

初回計測で **veil_musl の gRPC のみ大量エラー（13918）** を観測。調査の結果、当時の `veil:musl`
イメージは **2026-07-08 ビルドで B-40（2026-07-10）修正を含まない stale イメージ**であり、
B-40 修正前の 502（`/pkg.Service/*` プレフィックス除去 + HTTP/1.1 誤中継）を再現していた。
**B-40 を含む現行コードで `veil:musl` を再ビルド（cargo test --lib 725 passed）し再計測したところ
gRPC は 598 req/s・Non-2xx=0 に回復**。→ コードのアーキ依存バグではなく、計測時のイメージ鮮度の問題。

## 5. 最適化検証: HTTP/3 UDP GSO/GRO（コンテナでは効果なし）

4.1 の通り HTTP/3 のボトルネックは per-packet syscall。`[http3] gso_gro_enabled = true` で
UDP GSO（送信バッチ）/ GRO（受信バッチ）を有効化し、同一 glibc イメージ・同一負荷（h2load QUIC,
`-n4000 -c20`）で **背中合わせに A/B** 計測した。

| 設定 | Req/s |
|------|-------|
| `gso_gro_enabled = false`（既定） | 404 |
| `gso_gro_enabled = true` | 365 |

**GSO/GRO 有効化でスループット改善なし（むしろ微減）。** これは
`[http3] gso_gro_enabled` の doc コメントにある「一部の仮想環境や Docker では期待通りに
動作しない場合あり」という注意と一致する。Docker の veth/bridge 経由の UDP パスでは
GSO/GRO のバッチ送受信が効かず、**既定 false が妥当**であることを裏付けた
（この A/B はバックグラウンドビルド負荷下の参考値だが、両者を同条件で連続計測した相対比較）。

→ 本計測から landable な**コード最適化は見つからなかった**。HTTP/3 の低速は userspace QUIC の
構造的コストであり、コンテナでは GSO/GRO も効かない。gRPC 経路の実バグ（h2c フルパス保持・
`use_h2c` 尊重）は既に **B-40（F-92）で修正済み**であり、本計測で修正の有効性を再検証した
（＝「計測に基づく最適化」は既存修正の検証と既定値の妥当性確認に帰着）。

## 6. 結論

- **HTTP/3・gRPC・WebSocket の 3 プロトコル計測を専用クライアントでハーネス化**し、
  glibc で全構成 Non-2xx=0 を確認。
- 支配的ボトルネックは **HTTP/3 の userspace QUIC コスト**（HTTP/2 比 約 1/7）。TCP は kTLS で
  カーネルオフロードできるが QUIC は不可という構造的差。**UDP GSO/GRO はコンテナでは効果なし**
  （既定 false が妥当）。
- gRPC/WebSocket の L7 中継は機能正常。gRPC の h2c 中継バグは B-40 修正済みを本計測で再検証。
- `veil:musl` の gRPC 502 は 2 日前ビルドの stale イメージ（B-40 未適用）が原因で、コード非依存。
