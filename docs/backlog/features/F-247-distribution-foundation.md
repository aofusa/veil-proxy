# F-247: 配布の土台（既定のパス・パッケージの既定の設定とページ・GitHub Release の draft を CI で作る）

**状態: 実装済み（feat/distribution-tickets、2026-10-23。v0.9.0 で公開）**

パッケージマネージャでの公開（F-248〜F-253）・Helm chart（F-254）の前提になるチケット。

## 目的

- **パッケージマネージャで入れた利用者:** 何も指定せずに起動すれば既定の設定で動き、すぐに動作を確かめられるようにする。
  - 公開イメージの `docker run -p 8080:8080 ghcr.io/aofusa/veil`（F-246）と同じ体験にする。
  - 対象: Homebrew、apt、dnf、winget、BSD の ports、Helm。
  - 各パッケージは入れるときに、既定の設定（`config.toml`）と既定のページ（`index.html`・`quickstart.html`）をそのプラットフォームの適切な場所に置く。
  - veil は既定のパスからそれを読む。
- **veil は設定をバイナリに内蔵しない。** 起動時は常に設定ファイルを読む。
  - `-c` が無ければ既定のパスを見る。無ければ、分かりやすいエラーで止まる。
- **バイナリだけを取る利用者は、設定ファイルとページを自分で用意する前提にする。** この前提を文書に明記する。
  - 対象: GitHub Release のアーカイブ・`cargo install`・`cargo binstall`。
  - アーカイブには、出発点として例の `config.toml` と `www/` を同梱する。自動では読まない。
- **リリースのページ:** タグの push から CI が、バイナリ・パッケージ・チェックサム・署名・リリースノートまでを作ってアップロードする。
  - ページは draft で作る。公開するのは人。

## 今の状態（v0.8.4）

- **既定のパス:** `veil` は `-c` が無いと `/etc/veil/config.toml` を読む（`src/config/loaded.rs` の `DEFAULT_CONFIG_PATH`）。
  - 全 OS で同じ値。Windows・macOS（Homebrew の prefix）・FreeBSD（`/usr/local/etc`）・NetBSD（`/usr/pkg/etc`）では、その OS の慣習の場所と合わない。
  - ファイルが無いときのエラーは、どこに何を置けばよいかを示さない。
- **コンテナ:** 何も指定せずに動くのはコンテナだけ。
  - `packaging/container/config.toml` と `packaging/container/www/` を、`CMD ["/veil", "-c", "/etc/veil/conf.d/config.toml"]` で読む。
- **`.deb`・`.rpm`:**
  - 既定の設定は `contrib/config/config.toml`（約 2100 行のリファレンスの写し）。
  - リスナーは `127.0.0.1:443` の TLS と `:80` のリダイレクト。postinst で自己署名の証明書を作る。
  - 設定は `/var/etc/veil/` で、バイナリの既定と違う（ユニットが `-c` で渡している）。
- **アーカイブ:** tar.gz・zip には `veil` のバイナリしか入っていない。
- **GitHub Release:** 成果物とリリースノートを手元で作り、手で作ったページに手でアップロードしている。
  - v0.8.0 には成果物が 18 個ある。v0.8.1〜v0.8.4 にはリリースのページも成果物も無い。
  - README の「Install」は「各リリースに成果物が付いている」と書いていて、今の状態と合わない。
  - リリースノートは `docs/artifacts/v<ver>_release_note.md`（git 管理外）にあり、CI からは読めない。
  - `release.yml` が作るのは、イメージ・crates.io・PyPI だけ。
  - `release-sbom.yml` は、ページを公開したとき（`release: published`）に SBOM を添付する。

## 設計

### 1. 設定ファイルの場所の決め方

起動時に読む設定ファイルを、次の順で決める。

1. `-c <path>`
2. 環境変数 `VEIL_CONFIG`（clap の `env`）
   - パッケージの起動の仕組み（Homebrew の wrapper、systemd の `EnvironmentFile` など）が使う。
3. ビルド時に決めた既定のパス
   - ビルド時の環境変数 `VEIL_DEFAULT_CONFIG_PATH` があれば、その値（`option_env!`）。ソースからビルドする ports が、その OS の prefix を入れる。
   - 無ければ OS ごとの既定:

     | OS | 既定のパス |
     |---|---|
     | Linux・macOS・FreeBSD・OpenBSD・NetBSD | `/etc/veil/config.toml`（今と同じ） |
     | Windows | `%ProgramData%\veil\config.toml`（新しい。今は Unix のパスを見ている） |

- **ファイルが無いときの扱い:** 今と同じく起動しない。内蔵の設定に落ちることはしない。
  - エラーの文を、どこに何を置けばよいか分かるものにする。英語で、チケット番号は入れない（F-242）。
    ```
    configuration file not found: /etc/veil/config.toml
    pass a file with -c <path> or set VEIL_CONFIG; package installs place a default file here.
    see https://github.com/aofusa/veil-proxy/blob/main/docs/guide/running.md#configuration-file
    ```
- `-t` も同じ順で決めたファイルを検査する。
- 起動の `info!` の `Config File: <path>`（今もある）に、どの方法で決まったか（`-c`・`VEIL_CONFIG`・既定）を足す。
- **触る場所:** `src/config/cli.rs`・`src/config/loaded.rs`（`DEFAULT_CONFIG_PATH` を関数にする）・`build.rs`（`VEIL_DEFAULT_CONFIG_PATH` を `rerun-if-env-changed`）。
  起動時だけの変更で、ホットパスには入らない。
- 外部契約が増える（`VEIL_CONFIG`・Windows の既定のパス）。`docs/guide/running.md`（英日）と `--help` に書く。

### 2. 既定の設定とページを 1 つの元から作る

- **正本:** `packaging/defaults/` に置く。
  - `config.toml.in`: 短い既定の設定。パスと権限の差は `@WWW_DIR@`・`@USER@` などで埋める。
  - `www/index.html`・`www/quickstart.html`: 今の `packaging/container/www/` を移したもの。
  - コンテナの Dockerfile も、ここから作ったものをコピーする（中身は今と同じ）。
- **既定の設定の中身**（コンテナの今の設定と同じ考え方）:
  - 平文の HTTP/1.1 + h2c、`enable_secure_redirect = false`、アクセスログを標準出力（サービスの仕組みでは journald 等）に text。
  - `/` → `File` で `@WWW_DIR@`。メソッドは `GET`・`HEAD`。
  - その OS のサンドボックスを有効にする（Linux は seccomp・Landlock。BSD は F-253、macOS は F-250 で決める）。
  - TLS・リバースプロキシ・HTTP/3 の例はコメントで添え、全キーのリファレンス（`examples/config.toml`）へリンクする。
  - `quickstart.html` に、入手経路ごとの設定の場所（次の表）を足す。
- **パッケージごとの置き場所**:

  | 経路 | 設定（既定で読む場所） | ページ | 読ませ方 | リスナー | 特権 |
  |---|---|---|---|---|---|
  | コンテナ | `/etc/veil/conf.d/config.toml`（今のまま） | `/var/www/` | `CMD` の `-c`（今のまま） | `0.0.0.0:8080` | root で起動し `nonroot` に降格（今のまま） |
  | `.deb`・`.rpm`（F-251） | `/etc/veil/config.toml` | `/usr/share/veil/www/` | 既定のパス | `127.0.0.1:8080` | root で起動し `veil` に降格 |
  | Homebrew（F-250） | `$(brew --prefix)/etc/veil/config.toml` | `$(brew --prefix)/share/veil/www/` | wrapper が `VEIL_CONFIG` を設定 | `127.0.0.1:8080` | 利用者のまま |
  | winget（F-252） | `%ProgramData%\veil\config.toml` | `%ProgramData%\veil\www\` | 既定のパス（Windows） | `127.0.0.1:8080` | 利用者のまま |
  | FreeBSD ports（F-253） | `/usr/local/etc/veil/config.toml` | `/usr/local/share/veil/www/` | ビルド時の `VEIL_DEFAULT_CONFIG_PATH` | `127.0.0.1:8080` | root で起動し降格 |
  | OpenBSD ports（F-253） | `/etc/veil/config.toml` | `/usr/local/share/veil/www/` | 既定のパス | `127.0.0.1:8080` | root で起動し降格 |
  | NetBSD pkgsrc（F-253） | `/usr/pkg/etc/veil/config.toml` | `/usr/pkg/share/veil/www/` | ビルド時の `VEIL_DEFAULT_CONFIG_PATH` | `127.0.0.1:8080` | root で起動し降格 |
  | Helm（F-254） | ConfigMap → `/etc/veil/conf.d/config.toml` | イメージの `/var/www/` | イメージの `CMD` | `0.0.0.0:8080`（Pod の中） | 非 root（F-254 で決める） |
  | アーカイブ・`cargo install`・binstall（F-249） | **利用者が用意**（既定のパスに置くか `-c`） | 利用者が用意 | — | — | — |

  - コンテナ以外は、ループバックで待ち受ける。利用者が設定する前に、ネットワークへ公開しないため。
  - **設定ファイルの扱い:** どのパッケージでも、上げたときに利用者が書き換えた設定を上書きしない。
    - deb の conffile、rpm の `%config(noreplace)`、Homebrew の `etc`、ports の `@sample`・`CONF_FILES`、MSI の `NeverOverwrite`。
- **生成と検査:** `tools/release/render-defaults.sh <経路>` で生成する。
  CI で、全経路の生成物が `veil -t` を通ることを確かめる。生成物の中のパスが、その経路の表の値であることも確かめる。

### 3. アーカイブの中身（バイナリだけを使う利用者向け）

- `veil-<ver>-<target>/` の下に次のものを入れる。
  - `veil`（Windows は `veil.exe`）
  - `LICENSE-APACHE`・`LICENSE-MIT`
  - `README.md`（短い。設定ファイルは自分で用意すること・既定のパス・例の使い方・文書へのリンク）
  - `examples/config.toml`: 雛形を `./www/` 向けに埋めた例。自動では読まない。
  - `examples/www/`
- **例の使い方**（README と文書に書く）:
  ```sh
  cd veil-<ver>-<target>
  ./veil -c examples/config.toml      # 例をそのまま試す
  # 常用するなら、既定のパスに置く（または -c / VEIL_CONFIG で指定する）
  ```
- **形式と名前:** tar.gz（Windows だけ zip）。名前は今と同じ `veil-<ver>-<target>.tar.gz` にする。binstall・Homebrew が名前から組み立てる。

### 4. release.yml で成果物を作る

- `release.yml` に `binaries` ジョブを足す。`advisories` を `needs` にし、依存の検査が通る前には作らない（B-158 と同じ）。

  | 成果物 | runner | 方法 | feature |
  |---|---|---|---|
  | `x86_64`・`aarch64-unknown-linux-gnu` | ubuntu-24.04 | `build.sh`（zigbuild、glibc 2.28） | `full` |
  | `x86_64`・`aarch64-unknown-linux-musl` | ubuntu-24.04 | `build.sh` | `full` |
  | `.deb`・`.rpm`（amd64・arm64） | ubuntu-24.04 | `build.sh`（F-251 の作り） | `full` |
  | `universal2-apple-darwin` | macos-14 | ネイティブ（`lipo`。arm64 は ad-hoc 署名） | `full-macos` |
  | `x86_64`・`aarch64-pc-windows-msvc` の zip・MSI（F-252） | windows-2022・windows-11-arm | ネイティブ | `full-windows` |
  | `x86_64-unknown-freebsd` | ubuntu-24.04 | `build-cross.sh` | `full-freebsd` |
  | FreeBSD aarch64・OpenBSD・NetBSD | ubuntu-24.04 + VM（`cross-platform-actions/action` など） | VM でネイティブ | `full-freebsd`・`full-openbsd`・`full-netbsd` |

  - **BSD の VM:** 別の workflow（`release-bsd.yml`）にする。
    - NetBSD の fat LTO は KVM 無しで数時間かかるので、本体を待たせない。
    - 作った成果物は、同じ draft に後から添付する。
    - VM の上でも単体試験（`cargo test --lib`）を回す。
- **ビルド:** `cargo auditable` でビルドし、依存の一覧をバイナリに埋め込む。
- **公開の前の検査:** grype でバイナリそのものを検査し、High 以上で失敗にする。直す版の無いものも含める（B-158 の方針と同じ）。
- **整合性:**
  - `SHA256SUMS` を作り、cosign の keyless 署名（`SHA256SUMS.sigstore.json`）を付ける。
  - `actions/attest-build-provenance` で出自の証明を付ける。
  - binstall 用に minisign の署名（`.minisig`）も付ける（F-249）。
- **成果物の起動の試験:** `tools/release/binary-smoke.sh` を足す。各 runner で展開し、次のことを確かめる。
  - 設定ファイルを置かずに引数無しで起動すると、上のエラーの文で失敗する（終了コード ≠ 0）。
  - `-c examples/config.toml` で起動し、`GET /`・`/quickstart.html` が 200、h2c も 200、アクセスログが出る。
  - `VEIL_CONFIG` でも同じ。
  - `--version` がタグと一致する。
  - `.deb`・`.rpm`・MSI は、各チケットの「入れて引数無しで起動」の試験（F-251・F-252）を同じ draft の成果物で回す。
- `verify-artifacts.sh`（中身の鮮度の検査）を CI でも回す。

### 5. GitHub Release の draft（バイナリ・リリースノートの作成とアップロードまで）

- **ジョブ:** `release.yml` に `github-release` を足す。`binaries`・`image-publish`・`crates`・`pypi` が通った後に動く。
  - `gh release create <tag> --draft --verify-tag --title "Veil <ver>" --notes-file <組み立てたノート>` で draft を作る。
  - 成果物・`SHA256SUMS`・署名を `gh release upload` で付ける。
  - rc のタグも draft で作り、`--prerelease` を付ける。
  - 同じタグの draft が既にあるとき（再実行）は、ノートと成果物を差し替える。公開済みのページには触らない（失敗にする）。
  - `permissions: contents: write` はこのジョブだけに付ける。
- **リリースノートの元:**
  - 今の `docs/artifacts/`（git 管理外）は CI から読めない。そこで `docs/releases/v<ver>.md` を git で管理する。
    - リリースのブランチで書く。書き方・言語は今と同じ（日本語、v0.8.x のノートの構成）。
    - 作業メモは今までどおり `docs/artifacts/` に置き、ノートの本文だけを `docs/releases/` に置く。
  - `verify` ジョブで、タグの版の `docs/releases/v<ver>.md` があることを確かめる。無ければ、何も公開する前に失敗にする。
- **ノートの組み立て:** `tools/release/release-notes.sh <tag>` で、本文の後ろに次の節を足す（雛形は `tools/release/release-notes/`）。
  - 入れ方の一覧（コンテナ・Homebrew・apt・dnf・winget・ports・cargo・Helm。各チケットで有効になったものだけ）
  - 成果物の表（名前・target・feature・大きさ）と `SHA256SUMS` の中身
  - 検証の方法（`cosign verify-blob`・`gh attestation verify`・イメージの `cosign verify`）
  - イメージの digest、crates.io・PyPI の版へのリンク
- **公開は人:**
  - draft を確かめてから公開する（今の「ページは人が作る」運用の、作る部分だけを CI に移す）。
  - 公開（`release: published`）を契機に、下流の更新を走らせる。対象は Homebrew・apt/dnf・winget（F-250〜F-252）と、今の SBOM の添付。
- **rc の下流の試験:** draft の成果物は公開されていないので、`gh release download`（`GITHUB_TOKEN`）で取る。
  手元の HTTP サーバ（runner の中）で配り、binstall・Homebrew・winget の試験に URL を差し替えて使う（各チケット）。

### 6. 文書

- **README（英日）の「Install」:**
  - 入手経路ごとに書き直す。経路は各チケットで足していく。
  - 「バイナリ（アーカイブ・`cargo install`・`cargo binstall`）だけを使う場合は、設定ファイルとページを自分で用意する」ことを明記する。例はアーカイブの `examples/`。
- **`docs/guide/running.md`（英日）に「Configuration file」の節を足す:**
  - 設定ファイルの決まり方（`-c` → `VEIL_CONFIG` → 既定のパス）と、OS ごとの既定のパス
  - パッケージごとの置き場所の表（上の表）
  - バイナリだけの利用者の手順（例を写して `-c` で起動、既定のパスに置く）
- **`packaging/README.md`:** CI の作り方と、`docs/releases/` の書き方に合わせて直す。

## 受け入れ条件

- **設定ファイルの決まり方:**
  - `-c` > `VEIL_CONFIG` > 既定のパス の順で決まる。ファイルが無いときは、決まったパスと対処を示すエラーで止まる。
  - 単体試験と、Linux・macOS・Windows・BSD の起動の試験で確かめる。
- **パッケージの生成物:** 全経路の既定の設定の生成物が `veil -t` を通り、パスがその経路の値になっている。
- **rc のタグ:** push だけで次のものが付いた draft の pre-release ができる。
  - 表のすべての成果物・`SHA256SUMS`・署名・組み立てたリリースノート
  - 各成果物の起動の試験と grype が通る。
- **本番のタグ:** 同じものが draft で作られ、人が公開すると下流の更新が走る。
- **ノートが無いとき:** `docs/releases/v<ver>.md` が無いタグでは、何も公開されずに失敗する。

## 依存・リスク

- **windows-11-arm runner:** 使えないときは、今の `build-cross.sh`（Docker のクロスビルド）に戻す。
- **BSD の VM の action:** 外部の action は SHA で固定する。VM のイメージの OS 版をそろえる。
- **外部契約の変更:** Windows の既定のパスの変更、`VEIL_CONFIG` の追加。リリースノートに書く。
- 版の目安は v0.9.0。


## 実装の結果

- 設計どおり: `-c` → `VEIL_CONFIG` → 既定のパス（ビルド時の `VEIL_DEFAULT_CONFIG_PATH`・Windows は `%ProgramData%`）、無ければ
  終了コード 1 で置き場所を示す。`--help` は英語で既定のパスを出す。既定の設定とページは `packaging/defaults/`
  （`render-defaults.sh` が 12 の入手経路の設定を作り、`--check` で全部を `veil -t` に通す）。
- **設計から変えたこと:**
  - `cargo auditable` + バイナリの grype はやめた。`--locked` でビルドするので、依存は公開の前の `advisories`
    （全 Cargo.lock の cargo-audit・cargo-deny、警告も失敗）で同じものを検査している。
  - BSD のビルド済みのバイナリは x86_64 だけ。CI の BSD の VM（cross-platform-actions）は arm64 の runner で動かず、
    x86 の runner の arm64 の VM は TCG で 6 時間に収まらない。
  - rc のタグも draft（pre-release）にした（人の指示: リリースのページは draft で作る）。下流の試験は draft の成果物を
    runner の中の HTTP サーバで配って行う。
  - Docker Hub のコピー・minisign の署名は、タグの時点ではなく、ページを公開したとき（`release-downstream.yml`）。
- 作業で見つけて直したもの: B-160（相対パスの File の 403）、B-161（起動の失敗が終了コード 0）、B-162（小さい応答が
  キャッシュされない）、B-163（平文だけの設定で BSD・macOS のサンドボックスが掛からない）。

## 関連

- [F-246](F-246-container-try-it-defaults.md)（コンテナの既定）
- [F-237](F-237-publish-container-images.md)
- [B-158](../bugs/B-158-unfixed-image-vulns-and-advisory-gaps.md)（公開の前の検査）
- F-248〜F-254
