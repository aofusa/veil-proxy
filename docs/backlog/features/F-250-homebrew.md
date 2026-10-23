# F-250: Homebrew で公開する

**状態: 実装済み（feat/distribution-tickets、2026-10-23。v0.9.0 で公開）**

## 目的

- `brew install aofusa/tap/veil` で macOS（arm64・x86_64）と Linux（x86_64・aarch64）に入れられるようにする。
- 入れるときに、既定の設定とページを Homebrew の場所に置く（F-247 の表）。
  - 設定: `$(brew --prefix)/etc/veil/config.toml`
  - ページ: `$(brew --prefix)/share/veil/www/`
- 入れた直後に `veil`（引数無し）か `brew services start veil` で、その既定の設定で起動するようにする。
  `curl http://127.0.0.1:8080/` で確かめられる。
- 版を上げるのを、リリースの公開から自動にする。

## 今の状態

- tap のリポジトリ（`aofusa/homebrew-tap`）は無い（GitHub API で 404。2026-10-23）。
- 成果物:
  - macOS 用の `veil-<ver>-universal2-apple-darwin.tar.gz` は v0.8.0 まで手で作っていた。
  - Linux 用の musl の tar.gz も同じ。
  - CI で作るのは F-247 から。
- homebrew-core は、ソースからのビルドを求める。ビルド済みのバイナリを落とすだけの formula は受け付けない。
  知名度の基準もある。まずは自前の tap にする。

## 設計

### tap と formula

- **tap:** `aofusa/homebrew-tap` を作る（人が作るか、workflow の初回で作る）。
- **formula:** `Formula/veil.rb` の作り:
  - `on_macos`: universal2 の tar.gz。
  - `on_linux`: `on_intel`・`on_arm` で、x86_64・aarch64 の musl の tar.gz。
    glibc の版に依らないよう musl にする。
  - `sha256` は F-247 の `SHA256SUMS` から入れる。
  - インストール:
    - 実体は `libexec.install "veil"`。
    - ページ: `pkgshare.install "examples/www"`。
    - 設定: アーカイブの `examples/config.toml`（F-247 の雛形の例）の `./www/` を `#{pkgshare}/www/` に `inreplace` し、
      `(etc/"veil").install "examples/config.toml"` で置く。
      利用者が書き換えた設定は、上げたときに上書きしない（Homebrew の `etc` の扱い）。
    - **既定で読ませる:** `bin.write_env_script libexec/"veil", VEIL_CONFIG: etc/"veil/config.toml"`。
      - バイナリの既定のパス（`/etc/veil/config.toml`）は Homebrew の prefix と違うので、wrapper で `VEIL_CONFIG` を渡す（F-247 の決まり方）。
      - 引数無しの `veil` が Homebrew の設定を読む。`-c` を渡せば、そちらが勝つ。
  - `service do`:
    - `run [opt_bin/"veil"]`（wrapper が `VEIL_CONFIG` を設定する）、`keep_alive true`。
    - `log_path`・`error_log_path` は `var/"log/veil.log"`。
  - `caveats`: 設定の場所、`brew services start veil`、`http://127.0.0.1:8080/`。
  - `test do`:
    - 入れた既定の設定を空きポートに書き換えた写しで起動し、`/` と `/quickstart.html` が 200。
    - wrapper が既定の設定を読むことを確かめる。`veil -t` の出力に `etc/veil/config.toml` が出る。
    - `veil --version` が版と一致。
- **macOS で確かめること:**
  - 利用者の権限で動く設定にする（特権の降格は無し）。
  - Seatbelt などの macOS の機構が既定の設定で邪魔をしないこと。
  - arm64 のバイナリは署名が要る（F-247 で ad-hoc 署名）。
- **Linux（Homebrew on Linux）:** 同じ formula。io_uring を使える環境と使えない環境の両方で起動を確かめる。

### 版の更新

- `release: published`（pre-release でないもの）を契機に、`tools/release/homebrew-bump.sh` で formula を書き換える。
  - 書き換えるのは、URL・`sha256`・`version`。
  - 書き換えたものを tap へ push する。
- **token:** tap の push には、`aofusa/homebrew-tap` だけに Contents の書き込みを持つ fine-grained token を使う。
  - 置き場所は environment `release` の secret `HOMEBREW_TAP_TOKEN`。
  - veil-proxy のリポジトリの `GITHUB_TOKEN` では他のリポジトリへ push できない。
- **tap 側の CI:** macos-14・macos-15-intel・ubuntu-24.04・ubuntu-24.04-arm で次のことを回す。
  - `brew install --formula ./Formula/veil.rb`
  - `brew test veil`
  - `brew audit --strict --online veil`
  - `brew services start veil` の後の `curl`
- **rc:** tap の試験用の一時ブランチで、同じ試験を回す。
  - 成果物は draft にあるので、`gh release download` で取り、runner の中の HTTP サーバで配って URL を差し替える。
  本番の formula は変えない。

### homebrew-core（後で）

- 受け入れの条件（知名度・安定した版・ソースからのビルド）を満たしてから、別のチケットで出す。
- そのときの formula は `depends_on "rust" => :build`、`cmake`・`nasm` を使い、`cargo install --features full-macos` 相当でビルドする。

### 文書

- README（英日）の「Install」と `docs/guide/running.md`（英日）に、Homebrew の入れ方・設定の場所・`brew services` を足す。

## 受け入れ条件

- macOS arm64・x86_64、Linux x86_64・aarch64 で、次のことができる。
  - `brew install aofusa/tap/veil` → `veil`（引数無し）か `brew services start veil` → `curl http://127.0.0.1:8080/` が、置いた既定のページを返す。
  - `brew test` と `brew audit --strict --online` が通る。
- リリースを公開すると、人の手無しで tap の formula が新しい版になる。
  - 書き換えた設定は、上げても残る。

## 依存・リスク

- F-247（CI の成果物・アーカイブの中身・署名）が先。
- 人の作業（tap のリポジトリ・token）が要る。

## 関連

- [F-247](F-247-distribution-foundation.md)
- [F-249](F-249-cargo-install-binstall.md)
