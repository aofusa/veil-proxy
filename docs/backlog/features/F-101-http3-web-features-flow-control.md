# F-101: HTTP/3 基本 Web 機能 E2E と QUIC フロー制御プローブ

## 概要

`docs/artifacts/http3_grpc_test_coverage_report.md` が指摘する残ギャップを埋める。

- **E2E**: HTTP/3 経路での静的ファイル・リダイレクト・SNI/証明書リロード中の生存・巨大ヘッダ・Alt-Svc アップグレードフロー
- **container_security**: QUIC `MAX_DATA` / `MAX_STREAM_DATA` フロー制御違反プローブ

**CI 組み込みは対象外** → [F-102](F-102-http3-web-features-ci.md)。

## 対象チェックリスト（レポート §3）

### 提案1: E2E HTTP/3 基本 Web 機能

| # | テスト名 | 内容 |
|---|----------|------|
| 1 | `test_http3_static_file_large` | HTTP/3 経由の大容量静的ファイル |
| 2 | `test_http3_static_file_etag` | HTTP/3 経由の ETag（任意ヘッダ）検証 |
| 3 | `test_http3_redirect_302` | HTTP/3 での 302 + Location |
| 4 | `test_http3_redirect_307` | HTTP/3 での 307 |
| 5 | `test_http3_sni_and_cert_reload` | H3 SNI 接続 + 証明書 SIGHUP 中の H3 生存 |
| 6 | `test_http3_oversized_header` | QPACK 巨大ヘッダの拒否（431 相当 or 接続切断） |

### 提案2: Alt-Svc トランジション

| # | テスト名 | 内容 |
|---|----------|------|
| 7 | `test_alt_svc_upgrade_flow` | H1/H2 で Alt-Svc を受け取り H3 へ接続切替をシミュレーション |

### 提案3: container_security フロー制御

| # | ID | 内容 |
|---|-----|------|
| 8 | `h3_flow_control_violation` (S-H3-17) | ウィンドウを超える大量 STREAM 送信刺激。サーバ crash なし + 接続制御 |

## 改修内容

1. backlog 登録（本チケット + F-102 CI 分離）
2. `tests/e2e_tests.rs` に上記 E2E を追加
3. HTTP/3 リクエストヘッダサイズ上限（`MAX_HEADER_SIZE`）を enforce（431）— H1 と同等の防御
4. `http3-client` に `flow_control_violation` モード + `http3_probe.sh` 配線
5. 失敗は `docs/artifacts` に切り分け。実装バグは B-* 化して修正
6. 一時成果物は gitignore / `docs/artifacts` 維持

## 受け入れ条件

- [ ] レポート §3 の 8 項目が実装または明確に文書化
- [ ] E2E / container_security / 単体が実行され、失敗は artifacts に切り分け
- [ ] full / default / no-default / 各 feature でビルド警告・エラーなし
- [ ] `allow(dead_code)` を理由なしに追加しない
- [ ] CI 組み込みは行わない（F-102）

## 依存・リスク

- 真の FLOW_CONTROL_ERROR フレーム偽造は quiche が send 側で FC を強制するため近似検証（大量送信 + 生存確認）
- HTTP/3 証明書は起動時 memfd 固定。SIGHUP の TLS リロードは TCP 側。H3 はリロード中も生存・接続継続を検証（完全な H3 証明書ホットリロードは別課題）
- container_security は Docker イメージ再ビルドが必要
- 既知フレーキー E2E は修正対象外

## 対応状況

進行中（fix/test）。
