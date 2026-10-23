# F-252: winget で公開する

**状態: 設計（feat/distribution-tickets、2026-10-23）**

## 目的

- Windows 10・11（x64・arm64）で `winget install Aofusa.Veil` で入れられるようにする。
- 入れた直後に `veil` を引数無しで起動すれば、内蔵の既定の設定で動くようにする（F-247）。
  `curl http://127.0.0.1:8080/`（または、ブラウザ）で確かめられる。
- 版を上げる提案（winget-pkgs への PR）を、リリースの公開から自動にする。

## 今の状態

- 成果物:
  - `veil-<ver>-{x86_64,aarch64}-pc-windows-msvc.zip` は v0.8.0 まで手で作っていた。
  - 中身は `veil.exe` だけ。CI で作るのは F-247 から。
- 引数無しの `veil.exe` は `/etc/veil/config.toml` を探して起動できない。F-247 で、内蔵の既定の設定と `%ProgramData%\veil\config.toml` にする。
- winget-pkgs への登録は無い。

## 設計

### マニフェスト

- `PackageIdentifier: Aofusa.Veil`、`Moniker: veil`。
- インストーラ:
  - `InstallerType: zip`、`NestedInstallerType: portable`。
  - `NestedInstallerFiles` は `veil-<ver>-<target>/veil.exe`。
  - `PortableCommandAlias: veil`。
  - x64・arm64 の 2 つ。`InstallerSha256` は F-247 の `SHA256SUMS` から入れる。
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
  - 署名の無い exe は SmartScreen の評判で止まることがある。
  - まずは署名無しで出し、止まったら Azure Trusted Signing などのコード署名を足す（別のチケット）。

### 試験

`release.yml`（rc を含む）の windows-2022・windows-11-arm の runner で、次のことを回す。

1. `winget validate --manifest <dir>`
2. `winget settings --enable LocalManifestFiles` の後、`winget install --manifest <dir>`。
   rc では pre-release の成果物の URL を使う。
3. 新しい shell で `veil --version`。
4. `veil` を引数無しで背景で起動し、`Invoke-WebRequest http://127.0.0.1:8080/`・`/quickstart.html` が 200。
5. `winget uninstall Aofusa.Veil` で、エイリアスと実行ファイルが消える。

### 文書

- README（英日）の「Install」と `docs/guide/running.md`（英日）に、winget の入れ方を書く。
- `%ProgramData%\veil\config.toml` を置けば、それを読むことも書く。
- サービスとしての登録（`sc.exe`・タスクスケジューラ）は今回の範囲外。手順だけを文書にする。

## 受け入れ条件

- winget-pkgs に `Aofusa.Veil` が載り、x64・arm64 で `winget install Aofusa.Veil` → `veil` → `http://127.0.0.1:8080/` が内蔵のページを返す。
- リリースを公開すると、版を上げる PR が自動で出る。

## 依存・リスク

- F-247（CI の Windows の成果物・内蔵の既定の設定・Windows の既定のパス）が先。
- 人の作業が要る: 初回の PR、winget-pkgs の fork、token。
- 審査の時間は読めない。公開の速さは winget-pkgs に依る。

## 関連

- [F-184](F-184-windows-macos-feature-sets.md)（Windows の feature セット）
- [F-247](F-247-distribution-foundation.md)
