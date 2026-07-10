# 実装済みテストケース一覧

作成日: 2026-07-10  
調査対象: `tests/e2e_tests.rs`（約 420 `test_*`）、`tests/e2e_setup.sh`、`tools/container_security/`

---

## 1. E2E — HTTP/3（実装済み）

| テスト関数 | 対応 required ID |
|------------|------------------|
| `test_http3_basic_connection` / `_async` | E-H3-01 |
| `test_http3_get_request` / `_async` | E-H3-02 |
| `test_http3_post_request` | E-H3-03 |
| `test_http3_proxy_forwarding` | E-H3-04 |
| `test_http3_proxy_load_balancing` | E-H3-05 |
| `test_http3_proxy_compression` | E-H3-06 |
| `test_http3_multiple_streams` / stream_priority / cancellation / timeout / close | E-H3-07 |
| `test_http3_bidirectional_streams` / `*_streaming*` / large / chunked | E-H3-08 |
| `test_http3_0rtt_connection` | E-H3-09 |
| `test_http3_configuration_check` / tls_handshake / invalid_frame / backend_failure | E-H3-16 |
| `test_http3_qpack_compression` / connection_migration / concurrent / throughput / latency | E-H3-16 |
| `test_http3_proxy_header_manipulation` | F-HDR×H3 |

**未実装（本作業前）**: E-H3-10〜15（rate_limit / ip_restriction / wasm / cache / 0rtt security / websocket）

---

## 2. E2E — gRPC（実装済み）

| テスト関数 | 対応 |
|------------|------|
| `test_grpc_connection_async` / h2c / unary / basic | E-G-01 |
| client/server/bidi streaming / streaming_detailed | E-G-02 |
| grpc_web_binary / text / cors / cors_headers | E-G-03 |
| invalid_frame / oversized / malformed_protobuf / stream_reset | E-G-04 |
| `test_grpc_http2_framing` | E-G-05（**薄い実装**: フレーム破壊検証が不完全） |
| proxy_forwarding / load_balancing / timeout / error / trailer / compression* / metadata / wire / status | E-G-08 |
| `test_e2e_grpc_health_check_metrics` | E-G-09 |

**未実装（本作業前）**: E-G-06 `test_grpc_over_http3`、E-G-07 `test_grpc_client_slowloris`、E-G-05 の厳密化

---

## 3. E2E — その他（要約）

- HTTP/1.1: 基本転送、ヘッダ操作、圧縮、LB、キャッシュ、レート制限専用ルート、IP 制限、Range、Redirect、エラー、スマグリング、Keep-Alive、メソッド、ストレス等
- HTTP/2: multiplexing、HPACK、chunked/streaming、ALPN、接続再利用
- WASM: header filter / http_call / 各種 lifecycle・capability
- WebSocket: upgrade / bidirectional / invalid / close
- L4: passthrough / least_conn / TLS terminate
- Reload / TLS cipher suites / kTLS / Admin / Metrics / buffering modes
- B-17 回帰、F-62 WASM http_call 等

`e2e_setup.sh` が生成するルート: `/`、`/cached/*`、`/rate-limited/*`、`/wasm/*`、`/api/ip-restricted/*`、gRPC、WS、L4、ヘルス等。

---

## 4. container_security — 実装済みプローブ

| スクリプト | 内容 |
|------------|------|
| `http3_probe.sh` | UDP 到達 + http3-client GET + TLS health（**攻撃系なし**） |
| `grpc_probe.sh` | malformed body / oversized metadata / grpc-over-h1 + health |
| `grpc_web_probe.sh` | malformed / invalid b64 / oversized metadata |
| `websocket_probe.sh` / `ws-probe` | WS 外形 |
| `rate_limit_probe.sh` / `wasm_security_probe.sh` / `compression_cache_probe.sh` | 各機能 |
| `admin_security` / `metrics` / `otel` / `access_log` / `l4_flood` | 管理・観測・L4 |
| `h2spec_run` / fuzz / chaos / slowloris / circuit_breaker / toxiproxy | プロトコル・カオス |
| `run_smuggling` / differential / testssl / trivy / semgrep / sbom / gitleaks | セキュリティ |
| `run_full_features_security.sh` | 上記 full プローブ群オーケストレーション |

**未実装（本作業前）**: S-H3-02〜05、S-G-05〜07

---

## 5. クライアント / ハーネス資産

| 資産 | 用途 |
|------|------|
| `tests/common/http3_client.rs` | h3+quinn E2E クライアント |
| `tests/common/grpc_client.rs` | tonic / HTTP/1.1 gRPC 互換 |
| `tests/common/http1_client.rs` / `http2_client.rs` | H1/H2 |
| `tests/grpc_server/` | tonic バックエンド |
| `tools/.../http3-client` | quiche GET プローブ |
| `tools/.../ws-probe` / `fuzz-http` / `bad-backend` | 外形検証 |
