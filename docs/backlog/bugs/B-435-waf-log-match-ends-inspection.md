# B-435: WAF の例の即時モードが、最初に当たったルールが記録だけ（log）でもそれを返して検査を終え、ブロックのルールを外せた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「WAF immediate mode returns the first match even when it is a Log-only rule, so a benign Log match turns off the rest of the inspection」から確認。

## 事象

- examples/wasm-filters/waf-filter/src/rules.rs の `RuleEngine::inspect` は、即時モード（`anomaly_scoring = false`、既定）でエンコードされたままの値に最初に当たったルールを、動作（`block`・`log`・`allow`）に関係なく返し、デコード後の値と他の検査対象を見なかった。呼び出し側は `block` のときだけ 403 にする。
- CRS レベル 2・3 の `crs-913110`（`python-requests` 等と bot 等の組み合わせ。`log`）に当たる文字列をクエリへ足すだけで（`?x=python-requests-bot&q=%3Cscript%3E...`）、エンコードした XSS・SQLi が上流へ届いた。本文の検査（`inspect_body`）も同じ。

## 対応

- 即時モードは `block` のルールに当たった時点で返し、`log`・`allow` に当たっても残りのルール・デコード後の値・他の対象の検査を続ける（何もブロックしなければ最初の `log`・`allow` の違反を記録のために返す）。デコード前とデコード後の値を常に両方見る（異常スコアの数え方は従来どおり）。README に `log`・`allow` は許可リストではないことを書いた。

## テスト

- `rules::tests::log_match_does_not_end_immediate_inspection`。
