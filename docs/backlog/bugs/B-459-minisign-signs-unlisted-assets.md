# B-459: リリースの minisign の署名が、SHA256SUMS に載っていないアーカイブにも署名していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「The release minisign key signs any .tar.gz/.zip on the release, including archives the cosign-signed SHA256SUMS does
not list」から確認。

## 事象

- `tools/release/minisign-assets.sh`（`release-downstream.yml` の minisign のジョブ）は、リリースの全ての
  `*.tar.gz`・`*.zip` をダウンロードし、`sha256sum -c --ignore-missing SHA256SUMS` で確かめてから全部に署名していた。
  `--ignore-missing` は SHA256SUMS に載っていないファイルを見ないので、リリースへ資産を足せる者（書き込みのトークン等）が
  足した、プロジェクトがビルドしていないターゲットの名前のアーカイブにもプロジェクトの鍵で署名し、`cargo binstall` は
  その署名を受けて入れた（「CI が作ったものだけに署名する」という規則に反する）。

## 対応

- cosign で確かめた SHA256SUMS に載っているアーカイブだけに署名する。載っていないアーカイブがリリースにあれば、
  署名せずに止める。

## テスト

- gh・cosign・minisign を差し替えた手元の試行で、載っているものだけに署名し、載っていないアーカイブがあると止まることを
  確かめた（リリースの経路なので CI の試験は無い）。
