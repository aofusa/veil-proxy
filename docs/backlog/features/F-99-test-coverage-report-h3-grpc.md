# F-99: test_coverage_report — gRPC over HTTP/3 E2E と container_security 拡充

## 概要

`docs/artifacts/test_coverage_report.md` が指摘するギャップを埋める。

- E2E: gRPC over HTTP/3 のエッジケース 5 件 + HTTP/3 メトリクス 2 件
- `tools/container_security`: gRPC over HTTP/3 攻撃 4 件 + gRPC-Web over HTTP/3 2 件

**CI 組み込みは対象外** → [F-100](F-100-test-coverage-report-ci.md)。

## 対象チェックリスト（レポート §3）

### 提案1: E2E gRPC over HTTP/3 エッジケース

| # | テスト名 | 内容 |
|---|----------|------|
| 1 | `test_grpc_over_http3_timeout_header` | `grpc-timeout` 処理 |
| 2 | `test_grpc_over_http3_compression` | gzip / deflate 等の圧縮 |
| 3 | `test_grpc_over_http3_trailer_detailed` | 複雑な Trailers（正常/エラー） |
| 4 | `test_grpc_over_http3_proxy_load_balancing` | ロードバランシングとリトライ |
| 5 | `test_grpc_web_over_http3` | gRPC-Web / gRPC-Web-Text |

### 提案2: E2E HTTP/3 メトリクス

| # | テスト名 | 内容 |
|---|----------|------|
| 6 | `test_http3_prometheus_metrics` | HTTP/3 接続・ストリームの Prometheus 計上 |
| 7 | `test_http3_active_connections_metric` | 切断・タイムアウト時のメトリクス減少 |

### 提案3: `grpc_probe.sh` gRPC over HTTP/3 攻撃

| # | ID | 内容 |
|---|-----|------|
| 8 | `h3_grpc_qpack_bomb` | QPACK 動的テーブル枯渇 |
| 9 | `h3_grpc_max_concurrent_streams` | MAX_CONCURRENT_STREAMS バースト |
| 10 | `h3_grpc_half_closed_body_hold` | ヘッダ後ボディ未送の長時間保持 |
| 11 | `h3_grpc_malicious_trailers` | 不正タイミング Trailer |

### 提案4: `grpc_web_probe.sh` gRPC-Web over HTTP/3

| # | ID | 内容 |
|---|-----|------|
| 12 | `h3_grpc_web_malformed_body` | 不正ペイロード（5 バイト未満） |
| 13 | `h3_grpc_web_large_base64_dos` | 巨大 Base64 DOS（curl `--http3-only` 等） |

## 改修内容

1. backlog 登録（本チケット + F-100 CI 分離）
2. `tests/e2e_tests.rs` に 7 件追加
3. HTTP/3 メトリクスが未配線なら `metrics` + `http3_server` に最小実装（ホットパス規則遵守）
4. `http3-client` に gRPC 攻撃モード追加し `grpc_probe.sh` / `grpc_web_probe.sh` 配線
5. 失敗は `docs/artifacts` に切り分け。実装バグは B-* 化して修正
6. 一時成果物は gitignore / `docs/artifacts` 維持

## 受け入れ条件

- [ ] レポート §3 の 13 項目が実装または明確に文書化
- [ ] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [ ] full / default / no-default / 各 feature でビルド警告・エラーなし
- [ ] `allow(dead_code)` を理由なしに追加しない
- [ ] CI 組み込みは行わない（F-100）

## 依存・リスク

- 真の QPACK 動的テーブル枯渇は quiche 内部依存のため近似検証
- gRPC-Web over H3 は curl `--http3` または http3-client 依存
- container_security は Docker イメージ再ビルドが必要
- 既知フレーキー E2E は修正対象外

## 対応状況

進行中（fix/test）。
