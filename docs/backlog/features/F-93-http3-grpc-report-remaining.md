# F-93: http3_grpc_test_coverage_report 残存ギャップ解消

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md` で指摘された E2E / `tools/container_security`
のうち、F-91・F-92 で未完了だった項目を実装する。

F-91 は別レポート由来の網羅（レート制限・WASM・キャッシュ等）、F-92 はフレーミング詳細・
H2 向け Slowloris/RST・H3 handshake/amplification を対象とした。
本チケットは **レポート 2.1/2.2 の残件** を対象とする。

## 現状（ギャップ）

### E2E（レポート 2.1）

| レポート要求 | 現状 |
|--------------|------|
| `test_http3_request_body_streaming` | 実装済み（F-32） |
| `test_http3_chunked_response_streaming` | 名称不一致。`test_http3_response_body_streaming` 等は有り。レポート名での明示が不足 |
| `test_http3_qpack_compression` | 実装済み（F-92） |
| `test_http3_connection_reuse` | **未実装** |
| `test_http3_connection_migration` | 実装済み（F-92） |
| `test_http3_early_data` | 0-RTT 系はあるがレポート名の専用テストが不足 |
| `test_grpc_over_http3_streaming` | **未実装**（Unary の `test_grpc_over_http3` のみ） |
| `test_grpc_over_http3_metadata` | **未実装** |
| `test_grpc_over_http3_error_handling` | **未実装** |

### container_security（レポート 2.2）

| 要求 | 現状 |
|------|------|
| QUIC 経由の不正 gRPC フレーム | **未実装**（H2C/HTTPS のみ） |
| QUIC 経由のヘッダスプーフィング | **未実装** |
| QUIC gRPC Slowloris（LPM 遅延） | **未実装**（TCP openssl 経路のみ） |
| QUIC ストリーム強制リセット | **未実装** |

## 改修内容

1. E2E: 不足関数を追加（connection_reuse / early_data / chunked_response_streaming 別名、gRPC over H3 詳細 3 件）
2. `Http3TestClient` にチャンク DATA 送信ヘルパを追加（クライアントストリーミング用）
3. `http3-client` に `grpc_malformed` / `grpc_header_spoof` / `grpc_slowloris` / `grpc_stream_reset` モード追加
4. `grpc_probe.sh` から `VEIL_HTTP3_PORT` 向け攻撃を実行
5. 失敗はテスト/実装を切り分け、実装バグは B-* 化して修正

## 改修案

- gRPC over H3: 既存 H2 の `test_grpc_streaming_detailed` / trailer 詳細と同じ protobuf LPM を H3 で送信
- connection_reuse: 同一 `SendRequest` 上で複数独立 GET/POST が成功することを検証
- early_data: セッション確立後の再接続 + 非べき等 POST の安全な扱い（既存 0-RTT セキュリティと統合強化）
- container_security: 攻撃完了 + post-health 200 を合格条件（クラッシュ/ハングなし）

## 受け入れ条件

- [x] レポート 2.1 の未実装テストが実ロジックを持ち E2E で実行される
- [x] `grpc_probe` が HTTP/3(QUIC) 向け攻撃ケースを含む
- [x] 失敗切り分けが `docs/artifacts/` に記録される
- [x] full / default / no-default / 主要 features でビルド警告・エラーなし
- [x] `allow(dead_code)` を理由なしに追加しない

## 依存・リスク

- 真の 0-RTT 送信は quinn セッションチケット保存に依存。未対応時は「安全な失敗」を合格とする
- gRPC ストリーミングは上流 gRPC サーバ（9004）と H3→H2C プロキシ経路に依存
- container_security は Docker イメージ再ビルドが必要

## 対応状況

完了（fix/test）。実装中に **B-41** を検出・修正。  
詳細: `docs/artifacts/f93_failed_tests_report.md`  
検証: E2E 445/445、単体 712、container_security ok、feature ビルド 0 警告
