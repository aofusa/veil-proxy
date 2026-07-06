# F-83: Nuclei DAST と Landlock 違反の意図的発火テスト

親: [F-54](F-54-security-scan-expansion.md)（セキュリティスキャン拡充）から分離。

## 背景

F-54 の受け入れ条件（TLS・依存関係スキャンが docker のみで完結、HIGH/CRITICAL 対応フロー記載）は
達成済み。改修内容に挙げていた項目のうち以下 2 つは外部ツール／専用環境を要するため本チケットへ分離した。

- **seccomp 違反の意図的発火テスト**は F-54 で実装済み
  （`src/security.rs::test_seccomp_denies_disallowed_syscall`：許可リスト外 syscall が
  Filter=EPERM / Strict=SIGSYS 即死になることを子プロセスで実機検証）。

## 目的（残件）

1. **Nuclei（DAST テンプレートスキャン）** の `tools/container_security/` への追加・配線。
   ZAP Baseline（[F-66](F-66-dast-owasp-zap.md)）と相補的に、CVE/誤設定テンプレートで
   稼働中 Veil を走査する。
2. **Landlock 違反の意図的発火**コンテナテスト。Landlock 有効時に許可外パスへの write が
   `EACCES` で拒否されることを、専用コンテナ内で実際に write を試みて検証する
   （seccomp 側の発火テストと対になる FS サンドボックスの発火確認）。

## 改修案

- `tools/container_security/security/run_nuclei.sh`（`projectdiscovery/nuclei` イメージ、
  SARIF 出力、非ブロッキング）を追加し `run.sh` フェーズへ配線。
- Landlock 発火は、`apply_security_restrictions` を適用した小さなヘルパープロセスを
  コンテナ内で起動し、`landlock_read_paths` 配下への write が拒否・`landlock_write_paths`
  配下への write が許可されることを検査する。

## 受け入れ条件

- `run.sh` から Nuclei が docker のみで実行でき、結果が `results/` に保存される。
- Landlock 発火テストが、許可外 write の `EACCES` 拒否と許可 write の成功を確認する。
