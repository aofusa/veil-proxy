# B-291: `container-security-nightly` の vendoring 版の勧告照合が無認証で GitHub API を呼び、rate limit で落ちる

**状態: 修正済み**

## 出典

`container-security-nightly` の定期実行（2026-11-04、run 41932864295 の `run.sh (musl)`）。

## 事象

cargo_audit の段階が `error: cannot read advisories for quiche@0.24.9: HTTP Error 403: rate limit exceeded` で失敗し、
`failed phases: cargo_audit` でジョブが落ちた。同じ実行の `run.sh (glibc)` は通った（落ちるかは実行ごとに変わる）。

## 原因

`tools/container_security/security/vendored_ghsa_audit.py`（F-277）は環境変数 `GITHUB_TOKEN` があれば認証して
GitHub の Advisory API を呼ぶ。`release.yml` の監査ステップは `GITHUB_TOKEN` を渡していたが、
`container-security-nightly.yml` の `Run container security suite` は渡しておらず、無認証で呼んでいた。
無認証の上限は IP ごとに 1 時間 60 回で、GitHub の共有ランナーでは他の利用者と分け合うため、利用者の都合で 403 になった。
API を読めないときに失敗にするのは F-277 の方針どおり（照合せずに ok にしない）。

## 修正

- `.github/workflows/container-security-nightly.yml`: `Run container security suite` の env に `GITHUB_TOKEN: ${{ github.token }}`
  を渡す（権限は `contents: read` のまま。公開されている勧告を読むだけ）。
- `vendored_ghsa_audit.py`: レート制限（`Retry-After`、403 + `X-RateLimit-Remaining: 0`、429）のときだけ、GitHub が示す時間
  （最大 120 秒）待って 3 回までやり直す。権限エラー等の 403、待ちが長すぎるとき、回数を使い切ったときは従来どおり失敗にする。

## 関連

- [F-277](../features/F-277-vendored-quiche-advisory-matching.md)
