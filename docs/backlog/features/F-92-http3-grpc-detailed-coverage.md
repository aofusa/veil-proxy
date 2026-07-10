# F-92: HTTP/3・gRPC 詳細 E2E / container_security 拡充

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md` で指摘された、E2E の「未実装テスト」スタブと
`tools/container_security` の gRPC ストリーミング攻撃・HTTP/3 プロトコル違反系ギャップを解消する。

F-91 は別レポート（`test_coverage_report.md`）由来の網羅ギャップ（レート制限・WASM・キャッシュ等）を対象とした。
本チケットは **フレーミング詳細・トレーラー詳細・QPACK/接続マイグレーション・DoS プローブ** を対象とする。

## 現状

- E2E: 以下が `// 未実装テスト:` コメント付きの薄いスタブ／基本疎通のみ
  - `test_grpc_http2_framing`
  - `test_grpc_streaming_detailed`
  - `test_grpc_trailer_detailed`
  - `test_http3_qpack_compression`
  - `test_http3_connection_migration`
- container_security:
  - gRPC: Slowloris（LPM 遅延送信）・RST_STREAM flood が不足
  - HTTP/3: handshake timeout / amplification 防止検証・h3spec 相当が不足

## 改修内容

1. E2E 5 関数をレポート 2.1 の仕様どおり詳細実装
2. `Http2TestClient` に chunked DATA 送信・トレーラー取得を追加（フレーミング検証用）
3. `grpc_probe.sh` に gRPC Slowloris / RST_STREAM flood を追加
4. `http3-client` + `http3_probe.sh` に handshake slowloris / amplification チェックを追加
5. 失敗はテスト/実装を切り分け、実装バグは B-* チケット化して修正

## 改修案

- gRPC: h2 クライアントで LPM を複数 DATA フレームに分割送信し、境界ずれを検証
- streaming: 複数メッセージ交互・途中切断後のプロキシ生存を検証
- trailer: 正常（Unary）とエラー（StreamReset）で `grpc-status` / `grpc-message` を検証
- QPACK: 同一接続で同一ヘッダ連打 + 巨大ヘッダの制御された拒否
- connection migration: 別ソースポート接続 + 同一接続継続の健全性（真の path migration 未対応時は安全失敗）
- container_security: crash/hang なし + post health を合格条件

## 受け入れ条件

- [x] レポート記載の E2E 5 テストが実ロジックを持ち、E2E 環境で実行される
- [x] `grpc_probe` / `http3_probe` に新規攻撃ケースが含まれる
- [x] 失敗切り分けが `docs/artifacts/` に記録される
- [x] full / default / no-default / 主要 features でビルド警告・エラーなし

## 依存・リスク

- 真の QUIC connection migration はクライアント（quinn）制約で完全再現が難しい場合あり
- gRPC streaming はプロキシ透過性に依存。上流 gRPC サーバ（9004）が必要
- container_security は Docker イメージ再ビルドが必要

## 対応状況

完了（fix/test）。B-40 製品修正込み。詳細は `docs/artifacts/f92_failed_tests_report.md`
