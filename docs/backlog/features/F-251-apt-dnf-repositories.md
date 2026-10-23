# F-251: apt・dnf のリポジトリで公開する

**状態: 実装済み（feat/distribution-tickets、2026-10-23。v0.9.0 で公開）**

## 目的

- Debian・Ubuntu では `apt install veil` で入るようにする。
- RHEL 系・Amazon Linux・Fedora では `dnf install veil` で入るようにする。
- 署名を検証できる形で配り、`apt upgrade`・`dnf upgrade` で上げられるようにする。
- 入れた直後に既定の設定で起動し、`curl http://127.0.0.1:8080/` で確かめられるようにする。
  - Debian 系: 入れると起動する（Debian の慣習）。
  - RPM 系: `sudo systemctl enable --now veil` の 1 行で起動する（Fedora・RHEL の方針では、入れただけでは起動しない）。

## 今の状態

- `packaging/scripts/build.sh` が `.deb`（amd64・arm64）と `.rpm`（x86_64・aarch64）を作る。
  - 作っているのは手元。v0.8.0 まではリリースに手で添付していた。
  - CI で作るのは F-247 から。
- **パッケージの中身:**
  - 設定: postinst が `/usr/share/veil/config.toml.default` を `/var/etc/veil/config.toml` へコピーする。
    conffile（deb）・`%config(noreplace)`（rpm）にはなっていない。
  - postinst が openssl で自己署名の証明書を作る。既定のリスナーは `127.0.0.1:443` の TLS と、`:80` のリダイレクト。
  - `/var/www/` が無ければ作って、`index.html` を置く。
- **設定の場所:** systemd のユニットは `-c /var/etc/veil/config.toml`。バイナリの既定（`/etc/veil/config.toml`）と違う。
- **試験:** `test-deb.sh`（Ubuntu のコンテナ）・`test-rpm.sh`（Amazon Linux 2023 のコンテナ）がある。
- **リポジトリ:** apt・dnf のリポジトリは無い。

## 設計

### リポジトリの置き場所と署名

- 別のリポジトリ `aofusa/veil-packages` の GitHub Pages に、静的なリポジトリとして置く。
  - URL: `https://aofusa.github.io/veil-packages/`
  - 外部のサービス（packagecloud・Cloudsmith など）に頼らず、署名の鍵と中身を自分で管理するため。
- **apt:**
  - `dists/stable/main/binary-{amd64,arm64}/` を `reprepro`（または aptly）で作る。
  - `InRelease`・`Release.gpg` を署名する。
  - 利用者の手順:

    ```sh
    sudo install -d -m 0755 /etc/apt/keyrings
    curl -fsSL https://aofusa.github.io/veil-packages/veil.gpg | sudo tee /etc/apt/keyrings/veil.gpg >/dev/null
    echo "deb [signed-by=/etc/apt/keyrings/veil.gpg] https://aofusa.github.io/veil-packages/apt stable main" \
      | sudo tee /etc/apt/sources.list.d/veil.list
    sudo apt update && sudo apt install veil
    ```

  - `curl | sh` の導入スクリプトは置かない。
- **dnf:**
  - `createrepo_c` で作り、`repomd.xml.asc` を署名する。
  - パッケージそのものも `rpm --addsign` で署名する。
  - `veil.repo` は `gpgcheck=1`・`repo_gpgcheck=1`。
- **署名の鍵:**
  - 専用の OpenPGP の鍵を作る。主鍵はオフラインに置き、CI には署名用の副鍵だけを environment `release` の secret で渡す。
  - 鍵の失効・入れ替えの手順と、指紋を文書に書く。
- **保持:** 最新の 3 版だけを置く（GitHub Pages の 1GB の上限）。古い版は GitHub Release から取ってもらう。
- **rc:** 成果物は draft にある（F-247）ので、公開のリポジトリには置かない。
  - runner の中で、draft の `.deb`・`.rpm` から同じ手順でリポジトリを作り、本番と同じ副鍵で署名する。
  - それを手元の HTTP サーバで配って試験する。

### パッケージの作り直し

- **設定の場所を `/etc/veil/` にする。** バイナリの既定・FHS に合わせる。
  - deb は `conffiles`、rpm は `%config(noreplace) /etc/veil/config.toml`。
    上げたときに、利用者の書き換えを上書きしない。
  - **前の版からの移行:** `/var/etc/veil/config.toml` が既にあるときは、その場所を使い続ける。
    - postinst が `/etc/default/veil`（deb）・`/etc/sysconfig/veil`（rpm）に `VEIL_CONFIG=/var/etc/veil/config.toml` を書く。
    - ユニットは `ExecStart=/usr/bin/veil`（`-c` 無し。新しく入れた人は既定のパス `/etc/veil/config.toml` を読む）。
      `EnvironmentFile=-...` で `VEIL_CONFIG` を渡す（F-247 の決まり方で、既定のパスより優先される）。
    - 黙って移さない。移し方は文書に書く。
- **既定の設定:** F-247 の雛形から作る。
  - リスナーは `127.0.0.1:8080` の平文、ページは `/usr/share/veil/www/`。
  - root で起動し、`veil` に降格する。seccomp・Landlock を有効にする。
- **自己署名の証明書:** postinst で作らない。既定の設定は TLS を使わないので要らない。
  - 既に作ったものは消さない。
  - `openssl` への依存を外す。
  - TLS・ACME の手順は文書にする。
- `/var/www/` を作らない。他のサーバのファイルに触れない。
- **systemd のユニット:** 今のハードニング（`ProtectSystem=strict` など）は保つ。
  - `ReadOnlyPaths` に `/etc/veil` を足す。
  - 既定の設定で、ユニットの制限と veil の seccomp・Landlock がぶつからないことを確かめる。
- **確かめる配布:**
  - `.deb`: Debian 12・13、Ubuntu 22.04・24.04。
  - `.rpm`: RHEL 9・10 互換（Rocky・Alma）、Amazon Linux 2023、Fedora の最新版。
  - バイナリは glibc 2.28 を下限にしているので、1 つのパッケージをこれらで共通に使う。
- **検査:** `lintian`（deb）・`rpmlint`（rpm）を CI で回し、エラーを 0 にする。

### workflow

- `release: published` を契機に `tools/release/publish-repos.sh` を回す。
  1. リリースの `.deb`・`.rpm` と `SHA256SUMS` を取り、cosign の署名を検証する。
  2. リポジトリに足し、索引を作り直して署名する。
  3. `veil-packages` の Pages へ push する。
- push の token は、`veil-packages` だけに書き込める fine-grained token にする（environment `release` の secret）。

### 試験

`test-deb.sh`・`test-rpm.sh` を、上の配布の行列（systemd が動くコンテナか VM）に広げる。

- **新しく入れる:**
  - リポジトリから、署名の検証込みで入れる。
    - 本番の版: 公開した後の実際の URL。
    - rc: runner の中のリポジトリ。
  - 起動したら、`/`・`/quickstart.html`・h2c が 200。アクセスログが journald に出る。
  - RPM 系は `systemctl enable --now veil` の後に確かめる。
- **上げる:** 前の版から上げる。
  - 書き換えた設定が残る。
  - `/var/etc/veil/` の利用者はそのまま動く。
- **消す:**
  - `apt remove`・`dnf remove` で止まり、設定は残る。
  - `apt purge` で設定が消える。
- **改ざん:** 署名の合わない索引・パッケージを、apt・dnf が拒否する。

### 文書

- README（英日）の「Install」と `docs/guide/running.md`（英日）に、リポジトリの足し方・設定の場所・起動・移行を書く。
- `packaging/README.md` を新しい作りに合わせる。

## 受け入れ条件

- 上の配布で、文書の手順どおりに入れると `curl http://127.0.0.1:8080/` が、入れた既定のページ（`/usr/share/veil/www/`）を返す。
  - Debian 系は入れただけ、RPM 系は `systemctl enable --now veil` の後。
- リリースを公開すると、人の手無しでリポジトリが新しい版になる。
- 署名の検証と、上げる・消す・移行の試験が通る。

## 依存・リスク

- F-247 が先。
- 人の作業が要る: `veil-packages` のリポジトリと Pages、OpenPGP の鍵、token。
- **設定の場所を変えるのは外部契約の変更。** 前の版の利用者は自動では移さない方針で、リリースノートに書く。


## 実装の結果

- deb は xz で圧縮する（Ubuntu の dpkg-deb の既定の zstd は Debian の lintian が読めない）。lintian・rpmlint はエラー 0
  （rpmlint の `non-readable /etc/veil/config.toml` だけは理由付きの例外。`packaging/rpm/veil.rpmlintrc`）。
- `tools/release/build-repos.sh`・`repos-publish.sh`: Pages の履歴に版が積もらないよう、毎回 1 commit の履歴で置き換える。
- 試験: 8 つの配布 × 2 アーキ（`test-package.sh`）、署名付きのリポジトリから入れる、改ざんの拒否（`test-repo-tamper.sh`）。

## 関連

- [F-247](F-247-distribution-foundation.md)
- `packaging/README.md`
