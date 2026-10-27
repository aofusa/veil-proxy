# B-217: container_security の `run.sh` が、失敗したフェーズがあっても終了コード 0 で終わる（CI の偽 OK）

**状態: 修正済み（v0.10.0）**

## 出典

v0.10.0 の PR で手で回した `container-security-nightly`（2026-10-27）。全ジョブが成功で終わったが、`suite_summary.json` では
glibc・musl の `gitleaks`・`cargo_audit`・`cargo_deny`、chaos-extended の `pumba` が failed だった。

## 事象と原因

- `run.sh` は各フェーズを `|| log "...で警告"` で続け、最後に集計（`lib/report.sh`）するだけで、集計の結果を終了コードに反映していなかった。
  このため 2026-10-16 公開の zstd の勧告（B-216）が、毎晩の実行（main）でも 10 日以上ジョブの失敗にならなかった。
- `gitleaks` の 2 件は誤検知（`library_e2e.sh` の Basic 認証の試験の `admin:pw`、Python の ASGI の文書の `forwarded_allow_ips=`）。
- `pumba` の HTTP/3 の確認は、カオスの無いとき（回復の後）も毎回 fail だった。ハーネスのイメージは ENTRYPOINT がフェーズ名を取る
  （`entrypoint.sh`）のに `docker run <image> bash -c '...'` と呼んでおり、`bash` が「不明なフェーズ」で終了 1 → `fail` になっていた
  （F-112 で足したときから一度も確認が走っていない）。

## 修正

- 集計の後、failed のフェーズがあれば `run.sh` を終了コード 1 で終える（README にも書いた）。
- gitleaks の誤検知 2 件を `gitleaks.toml` の allowlist に足した（秘密の文字列そのものに一致する正規表現）。
- `pumba` の HTTP/3 の確認は `--entrypoint bash` で起動し、`VEIL_SNI=veil-proxy` も渡す。netem の最中の結果は参考として
  `result=...` で書き（集計の `: fail` に数えない）、回復の後の結果だけで判定する。
