# F-96: HTTP/3・gRPC エッジケース / リソース枯渇テスト網羅

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md` **§5** のチェックリスト
（プロトコルエッジケース + DoS/リソース枯渇）を E2E と
`tools/container_security` に実装する。

F-91〜F-94 は同レポートの基本機能・攻撃・項目1〜11を対象とした。
本チケットは **§5 の未チェック項目**のみを対象とする。

**CI 組み込みは対象外** → [F-95](F-95-h3spec-ci-integration.md) および本チケットから分離する
追加 CI 配線は行わない。

## 対象チェックリスト（レポート §5）

### E2E（§5.1）

| # | 項目 | 内容 |
|---|------|------|
| 1 | PMTU ディスカバリ | 様々なペイロード/UDP サイズでの断片化・巨大データグラム耐性 |
| 2 | CID 更新・リタイア | 接続再確立（新 CID）後のセッション健全性 |
| 3 | QUIC Keep-Alive | 長時間アイドル後もタイムアウトせずリクエスト可能 |
| 4 | GOAWAY / Graceful Shutdown | リロード/切断後の既存ストリーム・新規接続の安全な扱い |
| 5 | gRPC リトライと Hedging | 連続リトライ + 並行投機リクエストの透過制御 |
| 6 | gRPC Keepalive (PING) | HTTP/2 PING による長寿命接続維持 |
| 7 | サーバ起因ストリーム異常終了 | RST/エラー時の gRPC ステータス伝播 |

### container_security（§5.2）

| # | 項目 | 内容 |
|---|------|------|
| 8 | UDP リフレクション/増幅（IP スプーフィング耐性） | Initial 連打 + 応答肥大化が 3 倍制限内 / 生存 |
| 9 | MAX_STREAMS 枯渇 | データ無しストリーム大量オープン |
| 10 | Connection Migration スプーフィング | 別ソースポートからの同一 CID 風パケット |
| 11 | QPACK 動的テーブル非同期参照 | 順序逆転/未着参照ヘッダ攻撃 |
| 12 | HPACK ヘッダ展開ボム（gRPC） | 高圧縮相当の巨大ヘッダブロック |
| 13 | MAX_CONCURRENT_STREAMS 違反 | 上限超過の gRPC 多重化 |
| 14 | Half-closed ストリーム保持 | 強化: ヘッダのみ・ボディ未完の長期保持 |
| 15 | 悪意のある Trailers 挿入 | ボディ完了前の Trailers 送信 |

## 改修内容

1. E2E 7 件を `tests/e2e_tests.rs` に追加
2. `Http2TestClient` に PING ヘルパを追加（Keepalive 検証）
3. `http3-client` に攻撃モード追加 + `http3_probe.sh` / `grpc_probe.sh` 配線
4. 失敗は artifacts に切り分け。実装バグは B-* 化して修正
5. 一時成果物は gitignore / `docs/artifacts` 維持

## 受け入れ条件

- [ ] レポート §5 の 15 項目が実装または明確に文書化（クライアント制約による近似含む）
- [ ] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [ ] full / default / no-default / 各 feature でビルド警告・エラーなし
- [ ] `allow(dead_code)` を理由なしに追加しない
- [ ] CI 組み込みは行わない（F-95 等）

## 依存・リスク

- 真の Path MTU Discovery / 真の CID retire / 真の IP spoof は特権・クライアント制約で近似検証になる
- gRPC hedging はクライアント側ポリシーの近似（並行同一 RPC）
- container_security は Docker イメージ再ビルドが必要

## 対応状況

進行中（fix/test）
