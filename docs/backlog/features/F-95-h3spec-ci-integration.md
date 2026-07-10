# F-95: h3spec の CI 組み込み（F-94 から分離）

## 概要

HTTP/3 準拠テストツール `h3spec` を CI/CD（GitHub Actions 等）で必須ゲートとして
運用する。**F-94 では Dockerfile インストールと `h3spec_run.sh` ハーネスのみ実装し、
CI ワークフロー配線は本チケットで行う。**

## 現状

- F-94: `tools/container_security/harness` に h3spec バイナリ導入 + `h3spec_run.sh`
- `H3SPEC_STRICT=1` で厳格ゲート可能（ローカル / 手動）
- GHA の `container-security-nightly.yml` 等への常設配線は未着手

## 改修内容（予定）

1. nightly / PR マトリクスで `h3spec` フェーズを実行
2. 失敗時の artifact（レポート・junit）添付
3. フレーキー対策（タイムアウト・リトライ方針）の文書化
4. 必要なら Job Summary へのサマリ出力

## 受け入れ条件

- [ ] CI で h3spec が自動実行される
- [ ] 結果が artifact として残る
- [ ] 失敗時のトリアージ手順が README または本チケットに記載される

## 対応状況

未着手
