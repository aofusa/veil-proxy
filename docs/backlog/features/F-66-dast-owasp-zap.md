# F-66: 高度な DAST（OWASP ZAP Baseline）導入

出典: `security_chaos_fuzzing_report.md` §2.3.1。親: [F-54](F-54-security-scan-expansion.md)。

## 目的

bash ベースの `security_scan.sh`（メソッド制限・TRACE・パストラバーサル）を補完し、
専用 DAST ツールで受動スキャン + 既知のプロキシ/HTTP 脆弱性パターンを網羅検査する。

## 実装済み

- `tools/container_security/security/run_zap.sh`（docker `ghcr.io/zaproxy/zaproxy`、
  `zap-baseline.py`、稼働中 Veil コンテナ IP を解決してターゲット化、
  HTML/XML レポート出力、非ブロッキング）。
- `run.sh` フェーズ 4f として配線（`SKIP_ZAP` トグル、`report.sh` に `zap` 追加）。

## 残件

- **HTTP リクエストスマグリング**（CL.TE / TE.CL）の能動テストは ZAP baseline では
  カバーされない。`smuggler`/`h2csmuggler` 等の専用ツール、または Nuclei テンプレートの追加。
- Active Scan（攻撃的スキャン）の別トグル化と対象 URL 群の定義。

## 受け入れ条件

- `SKIP_ZAP=0 ./tools/container_security/run.sh` が docker のみで baseline を実行し、
  レポートを `results/zap_baseline_report.{html,xml}` に出力する。
- 検出項目のトリアージ結果を backlog に反映（誤検知/実問題の切り分け）。
