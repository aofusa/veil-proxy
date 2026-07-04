# F-64: SAST（semgrep）導入

出典: `docs/artifacts/security_chaos_fuzzing_report.md` §2.3.3。親: [F-54](F-54-security-scan-expansion.md)。

## 目的

`cargo-audit`（依存関係のみ）を補完し、自作コード（特に `src/runtime/`・`unsafe` 周辺）の
危険パターンをコードレベルで静的検知する。

## 実装済み

- `tools/container_security/security/run_semgrep.sh`（docker `semgrep/semgrep`、
  ルールセット `p/rust` + `p/security-audit`、SARIF 出力、既定は非ブロッキング）。
- `run.sh` フェーズ 4d として配線（`SKIP_SEMGREP` トグル、`report.sh` に `semgrep` 追加）。

## 実行結果（初回・修正はしない）

- 検出 233 件、すべて `unsafe-usage`（severity warning）。injection/crypto/path-traversal
  等の検出は 0。unsafe 分布: runtime 77 / udp 41 / security.rs 30 / ktls_rustls 18 /
  system 14 ほか（計 19 ファイル）。
- 所見: `unsafe` は AGENTS.md で許容（最小限＋不変条件明記）されており、
  汎用ルールは本コードベースでは `unsafe` を一律に警告するノイズが主。
  詳細は `docs/artifacts/security_chaos_fuzzing_report_findings.md`。

## 残件

- `unsafe` 一律警告ではなく、**io_uring/ゼロコピー固有の不変条件**を検査する
  カスタム semgrep ルール（`.semgrep/` 配下）を整備し、意味のある差分だけを拾う。
- CI（[F-57](F-57-container-security-ci.md)）で PR 時にベースライン差分のみ失敗させる。

## 受け入れ条件

- `SKIP_SEMGREP=0 ./tools/container_security/run.sh` が docker のみで完結し SARIF を出力。
- 新規 finding のトリアージ方針を SECURITY.md/README に記載。
