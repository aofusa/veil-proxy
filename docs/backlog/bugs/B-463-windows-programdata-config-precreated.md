# B-463: Windows で特権の無い利用者が先に作った %ProgramData%\veil と config.toml を、MSI が残し veil が読んでいた

**状態: 修正済み（fix/security-scan-fable-max、2026-11-18）**

fix/security-scan-fable-max のセキュリティスキャン（`CLAUDE-SECURITY-20261115-030813`）の指摘
「MSI keeps a %ProgramData%\veil\config.toml that an unprivileged user created before install, so that user controls the
config of an elevated veil」と「MSI trusts a pre-existing %ProgramData%\veil and config.toml, so a standard user who
creates them first keeps write control of the config veil loads by default」から確認。

## 事象

- `%ProgramData%` には特権の無い利用者もフォルダを作れて、作った利用者がその所有者になる。MSI（`veil.wxs`）は
  `%ProgramData%\veil` の DACL だけを置き換え（所有者は変えない）、既にある `config.toml` は残していた
  （NeverOverwrite）。所有者は DACL をいつでも書き戻せる。
- veil は Windows の既定のパス `%ProgramData%\veil\config.toml` を所有者・DACL を見ずに読んでいた。入れる前に置かれた
  設定（任意のディレクトリを配信する File のルート・任意のファイルへ書くログ等）で、管理者・サービス（LocalSystem）
  として動く veil が動いた。

## 対応

- veil（`config::win_trust`）: 既定の設定ファイル（`-c`・`VEIL_CONFIG` で同じファイルを別の書き方で指したときも、
  ファイルの同一性で判定する）は、所有者が Administrators・SYSTEM・TrustedInstaller・veil を動かしている
  アカウントのどれかで、他のアカウントに書き換え（書き込み・追記・拡張属性・DACL・所有者の変更）を許していないときだけ
  読む。検査と読み取りは同じハンドルで行う。起動とリロードで同じ。
- MSI: `%ProgramData%\veil` と `www` の SDDL に所有者（`O:BA`）を足し、`www` も保護した DACL に置き換える。既定の
  ページは先にあったもの（日付で利用者のものと見なされて残る）を消してから入れる。先にあった `config.toml` は
  残す（所有者を付け替えると置かれた中身を信用することになる）が、上の検査で veil は読まない。
- 利用者ガイド（`running.md` の英日）に、検査と直し方を書いた。

## テスト

- Windows の単体試験 `config::win_trust::tests`（自分で作ったファイルは読める・Users に書き込みを許したファイルは
  断る・大文字で書いた同じファイルも断る・既定でないファイルは検査しない）を aofish の実機で確かめた。
- MSI: 同じ SDDL・RemoveFile を持つ試しの MSI を aofish で入れ、Users が所有する先に作ったフォルダの所有者が
  Administrators になり、DACL が保護され、置かれたページが入れ直され、置かれた設定は残ることを確かめた。
  `tools/release/test-msi.ps1` に同じ場面（5. 先に作られたフォルダ）と、veil が置かれた設定を断ることを足した
  （リリースの dist.yml で回る）。実際の `packaging/windows/veil.wxs` を WiX 5.0.2 で x64・arm64 ともビルドできることを
  aofish で確かめた（aofish には veil の MSI が入っているので、test-msi.ps1 は回していない）。
