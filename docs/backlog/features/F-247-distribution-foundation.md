# F-247: 配布の土台（何も指定しなければ既定の設定で動く・GitHub Release の成果物を CI で作る）

**状態: 設計（feat/distribution-tickets、2026-10-23）**

パッケージマネージャでの公開（F-248〜F-253）・Helm chart（F-254）の前提になるチケット。

## 目的

- どの入手経路でも、何も指定せずに `veil` を起動すれば既定の設定で動き、すぐに動作を確かめられるようにする。
  - 公開イメージの `docker run -p 8080:8080 ghcr.io/aofusa/veil`（F-246）と同じ体験にする。
  - 対象の経路: `cargo install`、`cargo binstall`、Homebrew、apt、dnf、winget、BSD の ports、Helm。
- パッケージマネージャが取りに来る成果物を、タグの push から CI で作る。
  - 成果物は、各 OS のバイナリのアーカイブ・`.deb`・`.rpm`・チェックサム・署名。
  - 置き場所は GitHub Release。

## 今の状態（v0.8.4）

- **既定の設定ファイルの扱い:**
  - `veil` は `-c` が無いと `/etc/veil/config.toml` を読む（`src/config/loaded.rs` の `DEFAULT_CONFIG_PATH`）。
  - その場所にファイルが無いと起動できない。Windows・macOS でもこの Unix のパスを見に行く。
- **コンテナ:** 何も指定せずに動くのはコンテナだけ。
  - `packaging/container/config.toml`（`0.0.0.0:8080` の平文・内蔵のページ・アクセスログ）と `packaging/container/www/` を使う。
- **`.deb`・`.rpm`:** 既定の設定は `contrib/config/config.toml`（約 2100 行のリファレンスの写し）。
  - リスナーは `127.0.0.1:443` の TLS と `127.0.0.1:80` の https へのリダイレクト。証明書は postinst で自己署名のものを作る。
  - 設定の置き場所は `/var/etc/veil/`。バイナリの既定（`/etc/veil/`）と違う。
- **アーカイブ:** tar.gz・zip には `veil` のバイナリしか入っていない。設定・ページ・ライセンスが無い。
- **GitHub Release:** 成果物を手元でビルドし、手でアップロードしている。
  - v0.8.0 には 18 個ある。v0.8.1〜v0.8.4 にはリリースのページも成果物も無い。
  - それなのに README の「Install」は、各リリースに成果物が付いていると書いている。
  - `release.yml` が作るのは、イメージ・crates.io・PyPI だけ。
- **SBOM:** `release-sbom.yml` は、リリースのページを公開したとき（`release: published`）に SBOM を添付する。

## 設計

### 1. 設定ファイルが無いときは内蔵の既定の設定で起動する

- 条件は次の 2 つを両方満たすとき。
  - `-c` が明示されていない。
  - 既定のパスにファイルが無い。
- そのときだけ、バイナリに埋め込んだ既定の設定で起動する。
- `-c` で渡したファイルが無いときは、今と同じく起動しない（打ち間違いを黙って既定の設定で動かさない）。
- `-t` は今と同じくファイルを要る（検査するものが無ければ失敗）。
- 起動時に 1 回だけ `info!` を出す。
  - 文: `no configuration file at <path>; running with the built-in defaults on http://127.0.0.1:8080/`
  - 英語にし、チケット番号は入れない（F-242）。
- 内蔵の既定の設定:
  - `listen_cleartext = "127.0.0.1:8080"`（平文の HTTP/1.1 + h2c）、`enable_secure_redirect = false`。
    - ループバックにするのは、利用者が設定する前にネットワークへ公開しないため。
    - コンテナは今の `0.0.0.0:8080` のままにする（`-p` で公開するのが前提）。
  - アクセスログは標準出力に text で出す。
  - `/` は内蔵のページ（次の項）を返す。メソッドは `GET`・`HEAD`。
  - Linux では seccomp（filter）と Landlock を有効にする。
    - 特権の降格は設定しない（利用者の権限のまま起動する前提）。
    - Landlock の読み取りの許可は名前解決の 3 ファイルだけ。書き込みの許可は無し。
  - 他の OS の機構（capsicum・pledge/unveil・Seatbelt）は、今の既定の扱いのまま。
- `veil --print-default-config` を足す。内蔵の既定の設定を標準出力に出す。自分の設定の出発点にしてもらう。
- **Windows の既定のパス:** `%ProgramData%\veil\config.toml` にする。他の OS は `/etc/veil/config.toml` のまま。
- **触る場所:** `src/config/cli.rs`・`src/config/loaded.rs`・`src/entry.rs`。
  - 起動時の配線だけを変え、ホットパスには入らない。
  - リロード（SIGHUP）では内蔵の設定を読み直さない（内蔵の設定は変わらないので、今の設定を保つ）。

### 2. 内蔵のページを返すバックエンド

- `[route.action] type = "Builtin"` を足す。
  - `index.html` と `quickstart.html` をバイナリに埋め込んで返す。
  - それ以外のパスは 404。
  - 外部契約が増えるので、`examples/config.toml`・`docs/guide/routing.md`（英日）に書く。
- **ホットパス:** 本文は `Bytes::from_static`。`Content-Type`・`Content-Length`・`ETag` は設定のロード時に決めておく。要求ごとの確保はしない。
- **ページの置き場所:** 今の `packaging/container/www/` を `src/builtin_www/` へ移す。
  - crates.io のパッケージの `include` は `/src/**` だけなので、`src/` の下に置く。
  - コンテナの Dockerfile は移した先からコピーし、コンテナの `/var/www/` は今と同じにする。
  - 中身の正本を 1 つにして、ページが 2 つに分かれないようにする。
- `quickstart.html` には、入手経路ごとの設定の場所（次の項）を足す。

### 3. パッケージの既定の設定を 1 つの雛形から作る

- `packaging/defaults/config.toml.in` を 1 つ置き、パッケージごとにパスと権限の差を埋めて作る。

  | 経路 | 設定の場所 | ページ | リスナー | 特権 |
  |---|---|---|---|---|
  | コンテナ | `/etc/veil/conf.d/config.toml`（今のまま） | `/var/www/` | `0.0.0.0:8080` | root で起動し `nonroot` に降格（今のまま） |
  | `.deb`・`.rpm`（F-251） | `/etc/veil/config.toml` | `/usr/share/veil/www/` | `127.0.0.1:8080` | root で起動し `veil` に降格 |
  | Homebrew（F-250） | `$(brew --prefix)/etc/veil/config.toml` | `$(brew --prefix)/share/veil/www/` | `127.0.0.1:8080` | 利用者のまま |
  | BSD の ports（F-253） | FreeBSD は `/usr/local/etc/veil/`、OpenBSD は `/etc/veil/`、NetBSD は `/usr/pkg/etc/veil/` の `config.toml` | `share/veil/www/` | `127.0.0.1:8080` | root で起動し `www` に降格 |
  | `cargo install`・binstall・winget（F-249・F-252） | 無し（内蔵の既定の設定） | 内蔵（`Builtin`） | `127.0.0.1:8080` | 利用者のまま |

- 雛形は短くし、TLS・リバースプロキシ・HTTP/3 の例はコメントで添える。全キーのリファレンスは `examples/config.toml` へリンクする。
- 試験で、雛形から作ったすべての設定が `veil -t` を通ることを確かめる。

### 4. アーカイブの中身

- `veil-<ver>-<target>/` の下に次のものを入れる。
  - `veil`（Windows は `veil.exe`）
  - `config.toml`（内蔵の既定の設定と同じもの。`www/` を使う版）
  - `www/`
  - `LICENSE-APACHE`・`LICENSE-MIT`
  - `README.md`（短い。起動の仕方・設定の場所・文書へのリンク）
- 形式: tar.gz（Windows だけ zip）。名前は今と同じ `veil-<ver>-<target>.tar.gz` にし、binstall・Homebrew が名前から組み立てられるようにする。

### 5. release.yml で成果物を作る

- `release.yml` に `binaries` ジョブを足す。`advisories` を `needs` にし、依存の検査が通る前には作らない（B-158 と同じ）。

  | target | runner | 方法 | feature |
  |---|---|---|---|
  | `x86_64`・`aarch64-unknown-linux-gnu` | ubuntu-24.04 | `build.sh`（zigbuild、glibc 2.28） | `full` |
  | `x86_64`・`aarch64-unknown-linux-musl` | ubuntu-24.04 | `build.sh` | `full` |
  | `.deb`・`.rpm`（amd64・arm64） | ubuntu-24.04 | `build.sh` | `full` |
  | `universal2-apple-darwin` | macos-14 | ネイティブ（`lipo`。arm64 は ad-hoc 署名） | `full-macos` |
  | `x86_64`・`aarch64-pc-windows-msvc` | windows-2022・windows-11-arm | ネイティブ | `full-windows` |
  | `x86_64-unknown-freebsd` | ubuntu-24.04 | `build-cross.sh` | `full-freebsd` |
  | FreeBSD aarch64・OpenBSD・NetBSD | ubuntu-24.04 + VM（`cross-platform-actions/action` など） | VM でネイティブ | `full-openbsd`・`full-netbsd` など |

- **BSD の VM:** 別の workflow（`release-bsd.yml`）にする。
  - NetBSD の fat LTO のビルドは KVM 無しで数時間かかるので、本体の公開を待たせない。
  - 作った成果物は、同じリリースに後から添付する。
  - VM の上でも単体試験（`cargo test --lib`）を回す。
- **ビルド:** `cargo auditable` でビルドし、依存の一覧をバイナリに埋め込む。
- **公開の前の検査:** grype でバイナリそのものを検査し、High 以上で失敗にする。直す版の無いものも含める（B-158 の方針と同じ）。
- **整合性:**
  - `SHA256SUMS` を作り、cosign の keyless 署名（`.sigstore.json`）を付ける。
  - `actions/attest-build-provenance` で出自の証明を付ける。
  - binstall の検証用に minisign の署名も付ける（F-249）。
- **リリースのページ:**
  - rc のタグ: 成果物を付けた pre-release を自動で公開する。下流（binstall・Homebrew・winget・apt の試験）が rc で確かめられるようにする。
  - 本番のタグ: 成果物を付けた **draft** を作る。本文を書いて公開するのは人で、今の運用（手動で作る）を保つ。
  - 公開（`release: published`）を契機に、下流の更新を走らせる。対象は Homebrew・apt/dnf・winget（F-250〜F-252）と、今の SBOM の添付。
- **成果物の起動の試験:** `tools/release/binary-smoke.sh` を足す。各 runner で展開し、次のことを確かめる。
  - 引数無しで起動し、`GET http://127.0.0.1:8080/` と `/quickstart.html` が 200、h2c も 200。
  - アクセスログが標準出力に出る。
  - `--version` がタグと一致する。
  - アーカイブの `config.toml` を `-c` で渡しても同じ結果になる。
- `verify-artifacts.sh`（中身の鮮度の検査）を CI でも回す。

### 6. 文書

- README（英日）の「Install」を入手経路ごとに書き直す。経路は各チケットで足していく。
- `docs/guide/running.md`（英日）に、次のことを書く。
  - 設定ファイルが無いときの挙動
  - `--print-default-config`
  - 入手経路ごとの設定の場所
- `packaging/README.md` を CI の作り方に合わせて直す。

## 受け入れ条件

- 設定ファイルの無い環境で `veil` を起動すると、`http://127.0.0.1:8080/` が内蔵のページを返す。
  - 確かめる OS は Linux（io_uring・epoll）・macOS・Windows・FreeBSD・OpenBSD・NetBSD。
- `-c` で無いファイルを渡すと、今と同じく起動しない。
- rc のタグの push だけで、表のすべての成果物と `SHA256SUMS`・署名が pre-release に付く。
  - 各成果物の起動の試験と grype が通る。
- 単体（内蔵の設定の検査、`Builtin` の応答）・E2E（引数無しの起動）を足す。
- 性能: `Builtin` は静的配信とは別の経路。既存の経路が変わらないことを `suite_ab.sh` で確かめる。

## 依存・リスク

- **windows-11-arm runner:** 使えないときは、今の `build-cross.sh`（Docker のクロスビルド）に戻す。
- **BSD の VM の action:**
  - 外部の action は SHA で固定する。
  - VM のイメージが古い OS 版のこともあるので、版をそろえる。
- **既定のパスの変更:** Windows の既定のパスを変えるのは、外部契約の変更。リリースノートに書く。
- 版の目安は v0.9.0（バックエンドの種類と既定の挙動が増えるため）。

## 関連

- [F-246](F-246-container-try-it-defaults.md)（コンテナの既定）
- [F-237](F-237-publish-container-images.md)
- [B-158](../bugs/B-158-unfixed-image-vulns-and-advisory-gaps.md)（公開の前の検査）
- F-248〜F-254
