# F-109: HTTP/3・gRPC over H3 パリティ網羅（レポート最新版 Task 1/2）

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md` が指摘する **現行ギャップ** を埋める。

- **E2E**: HTTP/2 / gRPC(H2) に存在するが HTTP/3 / gRPC over H3 に無い 9 件
- **container_security**: 他プローブで HTTP/3 が漏れている 4 系統

**CI 組み込みは対象外** → [F-110](F-110-http3-grpc-parity-ci.md)。

## 背景

F-91〜F-107 で大半のエッジ・ストリーミング・DoS を実装済み。本チケットはレポート最新版の **Task 1/2 未実装リスト** のみを対象とする。

## 対象チェックリスト

### フェーズ1: E2E HTTP/3 パリティ

| # | テスト名 | H2 相当 |
|---|----------|---------|
| 1 | `test_http3_settings_negotiation` | `test_h2c_settings_negotiation` |
| 2 | `test_http3_flow_control` | `test_h2c_flow_control` |
| 3 | `test_http3_handshake_failure` | `test_h2c_handshake_failure` |

### フェーズ2: E2E gRPC over HTTP/3

| # | テスト名 | H2 相当 |
|---|----------|---------|
| 4 | `test_grpc_over_http3_proxy_timeout` | `test_grpc_proxy_timeout` |
| 5 | `test_grpc_over_http3_status_code` | `test_grpc_status_code` |
| 6 | `test_grpc_over_http3_streaming_detailed` | `test_grpc_streaming_detailed` |
| 7 | `test_grpc_over_http3_wire_protocol` | `test_grpc_wire_protocol` |
| 8 | `test_grpc_over_http3_client_slowloris` | `test_grpc_client_slowloris` |

### フェーズ3: E2E gRPC-Web over HTTP/3

| # | テスト名 | H2 相当 |
|---|----------|---------|
| 9 | `test_grpc_web_over_http3_cors` | `test_grpc_web_cors`（Preflight OPTIONS + POST） |

### フェーズ4: container_security

| ID | 対象 | 内容 |
|----|------|------|
| S-H3-SMUG | `smuggling_probe.sh` | H3 経由の CL/TE ヘッダインジェクション |
| S-H3-WS | `websocket_probe.sh` | RFC 9220 Extended CONNECT 不正リクエスト |
| S-H3-FEAT | `compression_cache_probe` / `rate_limit_probe` / `wasm_security_probe` | HTTP/3 並行検証 |
| S-G-WEB-H3 | `grpc_web_probe.sh` | `grpc-status` スプーフィング + 不正 Origin CORS |

## 改修内容

1. backlog 登録（本チケット + F-110 CI 分離）
2. `tests/e2e_tests.rs` に上記 E2E を追加
3. `http3-client` に攻撃モード追加 + 各 probe 配線
4. 失敗は `docs/artifacts` に切り分け。実装バグは B-* 化して修正
5. 一時成果物は gitignore / `docs/artifacts` 維持

## 受け入れ条件

- [ ] レポート Task 1/2 の全項目が実装または明確に文書化
- [ ] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [ ] full / default / no-default / 各 feature でビルド警告・エラーなし
- [ ] `allow(dead_code)` を理由なしに追加しない
- [ ] CI 組み込みは行わない（F-110）

## 依存・リスク

- handshake_failure は ALPN 不一致や不正 path で近似（真の証明書検証失敗はテスト client が skip verify）
- H3 smuggling は HTTP/3 が TE を持たないため CL 重複 / 不正 CL ヘッダ注入が中心
- Extended CONNECT 未実装時は 4xx/5xx 安全失敗で合格
- 既知フレーキー E2E は修正対象外

## 対応状況

進行中（fix/test）。
