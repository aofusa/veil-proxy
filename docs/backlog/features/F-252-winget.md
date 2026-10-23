# F-252: winget で公開する

**状態: 設計（feat/distribution-tickets、2026-10-23。2026-10-23 に MSI へ改訂）**

## 目的

- Windows 10・11（x64・arm64）で `winget install Aofusa.Veil` で入れられるようにする。
- 入れるときに、既定の設定とページを Windows の場所に置く（F-247 の表）。
  - 設定: `%ProgramData%\veil\config.toml`
  - ページ: `%ProgramData%\veil\www\`
- 入れた直後に `veil` を引数無しで起動すれば、その既定の設定を読んで動くようにする。
  - Windows の既定のパスは F-247 で `%ProgramData%\veil\config.toml` にする。
  - `curl http://127.0.0.1:8080/`（またはブラウザ）で確かめられる。
- 版を上げる提案（winget-pkgs への PR）を、リリースの公開から自動にする。

## 今の状態

- 成果物:
  - `veil-<ver>-{x86_64,aarch64}-pc-windows-msvc.zip` は v0.8.0 まで手で作っていた。
  - 中身は `veil.exe` だけ。CI で作るのは F-247 から。
- 引数無しの `veil.exe` は `/etc/veil/config.toml` を探して起動できない（F-247 で Windows の既定のパスにする）。
- winget-pkgs への登録は無い。

## 設計

### インストーラは MSI にする

- winget の `zip` + `portable` は、zip を利用者ごとの場所に展開してコマンドの別名を作るだけ。
  `%ProgramData%` に既定の設定とページを置けない。
  そこで、ファイルを適切な場所に置ける MSI を作る。
- **作り方:** WiX Toolset（v5）で `packaging/windows/veil.wxs` を書き、F-247 の `binaries` の Windows のジョブで作る。
  - 成果物: `veil-<ver>-x86_64-pc-windows-msvc.msi`・`veil-<ver>-aarch64-pc-windows-msvc.msi`。
  - zip（バイナリだけを使う人向け）は今までどおり作る。
- **入れる範囲:** 端末全体（per-machine。管理者の権限が要る）。
  - `C:\Program Files\veil\veil.exe`。システムの `PATH` に `C:\Program Files\veil\` を足す。
  - `%ProgramData%\veil\config.toml`: F-247 の雛形から作る。
    - リスナーは `127.0.0.1:8080` の平文、ページは `%ProgramData%\veil\www\`。
    - component に `NeverOverwrite="yes"` と `Permanent="yes"` を付ける。上げても消しても、利用者が書き換えた設定を残す。
  - `%ProgramData%\veil\www\index.html`・`quickstart.html`: 上げたときは新しいものに替える。
  - **権限:** `%ProgramData%\veil\` の ACL を、Administrators・SYSTEM は書き込み、Users は読み取りだけにする（`PermissionEx`）。
    `%ProgramData%` の既定では Users が新しいファイルを作れるので、設定を書き換えられないように絞る。
- **上げる:** `MajorUpgrade`（同じ `UpgradeCode`）で、前の版を消して入れ直す。設定は上のとおり残る。
- **サービス:** 登録しない（今回の範囲外）。`sc.exe create` での登録の手順を文書にする。
- **署名:**
  - MSI と `veil.exe` に署名が無いと、SmartScreen・winget-pkgs の検証で止まることがある。
  - まずは署名無しで出し、止まったら Azure Trusted Signing などのコード署名を足す（別のチケット）。

### マニフェスト

- `PackageIdentifier: Aofusa.Veil`、`Moniker: veil`。
- インストーラ:
  - `InstallerType: wix`、`Scope: machine`。
  - x64・arm64 の 2 つ。`InstallerSha256` は F-247 の `SHA256SUMS` から入れる。`ProductCode` は MSI から読む。
  - `UpgradeBehavior: install`（MSI の `MajorUpgrade` に任せる）。
- メタデータ:
  - `License: MIT OR Apache-2.0`、`LicenseUrl`。
  - `ReleaseNotesUrl` はそのリリースのページ。
  - `Tags`: `reverse-proxy`・`http3`・`web-server`。
  - 英語の `ShortDescription`・`Description`。
- 雛形は `packaging/winget/` に置き、版ごとの値は workflow で埋める。

### 提出

- **初回:** 人が `wingetcreate new`（または komac）で PR を出す。モデレーターの審査を受ける。
- **2 回目以降:**
  - `release: published`（pre-release でないもの）を契機に、komac（`komac update Aofusa.Veil --version <ver> --urls ... --submit`）で PR を出す。
  - token は、winget-pkgs の fork に push し PR を出せる fine-grained token にする。
    置き場所は environment `release` の secret `WINGET_TOKEN`。
- winget-pkgs の検証の pipeline は、マルウェアの検査と、入れて動くかを見る。

### 試験

`release.yml`（rc を含む）の windows-2022・windows-11-arm の runner で、次のことを回す。
rc の成果物は draft にあるので、`gh release download` で取り、runner の中の HTTP サーバで配ってマニフェストの URL を差し替える。

1. `winget validate --manifest <dir>`
2. `winget settings --enable LocalManifestFiles` の後、`winget install --manifest <dir>`。
3. 置かれたものを確かめる。
   - `%ProgramData%\veil\config.toml`・`www\` が置かれている。
   - ACL が上のとおり（`Get-Acl`）。
   - 新しい shell で `veil --version`。
4. `veil` を引数無しで背景で起動し、`Invoke-WebRequest http://127.0.0.1:8080/`・`/quickstart.html` が 200。
   ログの `Config File:` が `%ProgramData%\veil\config.toml`（既定）であることを確かめる。
5. 設定を書き換えてから、前の版（rc では、前に公開した版）からの上げ直しをする。書き換えが残る。
6. `winget uninstall Aofusa.Veil` で、実行ファイルと `PATH` の項目が消え、`%ProgramData%\veil\config.toml` は残る。

### 文書

- README（英日）の「Install」と `docs/guide/running.md`（英日）に、次のことを書く。
  - winget の入れ方、設定・ページの場所、起動のしかた
  - 消しても設定が残ること
- zip（バイナリだけ）を使う人は、設定とページを自分で用意すること（F-247）。

## 受け入れ条件

- winget-pkgs に `Aofusa.Veil` が載り、x64・arm64 で次のことができる。
  - `winget install Aofusa.Veil` → `veil`（引数無し）→ `http://127.0.0.1:8080/` が、置いた既定のページを返す。
- 上げても消しても、利用者が書き換えた設定が残る。
- リリースを公開すると、版を上げる PR が自動で出る。

## 依存・リスク

- F-247 が先。依るもの: CI の Windows の成果物、Windows の既定のパス、既定の設定とページの雛形。
- 人の作業が要る: 初回の PR、winget-pkgs の fork、token。
- 審査の時間は読めない。公開の速さは winget-pkgs に依る。
- **arm64 の MSI:** WiX の arm64 の対応（`-arch arm64`）を、windows-11-arm の runner で確かめる。

## 関連

- [F-184](F-184-windows-macos-feature-sets.md)（Windows の feature セット）
- [F-247](F-247-distribution-foundation.md)
