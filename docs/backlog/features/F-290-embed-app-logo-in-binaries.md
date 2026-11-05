# F-290: 配布する各バイナリ・パッケージにアプリケーションのロゴ（`docs/images/veil_logo.webp`）を組み込む

**状態: 完了（v0.11.0）**

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

## 実装

設計のとおり。選んだ既定値と設計からの差分:

- **派生物**: `tools/release/render-icons.sh`（引数無しで作り直す・`--check`）。`packaging/icons/veil.ico`（16・20・24・32・40・48・64・128・256。
  256 は PNG 圧縮。約 210KB）、`packaging/icons/hicolor/<N>x<N>/apps/veil.png`（16〜512。`/usr/share/icons/hicolor` と同じ形に
  したのでそのまま写せる）、`packaging/defaults/www/favicon.ico`（16・32・48 だけ。約 15KB。`veil.ico` と同じにすると既定のページに
  210KB の favicon を配ることになるので分けた）、`packaging/icons/SOURCE.sha256`。日時のチャンクを書かないので、同じ ImageMagick
  なら作り直しても 1 バイトも変わらない（6.9.12 で確認）。`--check` は元画像のハッシュ・生成物の有無・`appstreamcli validate`
  （入っていれば）を見る。CI は `ci.yml` の `docs` の job で `appstream` を入れて回す。`.icns` は作っていない（設計どおり）。
- **AppStream**: `packaging/icons/io.github.aofusa.veil.metainfo.xml`（`console-application`、`<icon type="stock">veil</icon>`、
  `<provides><binary>veil</binary>`）。`<releases>` は入れていない（版を上げるたびの更新を増やさない。`appstreamcli validate
  --pedantic` の `releases-info-missing` だけが出る）。
- **Windows の PE**: `build.rs` の `emit_windows_resources`。`packaging/windows/veil.rc` は雛形で、`@ICON_PATH@`・`@VERSION@`・
  `@VERSION_COMMA@` を build.rs が埋めて `OUT_DIR` に書く（`/D` で渡す形にしなかったのは、llvm-rc のマクロの展開が clang の
  プリプロセッサ頼みで、clang が見つからない環境で結果が変わるため。llvm-rc には `/no-preprocess` を渡す）。`FILEVERSION` は
  `0.11.0-rc.1` → `0,11,0,0`、文字列の `ProductVersion` は版そのまま。リソースコンパイラは `RC_<target>`・`RC` → `llvm-rc`
  （`llvm-rc-<版>`・`/usr/lib/llvm-<版>/bin/llvm-rc` も）→ Windows のホストだけ `rc.exe`（PATH → Windows SDK）。Linux のホストで
  PATH の `rc` を使わない（Plan 9 の rc シェルのことがある）。llvm-rc は `/` で始まる引数をオプションとして読むので、`OUT_DIR` を
  作業ディレクトリにして相対パスで渡す。`cargo:rustc-link-arg-bins` で veil の bin だけにリンクする。見つからない・失敗したら
  `cargo:warning`、`VEIL_REQUIRE_WIN_RESOURCES=1` なら panic。Windows（MSVC）以外の target では環境変数を 1 つ読むだけで何もしない。
  `veil_capi.dll` の `VERSIONINFO`（任意）は入れていない。
- **Docker**: `docker/Dockerfile.windows` の cacher 段に `veil.rc`・`veil.ico` を COPY（cacher 段でも build.rs が走る）、
  `ENV VEIL_REQUIRE_WIN_RESOURCES=1`（`dist.yml` の Windows の job ではなく Dockerfile に置いた。配布の Windows ビルドはこの
  Dockerfile だけ）、`llvm-rc` が無ければ版付きの名前へのリンクを張る（それも無ければ apt の llvm）。`.dockerignore` に
  `packaging/windows/veil.rc`・`packaging/icons/veil.ico` を足した。Linux の Dockerfile は build.rs が何も読まないので変えていない。
- **crates.io**: `Cargo.toml` の `include` に `veil.rc`・`veil.ico` を足した（crates.io から入れた Windows のビルドでもリソースが付く）。
- **確認**: `tools/release/check-pe-resources.py`（標準ライブラリだけで PE のリソースの表を読み、RT_ICON・RT_GROUP_ICON・RT_VERSION と
  ProductName "veil" を確かめる）を `dist.yml` の `cross`（windows）の job で回す。`windows-test` で `(Get-Item veil.exe).VersionInfo`
  の ProductName・ProductVersion を確かめる。アイコンの比較（`ExtractAssociatedIcon`）は既定のアイコンの比べる相手が安定しないので
  入れず、リソースの表で見る。`test-msi.ps1` は Uninstall の `DisplayIcon` と `www\favicon.ico` を見る。`test-package.sh`（deb・rpm）は
  hicolor の 256 と metainfo が入ったことを見る。
- **MSI**: `<Icon>` + `ARPPRODUCTICON`（`build-msi.ps1` が `-d IconFile=` で渡す）、`www\favicon.ico` の Component。
- **deb・rpm**: `packaging/scripts/build.sh` の `stage_rootfs` が hicolor と metainfo を置き、`veil.spec` の `%files` に足した。
- **ports**: FreeBSD は `INSTALLS_ICONS=yes`、OpenBSD は `RUN_DEPENDS = x11/gtk+4,-guic` と `@tag gtk-update-icon-cache`、
  NetBSD は `ICON_THEMES=yes`。3 つとも PLIST に hicolor・metainfo・`favicon.ico` を足した（ソースの tarball の `packaging/icons` から入れる）。
- **コンテナ・Helm・rustdoc**: 4 つの Linux の Dockerfile の runtime 段に `LABEL io.artifacthub.package.logo-url`、`Chart.yaml` に
  `icon:`、`src/lib.rs`・`crates/veil-kit`・`crates/veil-build`・`crates/veil-macros` に `html_logo_url`（128px）・`html_favicon_url`
  （32px）。URL は `raw.githubusercontent.com/aofusa/veil-proxy/main/packaging/icons/hicolor/...`（main に入るまでは 404）。
- **既定のページ**: `index.html`・`quickstart.html` に `<link rel="icon" href="favicon.ico">`。`render-defaults.sh` は `www/` を丸ごと
  写すので、コンテナ・アーカイブ・Homebrew・deb・rpm・MSI・ports に入る。
- **文書**: `docs/guide/building.md`・`docs/guide/ja/building.md` の「Application icon and version information」、`packaging/README.md`。

### 確かめたこと

- Linux: `cargo fmt --all --check`、`cargo build --features full`、`cargo clippy --all-targets --features full -- -D warnings`、`cargo check`（default）、
  `cargo check --no-default-features` がすべて警告無しで通る。`cargo package --list` に `packaging/windows/veil.rc`・`packaging/icons/veil.ico` が入る。
  `render-defaults.sh --check`・`render-icons.sh --check` が通る。
- 本物の build.rs を最小のクレートに写して `cargo check --target x86_64-pc-windows-msvc` を回し、llvm-rc 18（`/usr/lib/llvm-18/bin`）で
  `.res` ができて `rustc-link-arg-bins` が出ること、`llvm-cvtres` で COFF にして `llvm-readobj --coff-resources` で ICON（9 枚）・
  GROUP_ICON・VERSIONINFO があること、`check-pe-resources.py` が ok を返すことを確かめた。`RC` を存在しないパスにすると警告で続き、
  `VEIL_REQUIRE_WIN_RESOURCES=1` を足すと失敗することも確かめた。
- `appstreamcli validate --no-net --pedantic`（1.0.2）が通る。

### 人の作業・未確認（残り）

- **Windows の実機**: 実際の `veil.exe` のリンク（cargo-xwin の lld-link、MSVC の link.exe）とエクスプローラのプロパティ・
  タスクマネージャの表示、MSI の「インストールされているアプリ」のアイコンは、`dist.yml` の Windows の job か Windows の機械で確かめる。
  手元の Windows で確かめるときは `cargo build --release`（Visual Studio Build Tools の Windows SDK の rc.exe を自動で探す）の後に
  `python tools/release/check-pe-resources.py target\release\veil.exe`。
- **cargo-xwin のイメージの llvm-rc**: Dockerfile は名前の揺れを吸収しているが、実際のイメージでの `docker build` は未確認。
- **パッケージ**: deb・rpm（lintian・rpmlint を含む）・BSD の ports（`bsd-port-test.sh`）のビルドと試験は未確認。
- **Docker Hub・ghcr.io のリポジトリのロゴ**: レジストリの画面で人が設定する（`packaging/icons/hicolor/256x256/apps/veil.png` を
  上げる）。Artifact Hub はラベルと `Chart.yaml` の `icon` で出る。
- **小さい大きさの絵**: 16px・32px はロゴを縮めただけ（読めるが細部は潰れる）。`veil_logo_alternative.svg` から小さい大きさを
  描き分けるかはデザインの判断（人）。

## 関連

- [F-247](F-247-distribution-foundation.md)〜F-256（配布）、[F-252](F-252-winget.md)（MSI）、[F-184](F-184-windows-macos-feature-sets.md)（Windows のクロスビルド）
