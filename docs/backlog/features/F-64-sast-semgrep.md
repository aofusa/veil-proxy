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

## 実装済み（カスタムルール・2026-07-05）

- `.semgrep/veil-rules.yml`（generic `pattern-regex`、対象 `src/` 限定）に **誤検知が極小の
  高信号ルール**を整備:
  - `veil-no-static-lifetime-transmute`（ERROR）: ライフタイムを `'static` へ延命する
    transmute（B-16 の RefCell 借用 UAF panic と同種）。現状 src/ 0 件＝新規混入の番人。
  - `veil-no-bare-allow-dead-code`（WARNING）: 素の `#[allow(dead_code)]`（AGENTS.md 禁止事項。
    cfg 条件つき `#[cfg_attr(..., allow(dead_code))]` は許容＝regex で除外、既存 3 件ベースライン）。
- `run_semgrep.sh` に `--config /src/.semgrep`（`SEMGREP_CUSTOM_RULES=0` で無効化可能）を配線。
- **設計判断**: `block_on`/`thread::sleep`/`std::net` の一律検出は、テスト/起動配線/offload
  専用スレッド等の正当用途（実測 block_on 49・sleep 11 箇所の大半）を単純 regex では
  区別できずノイズになるため**あえて含めない**（F-64 目的「意味のある差分だけ」）。
  `#[cfg(test)]` スコープを解する AST ベース検査が要るため [F-79] で対応する。

## 残件

- CI（[F-57](F-57-container-security-ci.md)）で PR 時にベースライン差分のみ失敗させる
  仕組み（現状は nightly で全件レポート。差分ゲートは semgrep `--baseline-commit` 利用で別途）。

## 受け入れ条件

- `SKIP_SEMGREP=0 ./tools/container_security/run.sh` が docker のみで完結し SARIF を出力。
- 新規 finding のトリアージ方針を SECURITY.md/README に記載。
