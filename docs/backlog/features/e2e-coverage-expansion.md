# F-90: E2E テストカバレッジ拡充（e2e_test_coverage.md 対応）

## 概要

`docs/artifacts/e2e_test_coverage.md` で指摘された E2E 網羅ギャップを解消する。

## 改修内容

1. **e2e_setup.sh**
   - 全バイナリ（veil / grpc-server / test-backends）の同期ビルドと ready 確認
   - `config_type=cache` バグ修正（`/cached/*` ルート生成）
   - `security` プロファイル実装（`/rate-limited/*`）
   - `/adaptive/*` ルート追加
   - kTLS 動的有効化（`check_ktls_available`）
   - SNI / 厳密証明書 / TCP・gRPC ヘルスチェック / L4 least_conn・TLS 終端ルート追加

2. **tests/e2e_tests.rs**
   - ギャップ対応 E2E 11 件追加（`test_e2e_*`）

3. **tests/grpc_server**
   - tonic-health 追加（gRPC ヘルスチェック E2E 用）

4. **docs/artifacts/**
   - 必要 / 実装済み / 不足テストケース一覧

## 受け入れ条件

- `./tests/e2e_setup.sh test` で新規テストを含め実行可能
- `e2e_missing_test_cases.md` の「解消済み」項目がすべて実装されていること
- 実装バグによる失敗は bugs/ にチケット化

## 対応状況

進行中（fix/e2e ブランチ）