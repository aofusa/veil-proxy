# F-91: HTTP/3・gRPC テスト網羅（E2E + container_security）

## 概要

`docs/artifacts/test_coverage_report.md` で指摘された HTTP/3 / gRPC の網羅ギャップを解消する。  
必要ケース正本: `docs/artifacts/required_test_cases.md`  
不足一覧: `docs/artifacts/missing_test_cases.md`

## 現状

- HTTP/3 E2E: 基本・ストリーム・LB・圧縮等は存在。**レート制限 / IP 制限 / WASM / キャッシュ / 0-RTT セキュリティ / WS-over-H3 が欠落**
- gRPC E2E: Unary/Streaming/Web は存在。**HTTP/3 上 gRPC・client slowloris・H2 framing 厳密化が欠落**
- container_security: `http3_probe` / `grpc_probe` は疎通中心。**QUIC 攻撃系・gRPC スプーフィング/巨大メッセージ/無限ストリームが欠落**

## 改修内容

1. E2E に E-H3-10〜15、E-G-05+、E-G-06、E-G-07 を追加
2. `http3_probe.sh` / `grpc_probe.sh` に S-H3-02〜05、S-G-05〜07 を追加
3. 必要に応じ `http3_client` / harness `http3-client` を拡張
4. 実行結果で失敗した実装バグは **bugs チケットのみ**（本チケットでは製品コード修正しない）

## 改修案

- 既存ルート（`/rate-limited/*`、`/api/ip-restricted/*`、`/wasm/*`、`/cached/*`、gRPC upstream）を再利用
- H3 クライアントは h3+quinn（E2E）、quiche（container_security）
- 攻撃系プローブは crash/hang なし + post health を合格条件とする

## 受け入れ条件

- [ ] `missing_test_cases.md` の全 ID が実装されている
- [ ] `./tests/e2e_setup.sh test` で新規テストが実行される
- [ ] `tools/container_security/run.sh`（または full_features プローブ）で新規プローブが動く
- [ ] 失敗の切り分け結果が `docs/artifacts/` に記録され、実装バグは bugs 化済み

## 依存・リスク

- HTTP/3 経路で cache/WASM ヘッダ変更が未配線の場合、テスト失敗 → バグチケット
- WebSocket over H3 未実装の可能性 → 安全な非対応応答を期待し、クラッシュ時のみバグ
- container_security は Docker / イメージビルド依存

## 対応状況

進行中（fix/test ブランチ）
