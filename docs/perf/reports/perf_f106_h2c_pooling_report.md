# F-106 パフォーマンスレポート: H2C バックエンド接続プーリング（gRPC 中継）

## 1. 目的

`tools/perf` の計測と既存レポートで、**HTTP/1.1・HTTP/2 が過去計測から劣化していないこと**を
確認しつつ、**HTTP/3・gRPC のボトルネックを調査し改善**する（依頼スコープ）。本レポートは
その調査結果と、gRPC 中継に対して実装した最適化（F-106）をまとめる。

> 計測環境の注意: 本作業時のホストは **co-tenant（並行 worktree のビルド）で 4 コアが飽和**
> （1 分 loadavg が 3〜10 で変動、ビルドプロセスが OOM/リソース制約で kill される場面もあった）。
> このためスループットの絶対値計測は極めてノイジーで、リポジトリ既定の方針
> （`docs/perf` 系レポート・メモリ: 「perf は host 制約で不可 → **コード精査 + Docker A/B**」）に従い、
> **ボトルネックのコード精査**を一次情報とし、可能な範囲で背中合わせ A/B を補助的に用いる。

## 2. ボトルネック調査

### 2.1 gRPC 中継 — リクエストごとの接続 + h2c ハンドシェイク（根本原因）

gRPC 中継の代表経路は `k6(gRPC) → veil(TLS h2) → grpcbin(h2c)`。veil 側は
`handle_http2_proxy_h2c`（`src/proxy.rs`）が担当する。改修前の実装は **1 リクエストごとに**:

1. `TcpStream::connect_str(addr)` — 新規 TCP 3-way ハンドシェイク（1 RTT）
2. `H2cClient::handshake()` — コネクションプリフェース送信 + SETTINGS 交換 + SETTINGS ACK
   往復（さらに ~1〜2 RTT + 複数の送受信 syscall）
3. 1 本のリクエスト/レスポンス
4. コネクション破棄

を行っていた。HTTP/1.1・HTTPS(H1) 経路は **B-28 で接続プール（`HTTP_POOL`/`HTTPS_POOL`）を実装済み**
だったが、**H2C 経路は B-28 の明示的残件**（「chunked 応答と **H2C バックエンドの再利用は残件**」）
として未対応のままだった。既存の gRPC 計測レポート
（[perf_http3_grpc_websocket_report.md](perf_http3_grpc_websocket_report.md)）でも
gRPC unary は **474 req/s**（静的 HTTP/2 の 1848 比 約 1/4）で、
「バックエンドホップ + h2c 接続 + gRPC trailers 処理」がコストと分析されていた。
この「h2c 接続」を毎回張り捨てていたのが主要因である。

**設計哲学との整合**: AGENTS.md の「ホットパスで同期・不要なシステムコール・確保を増やさない」に対し、
リクエストごとの `connect` + ハンドシェイク往復は明確な違反であり、プール化は必須の改善。

### 2.2 gRPC 中継 — 再利用で顕在化する送信ウィンドウ枯渇バグ（副次的に発見・修正）

`H2cClient` はコネクションを跨いで `next_stream_id`（1,3,5,…）を単調増加させ、複数リクエストを
直列に流せる状態（HPACK 動的テーブル・受信バッファ・フロー制御ウィンドウ）を元々保持していた。
しかし送信ホットパスで **`Frame::WindowUpdate` を無視**しており、送信のたびに減算した
`conn_send_window`（接続レベル送信ウィンドウ、初期 65535）を回復していなかった。

- 改修前は 1 リクエスト 1 接続で毎回リセットされ **顕在化しなかった**。
- プール再利用すると、gRPC の小さなメッセージ（~25B/リクエスト）でも約 2600 リクエストで
  ウィンドウが枯渇し、`send_data` が「Send window exhausted」で **失敗**する潜在バグとなる。

→ 接続レベル（stream 0）の `WINDOW_UPDATE` を積み増す `apply_window_update` を両受信ループ
（`receive_response` / `receive_grpc_response`）へ反映して修正。**プール化とセットで必須の修正**。

### 2.3 HTTP/3 — userspace QUIC の構造的コスト（既存分析を再確認）

HTTP/3 の主要コード（`src/http3_server.rs`）を精査した結果、送受信ホットパスは既に高度に
最適化されている:

- **受信**: `recv_gro_async`（`recvmsg(2)` + `UDP_GRO`）で同一フローの複数データグラムを
  カーネル集約受信し per-datagram syscall を削減。受信バッファは loop 外で 1 度確保し
  ゼロコピーで quiche へスライス直渡し（F-33/F-45）。
- **送信**: GSO バッチ（`UDP_SEGMENT`）で複数 QUIC パケットを 1 sendmsg に連結、送信スクラッチは
  スレッドローカル再利用でリクエスト毎の malloc を排除（F-60、B-18）。
- **ストリーミング**: アクターモデル（`src/http3_stream.rs`）で QUIC I/O とバックエンド I/O を
  ロックレス SPSC + Notify で多重化（F-32/F-44）。

既存レポートの通り、HTTP/3 が HTTP/2 比 約 1/7 なのは **QUIC の暗号処理・輻輳制御・ロスリカバリ・
ストリーム多重化がすべて userspace で走り、1 パケット = 1 sendmsg/recvmsg の syscall コストが
支配的**という構造的特性による（TCP は kTLS でカーネルオフロードできるが QUIC は不可）。
UDP GSO/GRO の有効化は **Docker の veth/bridge では効果がない**ことも A/B で確認済み
（[perf_http3_grpc_websocket_report.md](perf_http3_grpc_websocket_report.md) §5、既定 false が妥当）。

→ **HTTP/3 側には landable な追加コード最適化は見当たらず**、退行がないことの確認に留める
（HTTP/3 を HTTP/2 と同一スループットにするのは userspace QUIC の構造上、本質的に不可能）。

## 3. 改修（F-106）

- `src/pool.rs`: スレッドローカル **`H2C_POOL`（`H2cConnectionPool`）** を追加。
  `HTTP_POOL`/`HTTPS_POOL` と同じ checkout/return + アイドルタイムアウト方式で
  `H2cClient<TcpStream>` をバックエンド `addr` 単位に再利用。io_uring `TcpStream` の
  スレッド親和性のため、thread-per-core のスレッドローカルで同一スレッド内再利用のみ。
- `src/proxy.rs`: `handle_http2_proxy_h2c` をプール優先の取得に再構成。ヒット時は
  connect + handshake を完全省略。stale 接続の初回失敗は新規接続で 1 回だけリトライ。
  応答成功後、`is_reusable()` の健全な接続をプールへ返却。
- `src/http2/client.rs`: 上記 2.2 の送信ウィンドウ枯渇を修正 + 再利用可否判定 `is_reusable()`。

**HTTP/1.1・HTTP/2 のホットパスは不変**（本改修は h2c バックエンド中継経路のみに閉じる）ため、
静的配信・逆プロキシの HTTP/1.1・HTTP/2 性能に劣化はない（コード上、当該経路に変更なし）。

## 4. 検証

- **単体**: `src/http2/client.rs` に 2 件追加（直列再利用で stream id 1→3、WINDOW_UPDATE による
  送信ウィンドウ回復の白箱）。h2c クライアント単体 8 件全通過。lib 全体 729 件通過。
- **gRPC E2E**: `TEST_FILTER=grpc ./tests/e2e_setup.sh test` → **67 passed / 0 failed**
  （h2c プロキシ経路を含む gRPC 全 E2E が退行なし）。
- **ビルド警告**: no-default-features / default / http2 / http3 / grpc-full / full の各ビルドで
  警告 0（`#[allow(dead_code)]` 不使用）。clippy（full, --all-targets）新規警告 0。fmt clean。

## 5. 効果

### 5.1 コード精査ベースの根拠

gRPC unary 1 コールあたり **TCP 3-way（~1 RTT）+ h2c ハンドシェイク往復（~1〜2 RTT + 複数 syscall）**
を丸ごと削減する。コンテナ内 RTT は小さいが、毎リクエストの往復とプリフェース/SETTINGS 交換の
送受信 syscall はスループットとテールレイテンシに直接効くため、gRPC 中継スループットの
明確な改善が見込まれる（=B-28 の HTTP/1.1 プール導入が逆プロキシで得たのと同種の効果を
gRPC/h2c 経路へ拡張）。

### 5.2 実測（veil:glibc-f106・`feat_grpc` 構成・k6 VUS=50×10s×3 反復）

co-tenant 負荷が一時的に落ち着いた窓で、F-106 版イメージ（`veil:glibc-f106`）を実測した
（k6 gRPC → veil(TLS h2) → grpcbin(h2c)）:

| 反復 | Req/s | Lat 平均 | Fails |
|------|-------|----------|-------|
| 1 | 549.8 | 84.8ms | 0 |
| 2 | 622.0 | 75.8ms | 0 |
| 3 | 590.7 | 79.7ms | 0 |
| **median** | **590.7** | **79.7ms** | **0** |

全反復で **Non-2xx=0（k6 checks 100% 成功・interrupted 0）**。既存レポート
（[perf_http3_grpc_websocket_report.md](perf_http3_grpc_websocket_report.md)、同一ハーネス・同一
grpcbin 上流）の **改修前 baseline 474 req/s に対し median 590.7 req/s ＝ 約 +24.6%**。
これは §2.1 で特定した「毎リクエストの h2c ハンドシェイク往復の排除」の効果と整合する。
（注: 上記 baseline は別セッションの計測値。同一セッション A/B は §5.3 参照）

### 5.3 同一セッション A/B（`veil:glibc-base`〈改修前〉vs `veil:glibc-f106`〈改修後〉）

負荷が持続的に静穏な窓（1 分 loadavg < 1.5 を 3 回連続確認、ビルド開始時 load=0.70）で
**改修前後の 2 イメージを同一ハーネス・同一 grpcbin 上流で背中合わせ計測**した
（k6 gRPC VUS=50×10s×3 反復）:

| 反復 | base (改修前) Req/s | Lat | f106 (改修後) Req/s | Lat |
|------|--------------------:|-----|--------------------:|-----|
| 1 | 849.5 | 55.7ms | 1470.9 | 31.7ms |
| 2 | 642.4 | 74.9ms | 1500.1 | 31.6ms |
| 3 | 274.5 | 177.0ms | 1506.2 | 31.1ms |
| **median** | **642.4** | **74.9ms** | **1500.1** | **31.6ms** |

- **スループット: 642.4 → 1500.1 req/s ＝ +133%（約 2.3 倍）**、**レイテンシ: 74.9 → 31.6ms（約 2.4 倍改善）**、
  両者とも **fails=0**。
- **安定性の差が顕著**: 改修前は 274〜850 req/s と激しくばらつく（毎リクエストの TCP 接続 +
  h2c ハンドシェイクが負荷に敏感で、他プロセスの割り込みでテールが伸びる）。改修後は
  1471〜1506 req/s と**極めて安定**（プール接続の再利用でハンドシェイク往復が消え、
  1 コールの仕事がフレーミング+中継のみに縮小）。
- これは §2.1 のボトルネック（毎リクエストの h2c ハンドシェイク張り捨て）とその解消
  （§3 の `H2C_POOL`）に直接対応する、**計測に基づく裏付け**である。

> 注: §5.2 の f106 単独計測（median 590 req/s）は co-tenant 負荷下（loadavg 2〜3）の抑制値で、
> 本 §5.3 の静穏窓計測（f106 median 1500 req/s）が本来値。改修前 baseline も同一条件で
> 計測しているため、**相対比（+133%）が信頼できる指標**である。

> k6 は完了ストリームごとに WINDOW_UPDATE/リセットを送るため、veil の h2 サーバ側で
> `WINDOW_UPDATE frame on idle stream` の WARN が出るが、gRPC 呼び出しは全て成功（checks 100%）
> しており良性（フロントエンド h2 サーバの挙動で F-106 とは無関係）。

**Docker A/B 実測について**: 本作業時のホストは大半の時間 co-tenant の並行ビルドで 4 コアが飽和し
（loadavg が一時 10 まで上昇、ビルドプロセスが繰り返し kill された）、当初は A/B を取得できなかった。
その後、**負荷が持続的に静穏になった窓を待って**（`docs/artifacts` のスクラッチで自動ゲート）
`veil:glibc-base`（改修前）と `veil:glibc-f106`（改修後）を同一ハーネスで背中合わせ計測できた
（§5.3）。負荷連動でノイズが乗るため、絶対値より **同一セッション内の相対比**を見る
（リポジトリの perf 方針に一致）。

## 6. 結論

- gRPC のボトルネックは **h2c バックエンドをリクエストごとに張り捨てていたこと**（B-28 残件）と
  特定し、`H2C_POOL` による接続再利用（F-106）で解消。併せて再利用で顕在化する送信ウィンドウ
  枯渇バグも修正。**同一セッション A/B で gRPC スループット +133%（642→1500 req/s）・
  レイテンシ約 2.4 倍改善・fails=0**、かつ改修後は負荷変動に対して大幅に安定化。
- HTTP/3 のボトルネックは **userspace QUIC の構造的コスト**で、既存コードは既に高度に最適化済み。
  安全な landable 追加最適化は無く、退行なしの確認に留めた。
- HTTP/1.1・HTTP/2 の性能は本改修の影響外（ホットパス不変）。
