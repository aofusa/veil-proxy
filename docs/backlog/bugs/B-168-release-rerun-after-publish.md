# B-168: 公開の後に release の job を再実行すると、draft の job が公開物と合わない署名・証明を作って失敗する

## 出典

v0.9.2 の公開の後（2026-10-25）。release の run（40194475461）で `dist / port netbsd` が VM の停止で時間切れになり、
GitHub Actions の画面から再実行しようとしたところ、`GitHub Release draft` も一緒に再実行されることが分かった
（draft の job は dist の全 job を needs にしている）。

## 事象

公開済みのタグの run で dist の job（試験だけの port の job を含む）を再実行すると、`release-draft` が次を行う:

1. `bsd-ports.py` で ports の tarball を作り直す（tar.gz は時刻を含むので、公開物とハッシュが変わり得る）
2. `SHA256SUMS` を作り直す
3. `cosign sign-blob` で署名する（Sigstore の公開の透明性ログに記録が残る）
4. `attest-build-provenance` で attestation を作る（リポジトリの attestation に残る）
5. `github-draft.sh` が「公開済み」を見て失敗する（公開物は書き換えない）

公開のページは変わらないが、公開していない `SHA256SUMS` の署名と、公開していない tarball の attestation が消せない形で
残り、run は失敗で終わる。試験だけを確かめ直したいときに、画面からの再実行が使えない。

## 修正の方針

- `release-draft` の最初（checkout の後）でリリースの状態を見て、公開済みなら以降の step を飛ばして成功で終える
  （`gh release view "$TAG" --json isDraft`。公開済みなら notice を出して output で後続の step を止める）。
  署名・attestation・成果物の作り直しはどれも、draft を作る・差し替えるときだけ行う。
- `github-draft.sh` の「公開済みなら失敗」は残す（多重の安全網）。
- 文書（packaging/README.md の公開の節）に、公開の後に試験の job だけを再実行してよいことを書く。

## 試験

- 一時的なタグ（または workflow_dispatch の tag）で、公開済みの状態を模して release-draft の判定の step だけを回し、
  署名・attestation の step が skipped になることを確かめる。

## 回避（修正まで）

公開の後に試験だけを確かめ直すときは、画面から再実行せず、その job だけを抜き出した一時的な workflow を一時ブランチで
回す（v0.9.2 の NetBSD の port はそうした）。

## 実施内容（2026-10-25）

- `release.yml` の `release-draft` の checkout の直後に `release state` の step を足し、`gh release view --json isDraft` が
  `false`（公開済み）なら notice を出して output `published=true` にし、以降の全 step（成果物の取得・ports の tarball・
  SHA256SUMS・cosign・attestation・ノート・draft）を `if: steps.state.outputs.published != 'true'` で飛ばす。job は成功で終わる。
- `github-draft.sh` の「公開済みなら失敗」は多重の安全網として残した。
- packaging/README.md の公開の節に、公開の後も試験の job を画面から再実行してよいことを書いた。
- 試験: 次の rc・本番の run で、公開前は従来どおり（published=false）に全 step が走ることを確かめる。公開済みの再実行は v0.10.0 の公開の後に port の job を 1 つ再実行して、署名・attestation の step が skipped になることを確かめる。
