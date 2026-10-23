# F-250: Homebrew で公開する

**状態: 設計（feat/distribution-tickets、2026-10-23）**

## 目的

- `brew install aofusa/tap/veil` で macOS（arm64・x86_64）と Linux（x86_64・aarch64）に入れられるようにする。
- 入れた直後に次のどちらかで既定の設定で起動し、`curl http://127.0.0.1:8080/` で確かめられるようにする。
  - `veil -c $(brew --prefix)/etc/veil/config.toml`
  - `brew services start veil`
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
    - `bin.install "veil"`、`pkgshare.install "www"`。
    - 設定: アーカイブの `config.toml` の `/var/www/` を `#{pkgshare}/www/` に `inreplace` し、`(etc/"veil").install "config.toml"` で置く。
      利用者が書き換えた設定を、上げたときに上書きしない（Homebrew の `etc` の扱い）。
  - `service do`:
    - `run [opt_bin/"veil", "-c", etc/"veil/config.toml"]`、`keep_alive true`。
    - `log_path`・`error_log_path` は `var/"log/veil.log"`。
  - `caveats`: 設定の場所、`brew services start veil`、`http://127.0.0.1:8080/`。
  - `test do`:
    - 空きポートに書き換えた設定で起動し、`/` と `/quickstart.html` が 200。
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
- **rc:** tap の別の formula（`veil@rc` などにはせず、試験用の一時ブランチ）で rc の pre-release の成果物を使い、同じ試験を回す。
  本番の formula は変えない。

### homebrew-core（後で）

- 受け入れの条件（知名度・安定した版・ソースからのビルド）を満たしてから、別のチケットで出す。
- そのときの formula は `depends_on "rust" => :build`、`cmake`・`nasm` を使い、`cargo install --features full-macos` 相当でビルドする。

### 文書

- README（英日）の「Install」と `docs/guide/running.md`（英日）に、Homebrew の入れ方・設定の場所・`brew services` を足す。

## 受け入れ条件

- macOS arm64・x86_64、Linux x86_64・aarch64 で、次のことができる。
  - `brew install aofusa/tap/veil` → `brew services start veil` → `curl http://127.0.0.1:8080/` が内蔵のページを返す。
  - `brew test` と `brew audit --strict --online` が通る。
- リリースを公開すると、人の手無しで tap の formula が新しい版になる。
  - 書き換えた設定は、上げても残る。

## 依存・リスク

- F-247（CI の成果物・アーカイブの中身・署名）が先。
- 人の作業（tap のリポジトリ・token）が要る。

## 関連

- [F-247](F-247-distribution-foundation.md)
- [F-249](F-249-cargo-install-binstall.md)
