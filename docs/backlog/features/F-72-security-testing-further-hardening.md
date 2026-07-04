# F-72: セキュリティテスト追加提案（レポート範囲外の拡充）

出典: `security_chaos_fuzzing_report.md` を踏まえた**追加提案**（レポート未記載）。親: [F-07](fuzzing-chaos-security.md)。

レポート §2 に無いが、今後ファジング・カオス・セキュリティスキャンを高めるうえで
有効と考えられる項目を列挙する（各項目は着手時に個別チケット化する）。

## 提案項目

1. **シークレットスキャン（gitleaks / trufflehog）**
   - 現状 SCA（cargo-audit/deny）・SAST（semgrep, F-64）はあるが、
     鍵・トークンの誤コミット検出が無い。`gitleaks detect` を `security/` に追加し
     PR/nightly で実行。

2. **HTTP リクエストスマグリング専用テスト**
   - ZAP baseline（F-66）は受動スキャン中心。`smuggler` / `h2csmuggler` 等で
     CL.TE / TE.CL / H2C ダウングレードを能動検査（プロキシ特有の高リスク領域）。

3. **プロトコル差分（differential）テスト**
   - 同一リクエストを Veil と nginx/envoy に流し、ステータス・ヘッダー正規化・
     チャンク処理の差分を比較して曖昧な解釈を検出。

4. **OSS-Fuzz 連携の検討**
   - 既存 4+1 ターゲット（F-52/F-70）を OSS-Fuzz に載せ、継続ファジングと
     クラッシュ自動起票を得る（外部インフラ依存のため P3）。

5. **カバレッジ計測の常設化**
   - `cargo llvm-cov` によるファズ/ユニットのカバレッジをレポート集約
     （[F-55](F-55-harness-hardening.md)）に組み込み、盲点を可視化。

6. **回帰コーパスとしての既知クラッシュ固定**
   - B-16/B-17 等の再現入力を `corpus/`・E2E に固定し、修正後の回帰を防ぐ。

## 受け入れ条件

- 各項目のうち着手するものを個別チケット（F-xx）へ分割し、`backlog.md` に反映する。
