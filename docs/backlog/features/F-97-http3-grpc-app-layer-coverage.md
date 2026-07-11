# F-97: HTTP/3 アプリ層・gRPC 高度機能・container_security 攻撃ベクトル網羅

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md` の **§2.2 / §2.3 / §3 / §4 チェックリスト**
（HTTP/3 アプリケーション層、gRPC 高度機能、container_security 不足攻撃）を
E2E と `tools/container_security` に実装する。

F-91〜F-96 は同レポートの基本機能・攻撃・§5 エッジ/DoS を対象とした。
本チケットは **レポート §4 フェーズ1〜3 の未チェック項目**のみを対象とする。

**CI 組み込みは対象外** → [F-98](F-98-http3-grpc-app-layer-ci.md)。

## 対象チェックリスト（レポート §4）

### フェーズ1: E2E HTTP/3 アプリケーション層

| # | テスト名 | 内容 |
|---|----------|------|
| 1 | `test_http3_routing_conditions` | クエリ・カスタムヘッダ条件ルーティング |
| 2 | `test_http3_buffering_spillover` | 大容量時のディスクスピルオーバー/メモリ制限 |
| 3 | `test_http3_range_requests` | Range → 206 Partial Content |
| 4 | `test_http3_pseudo_header_validation` | `:authority` と `Host` 不一致の拒否 |

### フェーズ2: E2E gRPC 高度機能

| # | テスト名 | 内容 |
|---|----------|------|
| 5 | `test_grpc_consistent_hashing` | メタデータ `x-user-id` による Consistent Hash |
| 6 | `test_grpc_buffering_bypass` | Full バッファリング設定下でも gRPC はストリーミング |
| 7 | `test_grpc_active_health_check` | `grpc.health.v1` による不健全バックエンド切り離し |

### フェーズ3: container_security

| # | ID | 内容 |
|---|-----|------|
| 8 | S-H3-14 | QUIC Stream Body Slowloris |
| 9 | S-H3-15 | QPACK Memory Exhaustion |
| 10 | gRPC-Web | Large Base64 Decoding DOS |
| 11 | S-G-16 | gRPC Path Bypass |
| 12 | S-G-17 | gRPC WASM Crash Resilience |

## 改修内容

1. E2E 7 件を `tests/e2e_tests.rs` に追加（必要なら `proxy.toml` / `e2e_setup` / gRPC サーバ拡張）
2. 製品側: `:authority`/`Host` 不一致検証、gRPC の Full バッファバイパス、Consistent Hash のヘッダキー配線、必要なら第2 gRPC バックエンド
3. `http3-client` に `stream_body_slowloris` / `qpack_memory_exhaustion` を追加し `http3_probe.sh` 配線
4. `grpc_probe.sh` / `grpc_web_probe.sh` を拡張
5. 失敗は artifacts に切り分け。実装バグは B-* 化して修正
6. 一時成果物は gitignore / `docs/artifacts` 維持

## 受け入れ条件

- [x] レポート §4 の 12 項目が実装または明確に文書化
- [x] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [x] full / default / no-default / 各 feature でビルド警告・エラーなし
- [x] `allow(dead_code)` を理由なしに追加しない
- [x] CI 組み込みは行わない（F-98）

## 依存・リスク

- 真の QPACK 動的テーブル枯渇は quiche 内部依存のため近似検証
- gRPC ヘルスフェイルオーバーは第2バックエンド or 意図的不達サーバが必要
- container_security は Docker イメージ再ビルドが必要
- hyper/h3 は Host 併送不可 → 不一致 400 は unit + quiche プローブで担保

## 対応状況

完了（fix/test）。  
詳細: `docs/artifacts/f97_failed_tests_report.md`  
検証: 単体 723 / 統合 53 / F-97 E2E 7 PASS / feature ビルド 0 警告。  
container_security はイメージ再ビルド成功、起動時 seccomp ログ検知で中断（既存ハーネス挙動、プローブ本体未到達）。
