# F-94: http3_grpc_test_coverage_report 項目 1〜11 の実装

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md` セクション 2.1 / 2.2 のアクションアイテム
（1〜11）のうち、F-91〜F-93 で未完了だったギャップを解消する。

**CI 組み込み（h3spec の GHA 必須化等）は対象外** → [F-95](F-95-h3spec-ci-integration.md)。

## 対象チェックリスト

### E2E（2.1）

| # | 項目 | 内容 |
|---|------|------|
| 1 | Alt-Svc | HTTP/1.1・HTTP/2 応答で `Alt-Svc: h3="..."` 広告の検証。製品側に広告機能が無い場合は実装する |
| 2 | UDP 到達不能フォールバック | UDP 利用不可時に H2/H1.1 で処理継続できることのシミュレーション |
| 3 | gRPC フロー制御境界 | WINDOW_UPDATE 枯渇・極端制御下での大量ストリーム耐性 |
| 4 | gRPC + WASM | LPM 経路での WASM インターセプタ E2E |

### container_security（2.2）

| # | 項目 | 内容 |
|---|------|------|
| 5 | h3spec | Dockerfile へのインストール + `h3spec_run.sh`（`H3SPEC_STRICT`）。**CI 配線は F-95** |
| 6 | Amplification 厳格化 | RFC 9000 の 3 倍制限を計量・アサート |
| 7 | 0-RTT リプレイ | 非冪等 POST の Early Data リプレイ耐性プローブ |
| 8 | Fragmented LPM | 1 バイト分割 LPM 攻撃 |
| 9 | Half-closed 悪用 | half-close 後の読み捨て放置によるリソース耐性 |
| 10 | fuzz `grpc_frame_decode` | LPM デコーダの libFuzzer ターゲット |
| 11 | fuzz QPACK/H3 | 内部 QPACK 無し（quiche 依存）。代替として gRPC framing に加え公開境界を強化。H3 フレームは quiche 内部のため文書化 |

## 改修内容

1. Alt-Svc 製品機能（`http3_enabled` 時にグローバル広告、ホットパスゼロコピー Guard）
2. E2E 4 件追加
3. `h3spec_run.sh` + Dockerfile インストール + `run.sh` / `http3_probe` 連携
4. amplification 厳格アサート、0-RTT replay / fragmented LPM / half-closed プローブ
5. `fuzz/fuzz_targets/grpc_frame_decode.rs` + libfuzzer 配線

## 受け入れ条件

- [x] レポート 1〜11 が実装または明確に文書化（11 の quiche 内部は文書）
- [x] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [x] full / default / no-default / 各 feature でビルド警告・エラーなし
- [x] `allow(dead_code)` を理由なしに追加しない
- [x] CI 組み込みは F-95 のみ（本チケットでは行わない）

## 対応状況

完了（fix/test）。  
詳細: `docs/artifacts/f94_failed_tests_report.md`  
検証: 単体 717 / 統合 53 / E2E 448+1 フレーキー / container_security ok / feature ビルド 0 警告
