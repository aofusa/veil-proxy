# 必要テストケース一覧（全機能 × プロトコル）

作成日: 2026-07-10  
根拠: `AGENTS.md` / `README.md` / `examples/config.toml` / `Cargo.toml` `[features]` / `docs/artifacts/test_coverage_report.md`

本ドキュメントは **E2E（`tests/e2e_tests.rs` + `e2e_setup.sh`）** と **container_security（`tools/container_security/`）** が満たすべきテストケースの正本とする。  
「実装済み / 不足」は別ファイルを参照。

---

## 1. 実装済み機能一覧（検証対象）

### 1.1 Cargo features

| Feature | 概要 |
|---------|------|
| `ktls` | Kernel TLS |
| `http2` | HTTP/2 (h2 / h2c) |
| `http3` | HTTP/3 over QUIC |
| `grpc` / `grpc-web` / `grpc-full` | gRPC ワイヤ・gRPC-Web・HTTP/3 gRPC |
| `wasm` | Proxy-Wasm フィルタ |
| `compression` | gzip/br/zstd |
| `cache` | メモリ/ディスクキャッシュ |
| `metrics` | Prometheus |
| `websocket` | WebSocket プロキシ |
| `rate-limit` | レート/接続制限 |
| `buffering` | streaming/full/adaptive |
| `opentelemetry` | OTLP エクスポート |
| `admin` | Admin API |
| `access-log` | 構造化アクセスログ |
| `l4-proxy` | L4 TCP/UDP |
| `full` | 上記複合 |

### 1.2 設定ドメイン（examples/config.toml）

- `[server]` / `[tls]` / `[http2]` / `[http3]` / `[security]` / `[performance]`
- `[logging]` / `[access_log]` / `[prometheus]` / `[opentelemetry]` / `[admin]` / `[wasm]`
- `[[route]]` + conditions / action / security / cache / buffering / modules
- upstreams: RR / least_conn / ip_hash / weighted、health_check (http/tcp/grpc)、TLS upstream
- L4 listeners、circuit breaker / retry 等レジリエンス

### 1.3 クライアントプロトコル軸

| ID | プロトコル | 備考 |
|----|------------|------|
| P-H1 | HTTP/1.1 TLS | 主経路 |
| P-H1C | HTTP/1.1 cleartext | リダイレクト等 |
| P-H2 | HTTP/2 TLS (ALPN) | |
| P-H2C | HTTP/2 prior knowledge | ポート 8081 |
| P-H3 | HTTP/3 QUIC | UDP 443/8443 |
| P-GRPC-H2 | gRPC over HTTP/2 | content-type application/grpc |
| P-GRPC-H2C | gRPC over H2C | |
| P-GRPC-H3 | gRPC over HTTP/3 | grpc-full |
| P-GRPC-WEB | gRPC-Web / text | |
| P-WS | WebSocket over H1/H2 | |
| P-WS-H3 | WebSocket over HTTP/3 (RFC 9220) | 拡張 CONNECT |
| P-L4 | L4 passthrough / terminate / least_conn | |

### 1.4 ミドルウェア / 機能軸

| ID | 機能 |
|----|------|
| F-RL | レート制限 (429) |
| F-IP | IP 制限 (403) |
| F-METHOD | メソッド制限 |
| F-WASM | WASM ヘッダ/ボディフィルタ |
| F-CACHE | キャッシュ hit/miss / Age / 304 |
| F-COMP | 圧縮 |
| F-HDR | レスポンスヘッダ追加/除去 |
| F-LB | ロードバランシング (RR/LC/hash/weighted) |
| F-HC | ヘルスチェック (http/tcp/grpc) |
| F-BUF | buffering modes |
| F-METRICS | Prometheus |
| F-OTEL | OpenTelemetry |
| F-ADMIN | Admin API 認証 |
| F-ACCESS | access log |
| F-RELOAD | SIGHUP 設定/証明書リロード |
| F-SMUG | スマグリング拒否 |
| F-SIZE | 巨大ヘッダ/ボディ拒否 |
| F-0RTT | QUIC 0-RTT / 早期データ |

---

## 2. プロトコル × 機能マトリクス（必須）

記号: **R** = 必須 E2E、**S** = container_security プローブ必須、**—** = 非該当

| 機能 \\ プロトコル | H1 | H2 | H2C | H3 | gRPC-H2 | gRPC-H3 | gRPC-Web | WS | L4 |
|--------------------|----|----|-----|----|---------|---------|----------|----|----|
| 基本 GET/転送 | R | R | R | R | R | R | R | R | R |
| POST/ボディ | R | R | R | R | R | R | R | — | — |
| ストリーミング | R | R | R | R | R | R | — | R | R |
| レート制限 F-RL | R | R | — | **R** | R | **R** | — | — | — |
| IP 制限 F-IP | R | R | — | **R** | — | — | — | — | — |
| WASM F-WASM | R | R | — | **R** | — | — | — | — | — |
| キャッシュ F-CACHE | R | R | — | **R** | — | — | — | — | — |
| 圧縮 F-COMP | R | R | — | R | R* | — | — | — | — |
| LB F-LB | R | R | — | R | R | — | — | — | R |
| 不正フレーム/異常系 | R/S | R/S | S | **R/S** | **R/S** | **S** | S | S | S |
| リソース枯渇/DoS | S | S | S | **S** | **S** | **S** | S | S | S |
| 0-RTT セキュリティ | — | — | — | **R** | — | — | — | — | — |
| WebSocket over H3 | — | — | — | **R** | — | — | — | — | — |

\* gRPC 圧縮は content-coding ではなく grpc-encoding

---

## 3. E2E 必須ケース（HTTP/3 フォーカス）

| ID | テスト名 | 目的 | 期待 |
|----|----------|------|------|
| E-H3-01 | `test_http3_basic_connection` | QUIC 接続 | 接続成功 |
| E-H3-02 | `test_http3_get_request` | GET | 200 |
| E-H3-03 | `test_http3_post_request` | POST | 2xx/適切応答 |
| E-H3-04 | `test_http3_proxy_forwarding` | 逆プロキシ | バックエンド到達 |
| E-H3-05 | `test_http3_proxy_load_balancing` | LB | 分散 or 複数 200 |
| E-H3-06 | `test_http3_proxy_compression` | 圧縮 | encoding or 200 |
| E-H3-07 | `test_http3_stream_*` | 多重/取消/優先度 | 安定 |
| E-H3-08 | `test_http3_*_streaming` | 双方向ストリーム | 完了 |
| E-H3-09 | `test_http3_0rtt_connection` | セッション再開 | 2 回目成功 |
| E-H3-10 | **`test_http3_rate_limiting`** | `/rate-limited/*` 経由 H3 | 429 出現 |
| E-H3-11 | **`test_http3_ip_restriction`** | `/api/ip-restricted/*` | 403 |
| E-H3-12 | **`test_http3_wasm_integration`** | `/wasm/*` | WASM ヘッダ or 200+生存 |
| E-H3-13 | **`test_http3_cache_hit_miss`** | `/cached/*` 2 回 | hit 指標 or 同一ボディ |
| E-H3-14 | **`test_http3_early_data_0rtt_security`** | POST 0-RTT | 拒否/1-RTT フォールバック・クラッシュなし |
| E-H3-15 | **`test_http3_websocket`** | RFC 9220 | 確立成功 or 明確な非対応応答・クラッシュなし |
| E-H3-16 | 既存 large/chunked/invalid/migration 等 | 回帰 | 通過 |

## 4. E2E 必須ケース（gRPC フォーカス）

| ID | テスト名 | 目的 | 期待 |
|----|----------|------|------|
| E-G-01 | `test_grpc_unary_call` | Unary | 200 + フレーム |
| E-G-02 | `test_grpc_*_streaming` | C/S/Bidi | 動作 |
| E-G-03 | `test_grpc_web_*` | Web/CORS/text | 動作 |
| E-G-04 | `test_grpc_invalid_frame` / oversized / malformed | 異常系 | 安全拒否 |
| E-G-05 | `test_grpc_http2_framing` | H2 フレーム整合 | 不正 DATA で RST/安全切断・panic なし |
| E-G-06 | **`test_grpc_over_http3`** | Unary (+可能なら stream) over H3 | 200 / grpc-status 到達 |
| E-G-07 | **`test_grpc_client_slowloris`** | 極遅フレーム送信 | タイムアウト切断・プロセス生存 |
| E-G-08 | `test_grpc_proxy_*` / trailer / compression | プロキシ統合 | 通過 |
| E-G-09 | `test_e2e_grpc_health_check_metrics` | gRPC HC | メトリクス露出 |

## 5. container_security 必須ケース（HTTP/3）

| ID | プローブ | 内容 | 合格条件 |
|----|----------|------|----------|
| S-H3-01 | `http3_probe` GET | 正常 GET | 200 系 + プロセス生存 |
| S-H3-02 | **`h3_handshake_flood`** | QUIC Initial 大量送信 | hang/crash なし・TLS ヘルス 200 |
| S-H3-03 | **`h3_qpack_bomb`** | 巨大/高圧縮ヘッダ | 拒否 or 処理・crash なし |
| S-H3-04 | **`h3_cid_spoof`** | 不正 CID パケット | 既存接続非破壊・生存 |
| S-H3-05 | **`h3_malformed_frames`** | 不正 H3 フレーム | RST/close・生存 |
| S-H3-06 | post_probe TLS health | プローブ後 | 200 |

## 6. container_security 必須ケース（gRPC）

| ID | プローブ | 内容 | 合格条件 |
|----|----------|------|----------|
| S-G-01 | malformed body (<5B) | H2C | 応答あり crash なし |
| S-G-02 | oversized metadata | 巨大 grpc-timeout | 同上 |
| S-G-03 | grpc over HTTP/1.1 | 誤経路 | 同上 |
| S-G-04 | gRPC-Web 不正 | binary/text | 同上 |
| S-G-05 | **`grpc_status_spoofing`** | リクエストに grpc-status 付与 | 異常なし（除去/透過どちらでも crash なし） |
| S-G-06 | **`grpc_oversized_message`** | 巨大 length-prefix 宣言 | 413 or RESOURCE_EXHAUSTED or 切断・生存 |
| S-G-07 | **`grpc_infinite_streaming`** | 無データ長寿命ストリーム | タイムアウト回収・生存 |
| S-G-08 | post health | プローブ後 | 200 |

## 7. その他プロトコル（網羅性のため一覧・既存で充足するものは省略可）

- H1/H2: ルーティング条件、Range、Redirect、Expect、スマグリング、kTLS、Admin、Metrics、Reload、L4、カオス、h2spec、ファズ、差分テスト等（既存 F-90 / F-02 系で広範にカバー）
- 本チケット（F-91）の **実装優先対象** は §3 E-H3-10〜15、§4 E-G-05〜07、§5 S-H3-02〜05、§6 S-G-05〜07

---

## 8. 非ゴール

- プロダクション実装バグの修正（失敗時は backlog bugs のみ）
- 既知フレーキー E2E（負荷系等）の根絶
- XDP/eBPF 専用環境テスト
