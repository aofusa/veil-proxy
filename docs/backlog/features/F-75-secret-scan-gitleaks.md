# F-75: シークレットスキャン（gitleaks）

親: [F-72](F-72-security-testing-further-hardening.md) 項目 1 / [F-54](F-54-security-scan-expansion.md)。

## 目的

SCA（cargo-audit/deny）・SAST（semgrep, F-64）を補完し、鍵・トークン・認証情報の
誤コミットを検出する（プロキシは TLS 秘密鍵・上流認証情報を扱うため重要）。

## 実装済み（2026-07-05）

- `tools/container_security/security/run_gitleaks.sh`（docker `zricethezav/gitleaks`）。
  - `GITLEAKS_MODE=dir`（既定、ワークツリー `--no-git`）/ `git`（全履歴）切替。
  - SARIF 出力（`gitleaks_report.sarif`）+ `--redact` で値をマスク。
  - 既定は非ブロッキング（検出は警告としてレポート、`GITLEAKS_BLOCKING=1` で失敗化）。
- `run.sh` フェーズ 4g として配線（`SKIP_GITLEAKS` トグル、`report.sh` に `gitleaks` 追加）。

## 残件

- 検出時のトリアージ方針を SECURITY.md に明記（誤検知の allowlist `.gitleaksignore`）。
- CI（[F-57](F-57-container-security-ci.md)）で PR 時に差分のみ走査。

## 受け入れ条件

- [x] `SKIP_GITLEAKS=0 ./tools/container_security/run.sh` が docker のみで SARIF を出力。
- 検出時のトリアージ結果を backlog に反映（残件）。
