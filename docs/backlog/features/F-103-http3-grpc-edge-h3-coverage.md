# F-103: gRPC over HTTP/3 エッジケース E2E と HTTP/3 攻撃プローブ拡充

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md` が指摘する残ギャップを埋める。

- **E2E**: HTTP/2 ベース gRPC に存在するエッジ・異常系を gRPC over HTTP/3 へ移植（12 件）+ HTTP/3 多重化/coalesce（1 件）
- **container_security**: gRPC over H3 攻撃 5 件（S-G-H3-09〜13）+ QUIC コントロール層 3 件（S-H3-18〜20）

**CI 組み込みは対象外** → [F-104](F-104-http3-grpc-edge-h3-ci.md)。

## 対象チェックリスト（レポート §3）

### フェーズ1: E2E gRPC over HTTP/3

| # | テスト名 | H2 相当 |
|---|----------|---------|
| 1 | `test_grpc_over_http3_invalid_frame` | `test_grpc_invalid_frame` |
| 2 | `test_grpc_over_http3_oversized_message` | `test_grpc_oversized_message` |
| 3 | `test_grpc_over_http3_malformed_protobuf` | `test_grpc_malformed_protobuf` |
| 4 | `test_grpc_over_http3_stream_reset` | `test_grpc_stream_reset` |
| 5 | `test_grpc_over_http3_flow_control_window_boundary` | `test_grpc_flow_control_window_boundary` |
| 6 | `test_grpc_over_http3_retry_and_hedging` | `test_grpc_retry_and_hedging` |
| 7 | `test_grpc_over_http3_keepalive_ping` | `test_grpc_keepalive_ping` |
| 8 | `test_grpc_over_http3_server_stream_abnormal_termination` | `test_grpc_server_stream_abnormal_termination` |
| 9 | `test_grpc_over_http3_consistent_hashing` | `test_grpc_consistent_hashing` |
| 10 | `test_grpc_over_http3_active_health_check` | `test_grpc_active_health_check` |
| 11 | `test_grpc_over_http3_buffering_bypass` | `test_grpc_buffering_bypass` |
| 12 | `test_grpc_over_http3_wasm_interceptor` | `test_grpc_wasm_interceptor` |

### フェーズ2: E2E HTTP/3 全般

| # | テスト名 | H2 相当 |
|---|----------|---------|
| 13 | `test_http3_multiplexed_coalesced_responses` | `test_http2_multiplexed_coalesced_responses` |

### フェーズ3: container_security gRPC over H3

| ID | モード / 内容 |
|----|----------------|
| S-G-H3-09 | `grpc_oversized` — 巨大 gRPC メッセージ |
| S-G-H3-10 | `grpc_infinite_streaming` — CL 極大 + ボディ未送保持 |
| S-G-H3-11 | `grpc_fragmented_lpm` — LPM 1 バイト分割遅延送信 |
| S-G-H3-12 | Path Bypass over H3（curl `--http3-only` または http3-client） |
| S-G-H3-13 | WASM Crash Resilience over H3 |

### フェーズ4: container_security HTTP/3 コントロール層

| ID | モード |
|----|--------|
| S-H3-18 | `control_stream_abuse` |
| S-H3-19 | `cid_exhaustion` |
| S-H3-20 | `token_spoofing` |

## 改修内容

1. backlog 登録（本チケット + F-104 CI 分離）
2. `tests/e2e_tests.rs` に上記 E2E を追加（`#[cfg(all(feature = "grpc", feature = "http3"))]` 等）
3. `http3-client` に攻撃モード追加 + `grpc_probe.sh` / `http3_probe.sh` 配線
4. 失敗は `docs/artifacts` に切り分け。実装バグは B-* 化して修正
5. 一時成果物は gitignore / `docs/artifacts` 維持

## 受け入れ条件

- [x] レポート §3 の全項目が実装または明確に文書化
- [x] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [x] full / default / no-default / 各 feature でビルド警告・エラーなし
- [x] `allow(dead_code)` を理由なしに追加しない
- [x] CI 組み込みは行わない（F-104）

## 依存・リスク

- QUIC PING の厳密検証は quinn/h3 制約で近似（アイドル後 Unary 生存）
- 真の Retry Token 偽造は quiche 内部制約で近似（不正 Initial/Token 風パケット + 生存確認）
- container_security は Docker イメージ再ビルドが必要
- 既知フレーキー E2E は修正対象外

## 対応状況

完了（fix/test）。  
詳細: `docs/artifacts/f103_failed_tests_report.md`  
検証: 単体 725 / 統合 53 / E2E 490 PASS（F-103 新規 13） / container_security 新規 8 プローブ PASS / feature マトリクス warning なし。  
付随: `compression_cache_probe` の set -e × curl rc=56 ハーネス修正。
