# F-290: 配布する各バイナリ・パッケージにアプリケーションのロゴ（`docs/images/veil_logo.webp`）を組み込む

**状態: 未着手（2026-11-05 起票。調査・設計のみ）**

## 今の状態

- ロゴ `docs/images/veil_logo.webp`（1800×1800、VP8X・アルファ付き、約 290KB）は README・`docs/guide/references.md` で表示しているだけで、
  配布物のどれにも入っていない。
  - Windows の `veil.exe`（`x86_64`・`aarch64-pc-windows-msvc`、`docker/Dockerfile.windows` の cargo-xwin でクロスビルド）にアイコンも
    `VERSIONINFO` も無い（エクスプローラ・タスクマネージャ・「アプリと機能」で既定のアイコン、プロパティの「詳細」が空）。
  - MSI（`packaging/windows/veil.wxs`、WiX v5）に `ARPPRODUCTICON` が無い（「インストールされているアプリ」・winget の表示が既定のアイコン）。
  - macOS（universal2 のアーカイブ・Homebrew）、Linux（tar・deb・rpm・コンテナ）、BSD（tar・ports）の ELF / Mach-O にアイコンの仕組みは無く、
    パッケージもアイコンを入れていない。
  - Helm の `deploy/helm/veil/Chart.yaml` に `icon:` が無い（Artifact Hub・Rancher 等が既定の絵を出す）。
  - rustdoc（docs.rs の `veil-proxy`・`veil-proxy-kit` 等）に `html_logo_url`・`html_favicon_url` が無い。
  - 既定のページ（`packaging/defaults/www/index.html`・`quickstart.html`）・ライブラリの Swagger UI に favicon が無い。
- ビルド依存（`[build-dependencies]`）は無い。`build.rs` は cfg を出すだけで、リソースのコンパイルはしていない。

## OS ごとの「バイナリにロゴを組み込む」の意味（調査）

| 対象 | 仕組み | 効果 |
|---|---|---|
| Windows の PE（`veil.exe`。任意で `veil_capi.dll`） | リソース（`ICON`・`VERSIONINFO`）を `.rc` → `.res` にしてリンク | エクスプローラ・タスクマネージャ・プロパティにロゴと版・会社名・説明 |
| Windows の MSI | `<Icon>` + `ARPPRODUCTICON` | 「インストールされているアプリ」・winget の一覧 |
| macOS の Mach-O（CLI） | `-sectcreate __TEXT __info_plist`（Info.plist）は入れられるが、**バンドルでない CLI にアイコンを表示させる仕組みは無い**（Finder のカスタムアイコンはリソースフォーク・拡張属性で、tar・Homebrew で落ちる） | アイコンは出ない。Info.plist で `CFBundleIdentifier`・版を持たせるのは可能（コード署名・TCC の識別には効く） |
| Linux・BSD の ELF | アイコンの標準は無い。デスクトップは freedesktop の hicolor テーマ + AppStream の metainfo（`type="console-application"`）で出す | deb・rpm・ports が `/usr/share/icons/hicolor/*/apps/veil.png` と `/usr/share/metainfo/*.metainfo.xml` を入れると、ソフトウェアセンター（GNOME Software・Discover）が出す |
| コンテナイメージ | ラベル `io.artifacthub.package.logo-url`（Artifact Hub）。Docker Hub・ghcr.io のロゴはリポジトリの設定（人の作業） | Artifact Hub の表示 |
| Helm チャート | `Chart.yaml` の `icon:`（URL） | Artifact Hub・Rancher・Lens |
| rustdoc | `#![doc(html_logo_url = ..., html_favicon_url = ...)]` | docs.rs の左上・タブ |

ELF・Mach-O の中にロゴの画像そのものを埋め込んでも OS は見ない（独自のセクションに置く意味が無い）ので、**PE は実際に埋め込み、
それ以外は各パッケージの標準の場所に置く**。

## 改修内容（設計）

### 1. 元の画像から派生物を作るスクリプト（コミットする）

- `tools/release/render-icons.sh`（`render-defaults.sh` と同じく `--check` で一致を確かめる）で `docs/images/veil_logo.webp` から作る:
  - `packaging/icons/veil.ico`（16・20・24・32・40・48・64・128・256。256 は PNG 圧縮のエントリ）
  - `packaging/icons/hicolor/{16,32,48,64,128,256,512}/veil.png`
  - `packaging/icons/veil.icns`（任意。将来 `.pkg`・`.app` を作るとき用。今は作らない）
- 変換は ImageMagick（`magick`/`convert`、webp のデリゲート）で行う。**ビルド時には変換しない**（`image` クレート等の重い依存を
  `build-dependencies` に入れない。CI・BSD の VM・コンテナビルドに ImageMagick を要求しない）。生成物はバイナリだが小さい（ico で 100KB 程度の見込み）。
- `--check` は CI（`ci.yml` の文書・生成物の検査の job）で回し、ロゴを差し替えたのに派生物を作り直していないことを検出する
  （出力のバイト一致は ImageMagick の版で揺れるので、元画像の SHA-256 を `packaging/icons/SOURCE.sha256` に書いて比べる）。

### 2. Windows の PE にリソースを埋め込む（`build.rs`、依存を足さない）

- `CARGO_CFG_TARGET_OS == "windows"` かつ `CARGO_CFG_TARGET_ENV == "msvc"` のときだけ、`packaging/windows/veil.rc`（`ICON` と `VERSIONINFO`。
  版は `CARGO_PKG_VERSION` から `/D` で渡す）をリソースコンパイラで `.res` にし、`cargo:rustc-link-arg-bins=<OUT_DIR>/veil.res` で **veil の bin だけ**に渡す
  （ライブラリとして veil を使う側のバイナリには付けない。`rustc-link-arg-bins` は依存先のパッケージには効かない）。
  - リソースコンパイラは `RC` 環境変数 → `llvm-rc`（cargo-xwin の Docker イメージ）→ `rc.exe`（MSVC のホスト）の順に探す。見つからなければ
    `cargo:warning` を出してリソース無しで続ける（ビルドを壊さない。ただし `dist.yml` の Windows の job では必須にし、無ければ失敗させる環境変数
    `VEIL_REQUIRE_WIN_RESOURCES=1`）。
  - `lld-link`・`link.exe` は `.res` を直接受け取る（`embed-resource` クレートと同じやり方。クレートは `toml`・`rustc_version` 等を引くので使わない）。
- `veil_capi.dll`（`crates/veil-capi`）・Python の `.pyd` は任意（DLL のアイコンは表示されない。`VERSIONINFO` だけ入れる価値はある）。範囲は
  `veil.exe` を必須、`veil_capi.dll` の `VERSIONINFO` を任意とする。
- 確認: `dist.yml` の Windows の試験（`windows-test`）で PowerShell の `(Get-Item veil.exe).VersionInfo` が版と製品名を返すこと、
  `[System.Drawing.Icon]::ExtractAssociatedIcon` が既定でないアイコンを返すこと。`docker/Dockerfile.windows` の cacher 段と builder 段で
  `build.rs` の入力（`packaging/windows/veil.rc`・`packaging/icons/veil.ico`）がコピーされること（`.dockerignore` のホワイトリスト。
  memory にある [[project_cargo_target_docker_trap]] と同種の罠）。

### 3. パッケージ

- MSI（`veil.wxs`）: `<Icon Id="veil.ico" SourceFile="...\veil.ico" />` と `ARPPRODUCTICON`。`build-msi.ps1` に ico のパスを渡す。
- deb・rpm（`packaging/debian`・`packaging/rpm`）・BSD の ports（`packaging/bsd/ports`）・Homebrew（任意）: hicolor の PNG と
  `packaging/icons/io.github.aofusa.veil.metainfo.xml`（`console-application`、`<icon type="stock">veil</icon>`）を入れる。
  アーカイブ（tar・zip）には入れない（バイナリだけの入手経路は設定も入れない方針。F-247）。
- コンテナ: `docker/Dockerfile.*` の最終段にラベル `io.artifacthub.package.logo-url`（raw.githubusercontent.com の PNG の URL）。Docker Hub・ghcr の
  ロゴの設定は人の作業として `docs/releases` の手順に書く。
- Helm: `Chart.yaml` に `icon:`（同じ PNG の URL。`tools/release/bump-version.sh` の対象外）。
- rustdoc: `src/lib.rs`・`crates/veil-kit`・`crates/veil-macros` 等の `lib.rs` に `#![doc(html_logo_url, html_favicon_url)]`。

### 4. 既定のページ・Swagger UI（任意。ホットパス外）

- `packaging/defaults/www/` に `favicon.ico`（`veil.ico` と同じ）を置き、`index.html`・`quickstart.html` に `<link rel="icon">`。
  `render-defaults.sh` が入手経路ごとに配る。veil 本体に画像を埋め込んで `/favicon.ico` を返すことはしない（設定に無いルートを勝手に生やさない）。

## 受け入れ条件

- Windows の `veil.exe`（x86_64・aarch64）のプロパティにロゴ・版・製品名が出る（`windows-test` で自動確認）。MSI の一覧にロゴが出る（手動確認を
  リリースの人の作業に書く）。
- Linux の既定ビルド・`--no-default-features`・BSD・macOS のビルドが変わらない（`build.rs` の Windows 分岐は他の target で何もしない）。
  `cargo package`（crates.io）に `packaging/windows/veil.rc`・`packaging/icons/veil.ico` が含まれ、crates.io から入れた Windows のビルドでも
  リソースが付く（含めないなら警告だけで続く）。
- deb・rpm で `/usr/share/icons/hicolor/256x256/apps/veil.png`・metainfo が入り、`appstreamcli validate` が通る。
- `render-icons.sh --check` を CI で回す。

## リスク・注意

- crates.io のパッケージの大きさ（`include`/`exclude`）。ico を入れると 100KB 程度増える。
- cargo-xwin のイメージに `llvm-rc` が無い版がある → `docker/Dockerfile.windows` で入れる。
- ロゴの webp は透過付き。16px・32px で潰れて読めない場合は、`veil_logo_alternative.svg` から小さいサイズを作る（デザインの判断は人）。

## 関連

- [F-247](F-247-distribution-foundation.md)〜F-256（配布）、[F-252](F-252-winget.md)（MSI）、[F-184](F-184-windows-macos-feature-sets.md)（Windows のクロスビルド）
