# F-95: h3spec の CI 組み込み（F-94 から分離）

## 概要

HTTP/3 準拠テストツール `h3spec` を CI/CD（GitHub Actions 等）で必須ゲートとして
運用する。**F-94 では Dockerfile インストールと `h3spec_run.sh` ハーネスのみ実装し、
CI ワークフロー配線は本チケットで行う。**

## 現状

- F-94: `tools/container_security/harness` に h3spec バイナリ導入 + `h3spec_run.sh`
- `H3SPEC_STRICT=1` で厳格ゲート可能（ローカル / 手動）
- GHA の `container-security-nightly.yml` 等への常設配線は 2026-10-05 に完了（`suite` ジョブで `H3SPEC_REQUIRED=1`）

## 改修内容

1. nightly / PR マトリクスで `h3spec` フェーズを実行
2. 失敗時の artifact（レポート・junit）添付
3. フレーキー対策（タイムアウト・リトライ方針）の文書化
4. 必要なら Job Summary へのサマリ出力

## 受け入れ条件

- [ ] CI で h3spec が自動実行される
- [ ] 結果が artifact として残る
- [ ] 失敗時のトリアージ手順が README または本チケットに記載される

## 対応状況

完了（末尾「対応結果」参照）。

## 対応結果（2026-10-05、完了）

`suite` ジョブで `SKIP_H3SPEC=0` + `H3SPEC_REQUIRED=1`（バイナリ未導入を失敗扱い）で毎日実行し、`h3spec_report.txt` を artifact と Job Summary に出す。**`H3SPEC_STRICT`/`H3SPEC_REQUIRED` が `lib/common.sh` からハーネスコンテナへ渡っておらず、run.sh から有効化できなかった不具合**も修正した。トリアージは `h3spec_report.txt` の失敗ケース名で RFC 9114/9204 の該当節を確認し、`H3SPEC_STRICT=1` でローカル再現する。

ワークフローは GitHub Actions 上でしか実行できないため、ローカルでは YAML の構文検証と、
同じ環境変数での `tools/container_security/run.sh` の実行（v0.7.0 リリース前検証）で確認した。
