# F-112: HTTP/3・gRPC キャッシュ/LB/ヘルス/境界 E2E + container_security 網羅

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md`（最新版）が指摘する **現行ギャップ** を埋める。

- **E2E**: HTTP/1.1・HTTP/2 にあるが HTTP/3 / gRPC に無いキャッシュ境界・LB 明示・ヘルス・413/431・圧縮・スマグリング・gRPC ダウングレード拒否・CORS プリフライト等
- **container_security**: libFuzzer の `qpack_decode` / `http3_frame_decode`、0-RTT Anti-Replay 明示プローブ、Pumba 下の HTTP/3 劣化比較

**CI 組み込みは対象外** → [F-113](F-113-http3-grpc-cache-lb-ci.md)。

## 背景

F-91〜F-109 で大半のパリティ・エッジ・DoS・ストリーミングを実装済み。本チケットはレポート最新版 §3 の **未実装リスト** のみを対象とする。

## 対象チェックリスト

### フェーズ1: E2E HTTP/3

| # | テスト名 | 相当（H1/H2） |
|---|----------|---------------|
| 1 | `test_http3_cache_stale_while_revalidate` | `test_stale_while_revalidate` |
| 2 | `test_http3_cache_stale_if_error` | `test_cache_stale_if_error` |
| 3 | `test_http3_cache_invalidation` | `test_cache_invalidation` |
| 4 | `test_http3_load_balancing_least_connections` | `test_least_connections_distribution` |
| 5 | `test_http3_load_balancing_ip_hash` | `test_ip_hash_consistency` |
| 6 | `test_http3_health_check_failover` | `test_health_check_failover` |
| 7 | `test_http3_health_check_recovery` | `test_health_check_recovery` |
| 8 | `test_http3_error_handling_413_payload_too_large` | `test_error_handling_413_payload_too_large` |
| 9 | `test_http3_error_handling_431_request_header_fields_too_large` | `test_error_handling_431_*` / `test_http3_oversized_header` |
| 10 | `test_http3_compression_brotli` | `test_compression_brotli` |
| 11 | `test_http3_compression_zstd` | `test_compression_zstd` |
| 12 | `test_http3_request_smuggling_cl_te_rejected` | `test_request_smuggling_cl_te_rejected`（H3 では CL 重複 / 不正 framing） |

### フェーズ2: E2E gRPC

| # | テスト名 | 内容 |
|---|----------|------|
| 13 | `test_grpc_over_http1_rejected` | Content-Type: application/grpc を HTTP/1.1 で送り 415/426/400 等で拒否 |
| 14 | `test_grpc_load_balancing_least_connections` | least_conn 相当の gRPC 分散生存確認 |
| 15 | `test_grpc_health_check_failover` | ヘルスメトリクス + 健全経路 |
| 16 | `test_grpc_health_check_recovery` | 回復後の生存確認 |
| 17 | `test_grpc_max_concurrent_streams` | H2 上の並行ストリーム耐性 |
| 18 | `test_grpc_over_http3_max_concurrent_streams` | H3 上の並行ストリーム耐性 |
| 19 | `test_grpc_web_cors_preflight` | OPTIONS 単体の CORS プリフライト |

### フェーズ3: container_security

| ID | 対象 | 内容 |
|----|------|------|
| fuzz | `qpack_decode` / `http3_frame_decode` | ワイヤ形式パーサの libFuzzer ターゲット + `run_libfuzzer.sh` 配線 |
| S-H3-21 | `http3_probe.sh` | 0-RTT Anti-Replay（非冪等 POST）明示プローブ |
| pumba-h3 | `pumba_chaos.sh` | netem 下で HTTP/3 スループット・復帰確認 |

### 対象外・文書化

- **L4 UDP パススルー** (`test_l4_udp_passthrough_forward`): F-18 で UDP は将来拡張と明記済みで未実装。本チケットではテストを追加しない（実装時に別チケット）。
- **CI 組み込み**: F-113。

## 改修内容

1. backlog 登録（本チケット + F-113 CI 分離）
2. `src/protocol/http3_wire.rs`（http3 feature）に QPACK 整数/文字列プレフィックス + H3 フレームヘッダの純関数パーサと単体テスト
3. `fuzz/fuzz_targets/{qpack_decode,http3_frame_decode}.rs` + `fuzz/Cargo.toml` + libfuzzer 配線
4. `tests/e2e_tests.rs` に上記 E2E を追加
5. `http3_probe.sh` / `pumba_chaos.sh` 拡充
6. 失敗は `docs/artifacts` に切り分け。実装バグは B-* 化して修正

## 受け入れ条件

- [x] レポート §3 / 実装指示の全項目が実装または明確に文書化（L4 UDP・CI は文書）
- [x] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [x] full / default / no-default / 各 feature でビルド警告・エラーなし
- [x] `allow(dead_code)` を理由なしに追加しない
- [x] CI 組み込みは行わない（F-113）

## 依存・リスク

- H3 の CL/TE スマグリングは HTTP/3 が TE を持たないため、CL 重複・過大 CL・不正 content-length が中心
- least_conn / ip_hash は default 設定では RR の可能性があり、分散/一貫性は「生存 + バックエンド到達」で合格
- 既知フレーキー E2E は修正対象外
- QPACK/H3 フレームの本番経路は quiche 内部。本パーサは信頼境界のワイヤ検査・ファジング用（ホットパス外）
- **ネイティブ gRPC の HTTP/1.1 415 拒否は採用しない**: 製品は H1→H2C gRPC ブリッジを前提とし、`GrpcTestClient`/既存 E2E が依存。判定純関数のみ保持

## 対応状況

完了（fix/test）。  
詳細: `docs/artifacts/f112_failed_tests_report.md`  
検証:
- 単体 737 / 統合 53 / E2E 529 PASS
- feature マトリクス warning 0
- `veil:glibc` ビルド + `tools/container_security/run.sh` exit 0
- fuzz `qpack_decode` / `http3_frame_decode` 各 500 runs（nightly ローカル）
