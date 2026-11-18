# B-471: リリースの winget の job が、SHA256SUMS の cosign の署名と MSI のハッシュを確かめずに winget-pkgs へ PR を出していた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「The winget job submits the release MSIs to winget-pkgs without checking the Sigstore-signed SHA256SUMS」
から確認。

## 事象

- `release-downstream.yml` の winget の job は cosign を入れず、SHA256SUMS を署名を確かめずに取り、komac が URL の
  MSI を取り直してハッシュを取っていた。他の配布先（minisign・homebrew・repos）は MSI を扱わないので、リリースの
  MSI を差し替えられると、その MSI のハッシュの PR が winget-pkgs へ出た（`winget install` は管理者として入れる）。

## 対応

- 他の配布先と同じく cosign で SHA256SUMS の署名を確かめてからマニフェストを作る。
- komac の直前に両方の MSI を取り、確かめた SHA256SUMS の行と `sha256sum -c` で照らす（載っていなければ止める）。

## テスト

- リリースの経路なので CI の試験は無い。MSI を照らす部分を手元の偽の SHA256SUMS で試した（一致・改変した MSI・載っていない版）。
  ワークフローの YAML は Python の yaml で読めることを確かめた（actionlint はこの端末に無い）。
