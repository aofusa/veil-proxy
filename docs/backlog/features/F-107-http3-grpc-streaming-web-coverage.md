# F-107: gRPC over HTTP/3 ストリーミング細分化・圧縮・gRPC-Web と H3 攻撃プローブ残件

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md`（最新）が指摘する **残ギャップ** を埋める。

- **E2E**: 集約済みの `test_grpc_over_http3_streaming` / `compression` / `test_grpc_web_over_http3` を HTTP/2 相当の **個別テスト** に細分化
- **container_security**: H2 側にある巨大メタデータ・RST フラッド・gRPC-Web 不正 Base64 / 巨大メタデータを HTTP/3 へ移植

**CI 組み込みは対象外** → [F-108](F-108-http3-grpc-streaming-web-ci.md)。

## 背景

F-91〜F-103 で gRPC over H3 のエッジ・DoS・異常系の大半は実装済み。本チケットはレポート Task 1/2 の **未実装リストのみ** を対象とする。

## 対象チェックリスト

### フェーズ1: E2E ストリーミング細分化

| # | テスト名 | H2 相当 |
|---|----------|---------|
| 1 | `test_grpc_over_http3_client_streaming` | `test_grpc_client_streaming` |
| 2 | `test_grpc_over_http3_server_streaming` | `test_grpc_server_streaming` |
| 3 | `test_grpc_over_http3_bidirectional_streaming` | `test_grpc_bidirectional_streaming` |

### フェーズ2: E2E 圧縮・エンコーディング

| # | テスト名 | H2 相当 |
|---|----------|---------|
| 4 | `test_grpc_over_http3_gzip_compression` | `test_grpc_gzip_compression` |
| 5 | `test_grpc_over_http3_deflate_compression` | `test_grpc_deflate_compression` |
| 6 | `test_grpc_over_http3_compression_negotiation` | `test_grpc_compression_negotiation` |
| 7 | `test_grpc_over_http3_encoding_header` | `test_grpc_encoding_header` |
| 8 | `test_grpc_over_http3_accept_encoding_header` | `test_grpc_accept_encoding_header` |

### フェーズ3: E2E gRPC-Web over HTTP/3

| # | テスト名 | H2 相当 |
|---|----------|---------|
| 9 | `test_grpc_web_over_http3_binary_format` | `test_grpc_web_binary_format` |
| 10 | `test_grpc_web_over_http3_text_format` | `test_grpc_web_text_format` |
| 11 | `test_grpc_web_over_http3_cors_headers` | `test_grpc_web_cors_headers` |

### フェーズ4: container_security

| ID | 内容 | 配線先 |
|----|------|--------|
| S-G-H3-14 | `h3_grpc_oversized_metadata`（巨大 grpc-timeout） | `grpc_probe.sh` + `http3-client` |
| S-G-H3-15 | `h3_grpc_rst_flood`（短命ストリーム大量生成・即リセット） | `grpc_probe.sh` + `http3-client` |
| S-G-H3-16 | `h3_grpc_web_text_invalid_b64` | `grpc_web_probe.sh` + `http3-client` / curl |
| S-G-H3-17 | `h3_grpc_web_oversized_metadata` | `grpc_web_probe.sh` + `http3-client` / curl |

## 改修内容

1. backlog 登録（本チケット + F-108 CI 分離）
2. `tests/e2e_tests.rs` に上記 E2E を追加
3. `http3-client` に攻撃モード追加 + `grpc_probe.sh` / `grpc_web_probe.sh` 配線
4. 失敗は `docs/artifacts` に切り分け。実装バグは B-* 化して修正
5. 一時成果物は gitignore / `docs/artifacts` 維持

## 受け入れ条件

- [ ] レポート Task 1/2 の全項目が実装または明確に文書化
- [ ] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [ ] full / default / no-default / 各 feature でビルド警告・エラーなし
- [ ] `allow(dead_code)` を理由なしに追加しない
- [ ] CI 組み込みは行わない（F-108）

## 依存・リスク

- 圧縮はプロキシ透過のネゴシエーション検証が主（バックエンドが gzip 応答しない場合は identity 受理で合格）
- RST flood はリソース枯渇耐性（post-health 200）が合格条件
- 既知フレーキー E2E は修正対象外

## 対応状況

進行中（fix/test）。
